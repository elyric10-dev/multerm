use crossbeam_channel::unbounded;
use eframe::egui::text::{LayoutJob, TextFormat, TextWrapping};
use eframe::egui::{
    self, Color32, CursorIcon, FontFamily, FontId, Margin, Pos2, RichText, Sense, Stroke, Vec2,
};
use rfd::FileDialog;
use serde::{Deserialize, Serialize};
use std::{
    fs,
    path::PathBuf,
    time::{Duration, Instant},
};
use termite_core::{pty::spawn_pty, session::TerminalSession, PaneId, PtyHandle};
use termite_render::color::ansi_indexed_to_rgb;
use termite_vt::cell::{Cell, CellAttrs, Color, WideKind};
use termite_vt::TerminalGrid;

#[derive(Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
enum UiTheme {
    #[default]
    Dark,
    Light,
}

#[derive(Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
enum UiStyle {
    #[default]
    Normal,
    Glass,
}

/// Upper bound on the row count in a fixed grid (default height hint for new panes).
const MAX_PANEL_GRID_ROWS: u8 = 8;

#[derive(Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
enum PanelLayoutMode {
    #[default]
    Auto,
    Fixed,
}

#[derive(Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
struct PanelLayout {
    #[serde(default)]
    mode: PanelLayoutMode,
    #[serde(default = "default_panel_grid_cols")]
    cols: u8,
    #[serde(default = "default_panel_grid_rows")]
    rows: u8,
}

fn default_panel_grid_cols() -> u8 {
    2
}

fn default_panel_grid_rows() -> u8 {
    1
}

impl Default for PanelLayout {
    fn default() -> Self {
        Self {
            mode: PanelLayoutMode::default(),
            cols: default_panel_grid_cols(),
            rows: default_panel_grid_rows(),
        }
    }
}

impl PanelLayout {
    fn sanitized(mut self) -> Self {
        self.cols = self.cols.clamp(1, MAX_WORKSPACE_COLUMNS as u8);
        self.rows = self.rows.clamp(1, MAX_PANEL_GRID_ROWS);
        self
    }

    fn column_count(self, area_width: f32) -> usize {
        match self.mode {
            PanelLayoutMode::Auto => workspace_column_count_auto(area_width),
            PanelLayoutMode::Fixed => (self.cols as usize).clamp(1, MAX_WORKSPACE_COLUMNS),
        }
    }

    /// Viewport height divided by row count (fixed layout); default height for new panes.
    fn default_pane_height_hint(self, viewport_h: f32) -> Option<f32> {
        if self.mode != PanelLayoutMode::Fixed {
            return None;
        }
        let rows = self.rows.max(1) as f32;
        let h = viewport_h / rows;
        Some(h.max(TERMINAL_MIN_HEIGHT).min(viewport_h.max(TERMINAL_MIN_HEIGHT)))
    }
}

#[derive(Clone, Copy)]
struct UiPalette {
    bg: Color32,
    panel_bg: Color32,
    border: Color32,
    text: Color32,
    muted: Color32,
    tab_active_bg: Color32,
    tab_inactive_bg: Color32,
    tab_close: Color32,
    tab_close_hover_bg: Color32,
    tab_close_active_bg: Color32,
    tab_close_hover_text: Color32,
    path_bar_bg: Color32,
    path_bar_border: Color32,
    term_bg: Color32,
    /// Default VT foreground when the emulator uses the default color.
    vt_default_fg: Color32,
    header_strip: Color32,
    popover_fill: Color32,
    path_picker_icon: Color32,
    tab_label_active: Color32,
    resize_grip_hot: Color32,
    resize_grip_cold: Color32,
    terminal_border_active: Color32,
    spawn_flash_rgb: [u8; 3],
}

impl UiTheme {
    fn palette(self) -> UiPalette {
        match self {
            UiTheme::Dark => UiPalette {
                bg: Color32::from_rgb(7, 10, 16),
                panel_bg: Color32::from_rgb(11, 17, 28),
                border: Color32::from_rgb(33, 52, 84),
                text: Color32::from_rgb(195, 213, 242),
                muted: Color32::from_rgb(118, 137, 172),
                tab_active_bg: Color32::from_rgb(30, 67, 116),
                tab_inactive_bg: Color32::from_rgb(17, 27, 43),
                tab_close: Color32::from_rgb(166, 180, 208),
                tab_close_hover_bg: Color32::from_rgb(119, 44, 56),
                tab_close_active_bg: Color32::from_rgb(146, 56, 70),
                tab_close_hover_text: Color32::from_rgb(255, 241, 246),
                path_bar_bg: Color32::from_rgb(13, 22, 36),
                path_bar_border: Color32::from_rgb(29, 48, 76),
                term_bg: Color32::from_rgb(5, 8, 12),
                vt_default_fg: Color32::from_rgb(212, 212, 216),
                header_strip: Color32::from_rgb(9, 13, 21),
                popover_fill: Color32::from_rgb(8, 14, 24),
                path_picker_icon: Color32::from_rgb(52, 217, 113),
                tab_label_active: Color32::WHITE,
                resize_grip_hot: Color32::from_rgb(160, 196, 245),
                resize_grip_cold: Color32::from_rgb(96, 130, 184),
                terminal_border_active: Color32::from_rgb(88, 142, 222),
                spawn_flash_rgb: [120, 180, 255],
            },
            UiTheme::Light => UiPalette {
                bg: Color32::from_rgb(236, 239, 244),
                panel_bg: Color32::from_rgb(224, 229, 237),
                border: Color32::from_rgb(150, 165, 188),
                text: Color32::from_rgb(28, 34, 48),
                muted: Color32::from_rgb(88, 98, 118),
                tab_active_bg: Color32::from_rgb(64, 120, 200),
                tab_inactive_bg: Color32::from_rgb(206, 214, 228),
                tab_close: Color32::from_rgb(90, 100, 120),
                tab_close_hover_bg: Color32::from_rgb(200, 90, 100),
                tab_close_active_bg: Color32::from_rgb(180, 60, 72),
                tab_close_hover_text: Color32::from_rgb(255, 245, 247),
                path_bar_bg: Color32::from_rgb(248, 249, 252),
                path_bar_border: Color32::from_rgb(180, 190, 208),
                term_bg: Color32::from_rgb(252, 252, 254),
                vt_default_fg: Color32::from_rgb(36, 40, 52),
                header_strip: Color32::from_rgb(226, 231, 240),
                popover_fill: Color32::from_rgb(248, 250, 252),
                path_picker_icon: Color32::from_rgb(34, 150, 90),
                tab_label_active: Color32::WHITE,
                resize_grip_hot: Color32::from_rgb(70, 120, 200),
                resize_grip_cold: Color32::from_rgb(140, 160, 190),
                terminal_border_active: Color32::from_rgb(50, 110, 200),
                spawn_flash_rgb: [70, 130, 220],
            },
        }
    }
}

impl UiPalette {
    fn with_style(self, style: UiStyle) -> Self {
        match style {
            UiStyle::Normal => self,
            UiStyle::Glass => {
                let mut p = self;
                p.bg = color_with_alpha(p.bg, 236);
                p.panel_bg = color_with_alpha(p.panel_bg, 212);
                p.header_strip = color_with_alpha(p.header_strip, 206);
                p.popover_fill = color_with_alpha(p.popover_fill, 222);
                p.path_bar_bg = color_with_alpha(p.path_bar_bg, 214);
                p.tab_inactive_bg = color_with_alpha(p.tab_inactive_bg, 198);
                p.term_bg = color_with_alpha(p.term_bg, 224);
                p
            }
        }
    }
}

fn color_with_alpha(c: Color32, a: u8) -> Color32 {
    Color32::from_rgba_unmultiplied(c.r(), c.g(), c.b(), a)
}

fn apply_egui_visuals(ctx: &egui::Context, theme: UiTheme, p: UiPalette) {
    let mut visuals = match theme {
        UiTheme::Dark => egui::Visuals::dark(),
        UiTheme::Light => egui::Visuals::light(),
    };
    visuals.override_text_color = Some(p.text);
    visuals.panel_fill = p.bg;
    visuals.window_fill = p.bg;
    visuals.widgets.noninteractive.bg_fill = p.panel_bg;
    visuals.widgets.noninteractive.bg_stroke = Stroke::new(1.0, p.border);
    ctx.set_visuals(visuals);
}
const CELL_W: f32 = 9.0;
const CELL_H: f32 = 18.0;
const GRID_SPACING: f32 = 10.0;
/// Vertical gap between stacked terminals (horizontal column gutters use [`GRID_SPACING`]).
const STACK_GAP_Y: f32 = 6.0;
/// Upper bound on workspace columns from width (see [`workspace_column_count_auto`]).
const MAX_WORKSPACE_COLUMNS: usize = 12;
const TERMINAL_MIN_WIDTH: f32 = 260.0;
const TERMINAL_MIN_HEIGHT: f32 = 180.0;
const RESIZE_HANDLE_SIZE: f32 = 14.0;
const RESIZE_EDGE_THICKNESS: f32 = 6.0;
const RESIZE_CORNER_HOTSPOT: f32 = 20.0;
/// How close (px) an edge must be to a guide before it snaps — smaller = weaker magnet.
const RESIZE_SNAP_DISTANCE: f32 = 2.0;
const RESIZE_SNAP_OVERLAP_MIN: f32 = 0.0;
/// Pixels past the pane outer edge where the BR diagonal grip lives (outside the border).
const CORNER_GRIP_OUTSET: f32 = 2.0;
/// Extra radius around BR corner to show resize cursor early.
const BR_CURSOR_HOVER_RADIUS: f32 = 14.0;

/// Local-space `(left, right, top, bottom)` of the spawn-preview dashed frame, matching
/// [`TermiteUi::paint_spawn_flash`] when `area_w` / `area_h` match that call.
fn spawn_flash_stripe_local_edges(
    until: Option<Instant>,
    local_pos: Option<Pos2>,
    area_w: f32,
    area_h: f32,
    layout: PanelLayout,
) -> Option<(f32, f32, f32, f32)> {
    let until = until?;
    if Instant::now() >= until {
        return None;
    }
    let lp = local_pos?;
    if !(area_w > 0.0 && area_h > 0.0) {
        return None;
    }
    let col = pick_column_at_x(lp.x, area_w, layout);
    let w = column_stripe_width(area_w, layout);
    let left = column_band_left(area_w, col, layout);
    let right = left + w;
    let top = 0.0;
    let bottom = area_h.max(1.0);
    Some((left, right, top, bottom))
}

fn merge_layout_guide_resize_snaps(
    column_area_w: f32,
    flash: Option<(f32, f32, f32, f32)>,
    right_dragged: bool,
    left_dragged: bool,
    top_dragged: bool,
    bottom_dragged: bool,
    pane_x0: f32,
    pane_x1: f32,
    pane_y0: f32,
    pane_y1: f32,
    best_x: &mut Option<(f32, f32, bool, usize)>,
    best_y: &mut Option<(f32, f32, bool, usize)>,
    layout: PanelLayout,
) {
    // Default column layout: vertical guides (slot / gutter boundaries), always available.
    for sx in column_grid_vertical_snap_xs(column_area_w, layout) {
        if right_dragged {
            let d = (pane_x1 - sx).abs();
            if d <= RESIZE_SNAP_DISTANCE && best_x.is_none_or(|(bd, _, _, _)| d < bd) {
                *best_x = Some((d, sx, true, usize::MAX));
            }
        }
        if left_dragged {
            let d = (pane_x0 - sx).abs();
            if d <= RESIZE_SNAP_DISTANCE && best_x.is_none_or(|(bd, _, _, _)| d < bd) {
                *best_x = Some((d, sx, false, usize::MAX));
            }
        }
    }

    if let Some((fl, fr, ft, fb)) = flash {
        let y_flash = (pane_y1.min(fb) - pane_y0.max(ft)).max(0.0);
        if y_flash >= RESIZE_SNAP_OVERLAP_MIN {
            if right_dragged {
                for sx in [fl, fr] {
                    let d = (pane_x1 - sx).abs();
                    if d <= RESIZE_SNAP_DISTANCE && best_x.is_none_or(|(bd, _, _, _)| d < bd) {
                        *best_x = Some((d, sx, true, usize::MAX));
                    }
                }
            }
            if left_dragged {
                for sx in [fl, fr] {
                    let d = (pane_x0 - sx).abs();
                    if d <= RESIZE_SNAP_DISTANCE && best_x.is_none_or(|(bd, _, _, _)| d < bd) {
                        *best_x = Some((d, sx, false, usize::MAX));
                    }
                }
            }
        }
        let x_flash = (pane_x1.min(fr) - pane_x0.max(fl)).max(0.0);
        if x_flash >= RESIZE_SNAP_OVERLAP_MIN {
            if bottom_dragged {
                for sy in [ft, fb] {
                    let d = (pane_y1 - sy).abs();
                    if d <= RESIZE_SNAP_DISTANCE && best_y.is_none_or(|(bd, _, _, _)| d < bd) {
                        *best_y = Some((d, sy, true, usize::MAX));
                    }
                }
            }
            if top_dragged {
                for sy in [ft, fb] {
                    let d = (pane_y0 - sy).abs();
                    if d <= RESIZE_SNAP_DISTANCE && best_y.is_none_or(|(bd, _, _, _)| d < bd) {
                        *best_y = Some((d, sy, false, usize::MAX));
                    }
                }
            }
        }
    }
}

fn merge_layout_guide_drag_snaps(
    column_area_w: f32,
    flash: Option<(f32, f32, f32, f32)>,
    pos: Pos2,
    w: f32,
    h: f32,
    max_x: f32,
    max_y: f32,
    best_x: &mut Option<(f32, f32, f32)>,
    best_y: &mut Option<(f32, f32, f32)>,
    layout: PanelLayout,
) {
    for snap_x in column_grid_vertical_snap_xs(column_area_w, layout) {
        let d_left = (pos.x - snap_x).abs();
        if d_left <= RESIZE_SNAP_DISTANCE {
            let nx = snap_x.clamp(0.0, max_x);
            if best_x.is_none_or(|(bd, _, _)| d_left < bd) {
                *best_x = Some((d_left, nx, snap_x));
            }
        }
        let d_right = ((pos.x + w) - snap_x).abs();
        if d_right <= RESIZE_SNAP_DISTANCE {
            let nx = (snap_x - w).clamp(0.0, max_x);
            if best_x.is_none_or(|(bd, _, _)| d_right < bd) {
                *best_x = Some((d_right, nx, snap_x));
            }
        }
    }

    if let Some((fl, fr, ft, fb)) = flash {
        let pane_y0 = pos.y;
        let pane_y1 = pos.y + h;
        let pane_x0 = pos.x;
        let pane_x1 = pos.x + w;
        let y_flash = (pane_y1.min(fb) - pane_y0.max(ft)).max(0.0);
        if y_flash >= RESIZE_SNAP_OVERLAP_MIN {
            for snap_x in [fl, fr] {
                let d_left = (pos.x - snap_x).abs();
                if d_left <= RESIZE_SNAP_DISTANCE {
                    let nx = snap_x.clamp(0.0, max_x);
                    if best_x.is_none_or(|(bd, _, _)| d_left < bd) {
                        *best_x = Some((d_left, nx, snap_x));
                    }
                }
                let d_right = ((pos.x + w) - snap_x).abs();
                if d_right <= RESIZE_SNAP_DISTANCE {
                    let nx = (snap_x - w).clamp(0.0, max_x);
                    if best_x.is_none_or(|(bd, _, _)| d_right < bd) {
                        *best_x = Some((d_right, nx, snap_x));
                    }
                }
            }
        }
        let x_flash = (pane_x1.min(fr) - pane_x0.max(fl)).max(0.0);
        if x_flash >= RESIZE_SNAP_OVERLAP_MIN {
            for snap_y in [ft, fb] {
                let d_top = (pos.y - snap_y).abs();
                if d_top <= RESIZE_SNAP_DISTANCE {
                    let ny = snap_y.clamp(0.0, max_y);
                    if best_y.is_none_or(|(bd, _, _)| d_top < bd) {
                        *best_y = Some((d_top, ny, snap_y));
                    }
                }
                let d_bottom = ((pos.y + h) - snap_y).abs();
                if d_bottom <= RESIZE_SNAP_DISTANCE {
                    let ny = (snap_y - h).clamp(0.0, max_y);
                    if best_y.is_none_or(|(bd, _, _)| d_bottom < bd) {
                        *best_y = Some((d_bottom, ny, snap_y));
                    }
                }
            }
        }
    }
}

fn main() -> eframe::Result<()> {
    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_inner_size([1440.0, 860.0])
            .with_min_inner_size([1100.0, 700.0])
            .with_title("Termite UI"),
        ..Default::default()
    };

    eframe::run_native(
        "Termite UI",
        options,
        Box::new(|_cc| Ok(Box::<TermiteUi>::default())),
    )
}

struct TerminalPane {
    id: u64,
    title: String,
    session: TerminalSession,
    pty: PtyHandle,
    desired_size: Vec2,
    position: Option<Pos2>,
}

#[derive(Serialize, Deserialize, Clone)]
struct TerminalPaneState {
    title: String,
    width: f32,
    height: f32,
    x: Option<f32>,
    y: Option<f32>,
}

struct TermiteUi {
    ui_theme: UiTheme,
    ui_style: UiStyle,
    panel_layout: PanelLayout,
    /// When set, terminal widths track the column stripe width and x snaps to column bands.
    sync_terminals_to_columns: bool,
    selected_workspace: usize,
    workspaces: Vec<WorkspaceTab>,
    next_workspace_index: usize,
    workspace_runtime: Vec<WorkspaceRuntime>,
    next_terminal_id: u64,
    /// Workspace width and scrollable content height (used for placement / clamping).
    terminal_area_size: Vec2,
    /// Visible workspace height inside the central panel (caps default new pane height).
    terminal_workspace_viewport: Vec2,
    editing_workspace_idx: Option<usize>,
    editing_workspace_input: String,
    color_history: Vec<[u8; 4]>,
    color_hex_target_idx: Option<usize>,
    color_hex_input: String,
    color_picker_target_idx: Option<usize>,
    color_picker_draft: Color32,
    color_picker_original_rgba: Option<[u8; 4]>,
    color_picker_rendered_this_frame: bool,
    editing_working_dir: bool,
    working_dir_input: String,
    pending_terminal_spawn_pos: Option<Pos2>,
    pending_context_terminal: Option<usize>,
    pending_spawn_flash_pos: Option<Pos2>,
    pending_spawn_flash_until: Option<Instant>,
}

#[derive(Default)]
struct WorkspaceRuntime {
    terminals: Vec<TerminalPane>,
    active_terminal: Option<usize>,
}

struct WorkspaceTab {
    title: String,
    badge: Option<u8>,
    color_rgba: Option<[u8; 4]>,
    working_dir: String,
}

#[derive(Serialize, Deserialize)]
struct WorkspaceState {
    #[serde(default)]
    ui_theme: UiTheme,
    #[serde(default)]
    ui_style: UiStyle,
    #[serde(default)]
    panel_layout: PanelLayout,
    #[serde(default)]
    sync_terminals_to_columns: bool,
    selected_workspace: usize,
    workspaces: Vec<WorkspaceTabState>,
    next_workspace_index: usize,
    #[serde(default)]
    color_history: Vec<[u8; 4]>,
}

#[derive(Serialize, Deserialize, Clone)]
struct WorkspaceTabState {
    title: String,
    badge: Option<u8>,
    color_rgba: Option<[u8; 4]>,
    #[serde(default)]
    working_dir: Option<String>,
    #[serde(default)]
    terminal_sessions: Vec<TerminalPaneState>,
    #[serde(default)]
    active_terminal: Option<usize>,
}

impl Default for TermiteUi {
    fn default() -> Self {
        if let Some(state) = load_workspace_state() {
            let ui_theme = state.ui_theme;
            let ui_style = state.ui_style;
            let panel_layout = state.panel_layout.sanitized();
            let sync_terminals_to_columns = state.sync_terminals_to_columns;
            let theme_palette = ui_theme.palette().with_style(ui_style);
            let tab_states = state.workspaces.clone();
            let workspaces: Vec<WorkspaceTab> = tab_states
                .iter()
                .map(|tab| WorkspaceTab {
                    title: tab.title.clone(),
                    badge: tab.badge,
                    color_rgba: tab.color_rgba,
                    working_dir: tab.working_dir.clone().unwrap_or_else(default_working_dir),
                })
                .collect();
            let next_workspace_index = compute_next_workspace_index(&workspaces);
            let runtime_count = workspaces.len();
            let mut app = Self {
                ui_theme,
                ui_style,
                panel_layout,
                sync_terminals_to_columns,
                selected_workspace: state
                    .selected_workspace
                    .min(workspaces.len().saturating_sub(1)),
                workspaces,
                next_workspace_index,
                workspace_runtime: (0..runtime_count)
                    .map(|_| WorkspaceRuntime::default())
                    .collect(),
                next_terminal_id: 1,
                terminal_area_size: Vec2::new(1200.0, 700.0),
                terminal_workspace_viewport: Vec2::new(1200.0, 700.0),
                editing_workspace_idx: None,
                editing_workspace_input: String::new(),
                color_history: state.color_history,
                color_hex_target_idx: None,
                color_hex_input: String::new(),
                color_picker_target_idx: None,
                color_picker_draft: theme_palette.tab_active_bg,
                color_picker_original_rgba: None,
                color_picker_rendered_this_frame: false,
                editing_working_dir: false,
                working_dir_input: String::new(),
                pending_terminal_spawn_pos: None,
                pending_context_terminal: None,
                pending_spawn_flash_pos: None,
                pending_spawn_flash_until: None,
            };
            // Restore terminal sessions per workspace from persisted metadata.
            for idx in 0..app.workspaces.len() {
                if let Some(saved_tab) = tab_states.get(idx) {
                    let working_dir = app.workspaces[idx].working_dir.clone();
                    let mut restored: Vec<TerminalPane> = Vec::new();
                    for pane_state in &saved_tab.terminal_sessions {
                        let mut pane = spawn_terminal_pane(
                            pane_state.title.clone(),
                            app.next_terminal_id,
                            &working_dir,
                        );
                        app.next_terminal_id += 1;
                        pane.desired_size = Vec2::new(
                            pane_state.width.max(TERMINAL_MIN_WIDTH),
                            pane_state.height.max(TERMINAL_MIN_HEIGHT),
                        );
                        pane.position = match (pane_state.x, pane_state.y) {
                            (Some(x), Some(y)) => Some(Pos2::new(x.max(0.0), y.max(0.0))),
                            _ => None,
                        };
                        restored.push(pane);
                    }
                    if let Some(runtime) = app.workspace_runtime.get_mut(idx) {
                        runtime.terminals = restored;
                        runtime.active_terminal = saved_tab.active_terminal.and_then(|i| {
                            if i < runtime.terminals.len() {
                                Some(i)
                            } else {
                                None
                            }
                        });
                    }
                }
            }
            return app;
        }

        Self {
            ui_theme: UiTheme::default(),
            ui_style: UiStyle::default(),
            panel_layout: PanelLayout::default(),
            sync_terminals_to_columns: false,
            selected_workspace: 2,
            workspaces: vec![
                WorkspaceTab {
                    title: "Workspace 1".to_string(),
                    badge: Some(2),
                    color_rgba: None,
                    working_dir: default_working_dir(),
                },
                WorkspaceTab {
                    title: "Workspace 2".to_string(),
                    badge: None,
                    color_rgba: None,
                    working_dir: default_working_dir(),
                },
                WorkspaceTab {
                    title: "Workspace 3".to_string(),
                    badge: Some(11),
                    color_rgba: None,
                    working_dir: default_working_dir(),
                },
                WorkspaceTab {
                    title: "Workspace 4".to_string(),
                    badge: Some(5),
                    color_rgba: None,
                    working_dir: default_working_dir(),
                },
                WorkspaceTab {
                    title: "Workspace 5".to_string(),
                    badge: None,
                    color_rgba: None,
                    working_dir: default_working_dir(),
                },
            ],
            next_workspace_index: 6,
            workspace_runtime: (0..5).map(|_| WorkspaceRuntime::default()).collect(),
            next_terminal_id: 1,
            terminal_area_size: Vec2::new(1200.0, 700.0),
            terminal_workspace_viewport: Vec2::new(1200.0, 700.0),
            editing_workspace_idx: None,
            editing_workspace_input: String::new(),
            color_history: Vec::new(),
            color_hex_target_idx: None,
            color_hex_input: String::new(),
            color_picker_target_idx: None,
            color_picker_draft: UiTheme::default()
                .palette()
                .with_style(UiStyle::default())
                .tab_active_bg,
            color_picker_original_rgba: None,
            color_picker_rendered_this_frame: false,
            editing_working_dir: false,
            working_dir_input: String::new(),
            pending_terminal_spawn_pos: None,
            pending_context_terminal: None,
            pending_spawn_flash_pos: None,
            pending_spawn_flash_until: None,
        }
    }
}

fn new_terminal_context_menu(
    ui: &mut egui::Ui,
    app: &mut TermiteUi,
    target_terminal: Option<usize>,
) {
    if ui.button("New Terminal").clicked() {
        let spawn_pos = app.pending_terminal_spawn_pos.take();
        let anchor_terminal = app.pending_context_terminal.take().or(target_terminal);
        app.add_terminal(spawn_pos, anchor_terminal);
        app.pending_context_terminal = None;
        ui.close();
    }
    if ui.button("New Claude Code").clicked() {
        let spawn_pos = app.pending_terminal_spawn_pos.take();
        let anchor_terminal = app.pending_context_terminal.take().or(target_terminal);
        app.add_terminal(spawn_pos, anchor_terminal);
        app.launch_cli_tool(None, "claude");
        app.pending_context_terminal = None;
        ui.close();
    }
    if ui.button("New Codex").clicked() {
        let spawn_pos = app.pending_terminal_spawn_pos.take();
        let anchor_terminal = app.pending_context_terminal.take().or(target_terminal);
        app.add_terminal(spawn_pos, anchor_terminal);
        app.launch_cli_tool(None, "codex");
        app.pending_context_terminal = None;
        ui.close();
    }
}

impl eframe::App for TermiteUi {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        // Keep refreshing so PTY output appears live without explicit wakeups.
        self.ensure_workspace_runtime_slots();
        ctx.request_repaint_after(Duration::from_millis(16));
        self.drain_terminals();
        self.handle_keyboard_input(ctx);
        self.color_picker_rendered_this_frame = false;

        let p = self.ui_theme.palette().with_style(self.ui_style);
        apply_egui_visuals(ctx, self.ui_theme, p);

        egui::TopBottomPanel::top("workspace_tabs")
            .resizable(false)
            .exact_height(70.0)
            .frame(
                egui::Frame::default()
                    .fill(p.header_strip)
                    .stroke(Stroke::NONE),
            )
            .show(ctx, |ui| {
                header_tabs(ui, self, p);
                ui.add_space(5.0);
                directory_path_bar(ui, self, p);
            });

        egui::CentralPanel::default().show(ctx, |ui| {
            egui::Frame::default()
                .fill(p.bg)
                .inner_margin(Margin::same(10))
                .show(ui, |ui| {
                    self.terminal_workspace_viewport = ui.available_size();
                    let area_rect = ui.max_rect();
                    let viewport = Vec2::new(area_rect.width(), area_rect.height());

                    let Some(runtime_ref) = self.active_workspace_runtime() else {
                        let response = ui.interact(
                            area_rect,
                            ui.id().with("terminal_context_area"),
                            Sense::click(),
                        );
                        if response.secondary_clicked() {
                            if let Some(pointer_pos) = response.interact_pointer_pos() {
                                let local_pos = pointer_pos - area_rect.min.to_vec2();
                                self.pending_terminal_spawn_pos = Some(local_pos);
                                self.pending_context_terminal = self.terminal_index_at_local_pos(
                                    Pos2::new(local_pos.x, local_pos.y),
                                );
                                self.trigger_spawn_flash(local_pos);
                            } else {
                                self.pending_context_terminal = None;
                            }
                        }
                        let target_terminal = self.pending_context_terminal;
                        response.context_menu(|ui| {
                            new_terminal_context_menu(ui, self, target_terminal)
                        });
                        self.paint_spawn_flash(ui, area_rect.min, viewport, p);
                        let hint = ui.label(
                            RichText::new("Create a workspace tab to start terminals.")
                                .size(13.0)
                                .color(p.muted),
                        );
                        if hint.secondary_clicked() {
                            self.pending_context_terminal = None;
                            if let Some(pointer_pos) = hint.interact_pointer_pos() {
                                let local_pos = pointer_pos - area_rect.min.to_vec2();
                                self.pending_terminal_spawn_pos = Some(local_pos);
                                self.trigger_spawn_flash(local_pos);
                            }
                        }
                        hint.context_menu(|ui| new_terminal_context_menu(ui, self, None));
                        self.terminal_area_size = viewport;
                        return;
                    };

                    let content_h = workspace_content_height(&runtime_ref.terminals, viewport.y);

                    egui::ScrollArea::vertical()
                        .id_salt(("termite_ws_scroll", self.selected_workspace))
                        .auto_shrink([false, false])
                        .show(ui, |ui| {
                            ui.set_min_size(Vec2::new(viewport.x, content_h));
                            self.terminal_area_size = Vec2::new(viewport.x, content_h);

                            let content_origin = ui.min_rect().min;
                            let content_rect = ui.max_rect();
                            let workspace_col_w = content_rect.width();
                            let scroll_bg = ui.interact(
                                content_rect,
                                ui.id().with(("ws_scroll_bg", self.selected_workspace)),
                                Sense::click(),
                            );
                            if scroll_bg.secondary_clicked() {
                                if let Some(pointer_pos) = scroll_bg.interact_pointer_pos() {
                                    let local_pos = Pos2::new(
                                        pointer_pos.x - content_origin.x,
                                        pointer_pos.y - content_origin.y,
                                    );
                                    self.pending_terminal_spawn_pos = Some(local_pos);
                                    self.pending_context_terminal =
                                        self.terminal_index_at_local_pos(local_pos);
                                    self.trigger_spawn_flash(local_pos);
                                } else {
                                    self.pending_context_terminal = None;
                                }
                            }
                            let target_terminal = self.pending_context_terminal;
                            scroll_bg.context_menu(|ui| {
                                new_terminal_context_menu(ui, self, target_terminal)
                            });
                            self.paint_spawn_flash(
                                ui,
                                content_origin,
                                Vec2::new(workspace_col_w, viewport.y),
                                p,
                            );

                            let spawn_flash_edges = spawn_flash_stripe_local_edges(
                                self.pending_spawn_flash_until,
                                self.pending_spawn_flash_pos,
                                workspace_col_w,
                                viewport.y,
                                self.panel_layout,
                            );
                            let layout = self.panel_layout;
                            let sync_terminals_to_columns = self.sync_terminals_to_columns;

                            let Some(runtime) = self.active_workspace_runtime_mut() else {
                                return;
                            };

                            if runtime.terminals.is_empty() {
                                let hint = ui.label(
                                    RichText::new("Right-click and choose \"New Terminal\".")
                                        .size(13.0)
                                        .color(p.muted),
                                );
                                if hint.secondary_clicked() {
                                    self.pending_context_terminal = None;
                                    if let Some(pointer_pos) = hint.interact_pointer_pos() {
                                        let local_pos = Pos2::new(
                                            pointer_pos.x - content_origin.x,
                                            pointer_pos.y - content_origin.y,
                                        );
                                        self.pending_terminal_spawn_pos = Some(local_pos);
                                        self.trigger_spawn_flash(local_pos);
                                    }
                                }
                                hint.context_menu(|ui| new_terminal_context_menu(ui, self, None));
                                return;
                            }

                            let mut close_idx: Option<usize> = None;
                            let mut clicked_on_pane = false;
                            let available_width = content_rect.width();
                            let content_height = content_h;
                            let (_, _, n_cols) = column_slot_geometry(available_width, layout);
                            let slot_w = column_stripe_width(available_width, layout);

                            if sync_terminals_to_columns
                                && !ui.input(|i| i.pointer.any_down())
                            {
                                reflow_panes_to_column_starts(
                                    &mut runtime.terminals,
                                    available_width,
                                    layout,
                                );
                            }

                            // Next Y for stacking in each column (must use the pane *above*'s height, not row index).
                            let mut column_floor_y = vec![0.0_f32; n_cols];
                            for pane in runtime.terminals.iter() {
                                if let Some(pos) = pane.position {
                                    let col = pick_column_at_x(
                                        pos.x + pane.desired_size.x * 0.5,
                                        available_width,
                                        layout,
                                    );
                                    if col < n_cols {
                                        let bottom = pos.y + pane.desired_size.y + STACK_GAP_Y;
                                        column_floor_y[col] = column_floor_y[col].max(bottom);
                                    }
                                }
                            }

                            for idx in 0..runtime.terminals.len() {
                                let (left_group, right_group) = runtime.terminals.split_at_mut(idx);
                                let Some((pane, right_group)) = right_group.split_first_mut()
                                else {
                                    continue;
                                };

                                if pane.position.is_none() {
                                    pane.desired_size.x = slot_w
                                        .max(TERMINAL_MIN_WIDTH)
                                        .min(available_width.max(1.0));
                                    let mut h =
                                        pane.desired_size.y.max(content_height.max(260.0));
                                    if let Some(rh) = layout.default_pane_height_hint(viewport.y) {
                                        h = h.max(rh);
                                    }
                                    pane.desired_size.y = h;
                                    let col = idx % n_cols.max(1);
                                    let x = column_band_left(available_width, col, layout);
                                    let y = column_floor_y[col];
                                    pane.position = Some(Pos2::new(x, y));
                                    column_floor_y[col] = y + pane.desired_size.y + STACK_GAP_Y;
                                }

                                let mut pos = pane.position.unwrap_or(Pos2::ZERO);
                                let max_x = (available_width - pane.desired_size.x).max(0.0);
                                let max_y = (content_height - pane.desired_size.y).max(0.0);
                                pos.x = pos.x.clamp(0.0, max_x);
                                pos.y = pos.y.clamp(0.0, max_y);
                                pane.position = Some(pos);

                                let pane_rect = egui::Rect::from_min_size(
                                    content_origin + pos.to_vec2(),
                                    pane.desired_size,
                                );
                                let header_rect = egui::Rect::from_min_size(
                                    pane_rect.min,
                                    Vec2::new(pane_rect.width(), 24.0),
                                );
                                let drag_response = ui.interact(
                                    header_rect,
                                    ui.id().with(("pane_drag", pane.id)),
                                    Sense::click_and_drag(),
                                );
                                if drag_response.dragged() {
                                    ui.output_mut(|o| o.cursor_icon = CursorIcon::Grabbing);
                                } else if drag_response.hovered() {
                                    ui.output_mut(|o| o.cursor_icon = CursorIcon::Grab);
                                }
                                if drag_response.dragged() {
                                    let delta = ui.input(|i| i.pointer.delta());
                                    pos.x = (pos.x + delta.x).clamp(0.0, max_x);
                                    pos.y = (pos.y + delta.y).clamp(0.0, max_y);

                                    // Snap dragged pane edges to other terminals (same thresholds as resize).
                                    let w = pane.desired_size.x;
                                    let h = pane.desired_size.y;
                                    let pane_y0 = pos.y;
                                    let pane_y1 = pos.y + h;
                                    let pane_x0 = pos.x;
                                    let pane_x1 = pos.x + w;

                                    let mut best_x_snap: Option<(f32, f32, f32)> = None;
                                    let mut best_y_snap: Option<(f32, f32, f32)> = None;

                                    let mut inspect_drag_neighbor = |other: &TerminalPane| {
                                        let other_pos = other.position.unwrap_or(Pos2::ZERO);
                                        let other_left = other_pos.x;
                                        let other_right = other_pos.x + other.desired_size.x;
                                        let other_top = other_pos.y;
                                        let other_bottom = other_pos.y + other.desired_size.y;

                                        let y_overlap = (pane_y1.min(other_bottom)
                                            - pane_y0.max(other_top))
                                        .max(0.0);
                                        let x_overlap = (pane_x1.min(other_right)
                                            - pane_x0.max(other_left))
                                        .max(0.0);

                                        if y_overlap >= RESIZE_SNAP_OVERLAP_MIN {
                                            for snap_x in [other_left, other_right] {
                                                let d_left = (pos.x - snap_x).abs();
                                                if d_left <= RESIZE_SNAP_DISTANCE {
                                                    let nx = snap_x.clamp(0.0, max_x);
                                                    if best_x_snap
                                                        .is_none_or(|(best, _, _)| d_left < best)
                                                    {
                                                        best_x_snap = Some((d_left, nx, snap_x));
                                                    }
                                                }
                                                let d_right = ((pos.x + w) - snap_x).abs();
                                                if d_right <= RESIZE_SNAP_DISTANCE {
                                                    let nx = (snap_x - w).clamp(0.0, max_x);
                                                    if best_x_snap
                                                        .is_none_or(|(best, _, _)| d_right < best)
                                                    {
                                                        best_x_snap = Some((d_right, nx, snap_x));
                                                    }
                                                }
                                            }
                                        }
                                        if x_overlap >= RESIZE_SNAP_OVERLAP_MIN {
                                            for snap_y in [other_top, other_bottom] {
                                                let d_top = (pos.y - snap_y).abs();
                                                if d_top <= RESIZE_SNAP_DISTANCE {
                                                    let ny = snap_y.clamp(0.0, max_y);
                                                    if best_y_snap
                                                        .is_none_or(|(best, _, _)| d_top < best)
                                                    {
                                                        best_y_snap = Some((d_top, ny, snap_y));
                                                    }
                                                }
                                                let d_bottom = ((pos.y + h) - snap_y).abs();
                                                if d_bottom <= RESIZE_SNAP_DISTANCE {
                                                    let ny = (snap_y - h).clamp(0.0, max_y);
                                                    if best_y_snap
                                                        .is_none_or(|(best, _, _)| d_bottom < best)
                                                    {
                                                        best_y_snap = Some((d_bottom, ny, snap_y));
                                                    }
                                                }
                                            }
                                        }
                                    };

                                    for other in left_group.iter() {
                                        inspect_drag_neighbor(other);
                                    }
                                    for other in right_group.iter() {
                                        inspect_drag_neighbor(other);
                                    }

                                    merge_layout_guide_drag_snaps(
                                        workspace_col_w,
                                        spawn_flash_edges,
                                        pos,
                                        w,
                                        h,
                                        max_x,
                                        max_y,
                                        &mut best_x_snap,
                                        &mut best_y_snap,
                                        layout,
                                    );

                                    let mut drag_guide_x: Option<f32> = None;
                                    let mut drag_guide_y: Option<f32> = None;
                                    if let Some((_, nx, gx)) = best_x_snap {
                                        pos.x = nx;
                                        drag_guide_x = Some(gx);
                                    }
                                    if let Some((_, ny, gy)) = best_y_snap {
                                        pos.y = ny;
                                        drag_guide_y = Some(gy);
                                    }
                                    pane.position = Some(pos);

                                    if let Some(gx) = drag_guide_x {
                                        let x = content_origin.x + gx;
                                        ui.painter().line_segment(
                                            [
                                                Pos2::new(x, content_origin.y),
                                                Pos2::new(x, content_origin.y + content_height),
                                            ],
                                            Stroke::new(1.3, p.resize_grip_hot),
                                        );
                                    }
                                    if let Some(gy) = drag_guide_y {
                                        let y = content_origin.y + gy;
                                        ui.painter().line_segment(
                                            [
                                                Pos2::new(content_origin.x, y),
                                                Pos2::new(content_origin.x + available_width, y),
                                            ],
                                            Stroke::new(1.3, p.resize_grip_hot),
                                        );
                                    }
                                }

                                let left_rect = egui::Rect::from_min_max(
                                    pane_rect.min,
                                    Pos2::new(
                                        pane_rect.min.x + RESIZE_EDGE_THICKNESS,
                                        pane_rect.max.y,
                                    ),
                                );
                                let right_rect = egui::Rect::from_min_max(
                                    Pos2::new(
                                        pane_rect.max.x - RESIZE_EDGE_THICKNESS,
                                        pane_rect.min.y,
                                    ),
                                    pane_rect.max,
                                );
                                let top_rect = egui::Rect::from_min_max(
                                    pane_rect.min,
                                    Pos2::new(
                                        pane_rect.max.x,
                                        pane_rect.min.y + RESIZE_EDGE_THICKNESS,
                                    ),
                                );
                                let bottom_rect = egui::Rect::from_min_max(
                                    Pos2::new(
                                        pane_rect.min.x,
                                        pane_rect.max.y - RESIZE_EDGE_THICKNESS,
                                    ),
                                    pane_rect.max,
                                );

                                let tl_rect = egui::Rect::from_min_size(
                                    pane_rect.min,
                                    Vec2::splat(RESIZE_HANDLE_SIZE),
                                );
                                let tr_rect = egui::Rect::from_min_size(
                                    Pos2::new(
                                        pane_rect.max.x - RESIZE_HANDLE_SIZE,
                                        pane_rect.min.y,
                                    ),
                                    Vec2::splat(RESIZE_HANDLE_SIZE),
                                );
                                let bl_rect = egui::Rect::from_min_size(
                                    Pos2::new(
                                        pane_rect.min.x,
                                        pane_rect.max.y - RESIZE_HANDLE_SIZE,
                                    ),
                                    Vec2::splat(RESIZE_HANDLE_SIZE),
                                );
                                // Bottom-right grip: interaction + visuals sit outside the pane border.
                                let br_grip_rect = egui::Rect::from_min_size(
                                    pane_rect.right_bottom()
                                        + Vec2::new(CORNER_GRIP_OUTSET, CORNER_GRIP_OUTSET),
                                    Vec2::splat(RESIZE_CORNER_HOTSPOT),
                                );

                                let resize_left = ui.interact(
                                    left_rect,
                                    ui.id().with(("pane_resize_left", pane.id)),
                                    Sense::click_and_drag(),
                                );
                                let resize_right = ui.interact(
                                    right_rect,
                                    ui.id().with(("pane_resize_right", pane.id)),
                                    Sense::click_and_drag(),
                                );
                                let resize_top = ui.interact(
                                    top_rect,
                                    ui.id().with(("pane_resize_top", pane.id)),
                                    Sense::click_and_drag(),
                                );
                                let resize_bottom = ui.interact(
                                    bottom_rect,
                                    ui.id().with(("pane_resize_bottom", pane.id)),
                                    Sense::click_and_drag(),
                                );
                                let resize_tl = ui.interact(
                                    tl_rect,
                                    ui.id().with(("pane_resize_tl", pane.id)),
                                    Sense::click_and_drag(),
                                );
                                let resize_tr = ui.interact(
                                    tr_rect,
                                    ui.id().with(("pane_resize_tr", pane.id)),
                                    Sense::click_and_drag(),
                                );
                                let resize_bl = ui.interact(
                                    bl_rect,
                                    ui.id().with(("pane_resize_bl", pane.id)),
                                    Sense::click_and_drag(),
                                );
                                let resize_br = ui.interact(
                                    br_grip_rect,
                                    ui.id().with(("pane_resize_br", pane.id)),
                                    Sense::click_and_drag(),
                                );

                                let left_active = resize_left.hovered() || resize_left.dragged();
                                let right_active = resize_right.hovered() || resize_right.dragged();
                                let top_active = resize_top.hovered() || resize_top.dragged();
                                let bottom_active =
                                    resize_bottom.hovered() || resize_bottom.dragged();
                                let tl_active = resize_tl.hovered() || resize_tl.dragged();
                                let tr_active = resize_tr.hovered() || resize_tr.dragged();
                                let bl_active = resize_bl.hovered() || resize_bl.dragged();
                                let br_active = resize_br.hovered() || resize_br.dragged();
                                let near_br_corner = ui.input(|i| {
                                    i.pointer.hover_pos().is_some_and(|p| {
                                        p.x >= pane_rect.max.x - BR_CURSOR_HOVER_RADIUS
                                            && p.y >= pane_rect.max.y - BR_CURSOR_HOVER_RADIUS
                                            && p.x <= pane_rect.max.x + BR_CURSOR_HOVER_RADIUS
                                            && p.y <= pane_rect.max.y + BR_CURSOR_HOVER_RADIUS
                                    })
                                });

                                // Cursor priority: corners first, then edges.
                                if tl_active || br_active || near_br_corner {
                                    ui.output_mut(|o| o.cursor_icon = CursorIcon::ResizeNwSe);
                                } else if tr_active || bl_active {
                                    ui.output_mut(|o| o.cursor_icon = CursorIcon::ResizeNeSw);
                                } else if left_active || right_active {
                                    ui.output_mut(|o| o.cursor_icon = CursorIcon::ResizeHorizontal);
                                } else if top_active || bottom_active {
                                    ui.output_mut(|o| o.cursor_icon = CursorIcon::ResizeVertical);
                                }

                                let any_resize_dragged = resize_left.dragged()
                                    || resize_right.dragged()
                                    || resize_top.dragged()
                                    || resize_bottom.dragged()
                                    || resize_tl.dragged()
                                    || resize_tr.dragged()
                                    || resize_bl.dragged()
                                    || resize_br.dragged();
                                if any_resize_dragged {
                                    let delta = ui.input(|i| i.pointer.delta());
                                    let mut new_x = pos.x;
                                    let mut new_y = pos.y;
                                    let mut new_w = pane.desired_size.x;
                                    let mut new_h = pane.desired_size.y;
                                    let mut snap_guide_x: Option<f32> = None;
                                    let mut snap_guide_y: Option<f32> = None;

                                    let left_dragged = resize_left.dragged()
                                        || resize_tl.dragged()
                                        || resize_bl.dragged();
                                    let right_dragged = resize_right.dragged()
                                        || resize_tr.dragged()
                                        || resize_br.dragged();
                                    let top_dragged = resize_top.dragged()
                                        || resize_tl.dragged()
                                        || resize_tr.dragged();
                                    let bottom_dragged = resize_bottom.dragged()
                                        || resize_bl.dragged()
                                        || resize_br.dragged();

                                    if left_dragged {
                                        let right = pos.x + pane.desired_size.x;
                                        let max_left = (right - TERMINAL_MIN_WIDTH).max(0.0);
                                        let proposed_left = (pos.x + delta.x).clamp(0.0, max_left);
                                        new_x = proposed_left;
                                        new_w = right - proposed_left;
                                    }
                                    if right_dragged {
                                        let max_w =
                                            (available_width - new_x).max(TERMINAL_MIN_WIDTH);
                                        new_w = (new_w + delta.x).clamp(TERMINAL_MIN_WIDTH, max_w);
                                    }
                                    if top_dragged {
                                        let bottom = pos.y + pane.desired_size.y;
                                        let max_top = (bottom - TERMINAL_MIN_HEIGHT).max(0.0);
                                        let proposed_top = (pos.y + delta.y).clamp(0.0, max_top);
                                        new_y = proposed_top;
                                        new_h = bottom - proposed_top;
                                    }
                                    if bottom_dragged {
                                        let max_h =
                                            (content_height - new_y).max(TERMINAL_MIN_HEIGHT);
                                        new_h = (new_h + delta.y).clamp(TERMINAL_MIN_HEIGHT, max_h);
                                    }

                                    let mut best_x_snap: Option<(f32, f32, bool, usize)> = None;
                                    let mut best_y_snap: Option<(f32, f32, bool, usize)> = None;
                                    let pane_y0 = new_y;
                                    let pane_y1 = new_y + new_h;
                                    let pane_x0 = new_x;
                                    let pane_x1 = new_x + new_w;

                                    let mut inspect_other =
                                        |other_idx: usize, other: &TerminalPane| {
                                            let other_pos = other.position.unwrap_or(Pos2::ZERO);
                                            let other_left = other_pos.x;
                                            let other_right = other_pos.x + other.desired_size.x;
                                            let other_top = other_pos.y;
                                            let other_bottom = other_pos.y + other.desired_size.y;

                                            let y_overlap = (pane_y1.min(other_bottom)
                                                - pane_y0.max(other_top))
                                            .max(0.0);
                                            let x_overlap = (pane_x1.min(other_right)
                                                - pane_x0.max(other_left))
                                            .max(0.0);

                                            if right_dragged && y_overlap >= RESIZE_SNAP_OVERLAP_MIN
                                            {
                                                for snap_x in [other_left, other_right] {
                                                    let dist = (pane_x1 - snap_x).abs();
                                                    if dist <= RESIZE_SNAP_DISTANCE {
                                                        if best_x_snap.is_none_or(
                                                            |(best, _, _, _)| dist < best,
                                                        ) {
                                                            best_x_snap = Some((
                                                                dist, snap_x, true, other_idx,
                                                            ));
                                                        }
                                                    }
                                                }
                                            }
                                            if left_dragged && y_overlap >= RESIZE_SNAP_OVERLAP_MIN
                                            {
                                                for snap_x in [other_left, other_right] {
                                                    let dist = (pane_x0 - snap_x).abs();
                                                    if dist <= RESIZE_SNAP_DISTANCE {
                                                        if best_x_snap.is_none_or(
                                                            |(best, _, _, _)| dist < best,
                                                        ) {
                                                            best_x_snap = Some((
                                                                dist, snap_x, false, other_idx,
                                                            ));
                                                        }
                                                    }
                                                }
                                            }
                                            if bottom_dragged
                                                && x_overlap >= RESIZE_SNAP_OVERLAP_MIN
                                            {
                                                for snap_y in [other_top, other_bottom] {
                                                    let dist = (pane_y1 - snap_y).abs();
                                                    if dist <= RESIZE_SNAP_DISTANCE {
                                                        if best_y_snap.is_none_or(
                                                            |(best, _, _, _)| dist < best,
                                                        ) {
                                                            best_y_snap = Some((
                                                                dist, snap_y, true, other_idx,
                                                            ));
                                                        }
                                                    }
                                                }
                                            }
                                            if top_dragged && x_overlap >= RESIZE_SNAP_OVERLAP_MIN {
                                                for snap_y in [other_top, other_bottom] {
                                                    let dist = (pane_y0 - snap_y).abs();
                                                    if dist <= RESIZE_SNAP_DISTANCE {
                                                        if best_y_snap.is_none_or(
                                                            |(best, _, _, _)| dist < best,
                                                        ) {
                                                            best_y_snap = Some((
                                                                dist, snap_y, false, other_idx,
                                                            ));
                                                        }
                                                    }
                                                }
                                            }
                                        };

                                    for (other_idx, other) in left_group.iter().enumerate() {
                                        inspect_other(other_idx, other);
                                    }
                                    for (off, other) in right_group.iter().enumerate() {
                                        inspect_other(idx + 1 + off, other);
                                    }

                                    merge_layout_guide_resize_snaps(
                                        workspace_col_w,
                                        spawn_flash_edges,
                                        right_dragged,
                                        left_dragged,
                                        top_dragged,
                                        bottom_dragged,
                                        pane_x0,
                                        pane_x1,
                                        pane_y0,
                                        pane_y1,
                                        &mut best_x_snap,
                                        &mut best_y_snap,
                                        layout,
                                    );

                                    if let Some((_, snap_x, snapped_right_edge, _other_idx)) =
                                        best_x_snap
                                    {
                                        if snapped_right_edge && right_dragged {
                                            new_w = (snap_x - new_x).clamp(
                                                TERMINAL_MIN_WIDTH,
                                                (available_width - new_x).max(TERMINAL_MIN_WIDTH),
                                            );
                                            snap_guide_x = Some(snap_x);
                                        } else if !snapped_right_edge && left_dragged {
                                            let right = new_x + new_w;
                                            new_x = (snap_x)
                                                .clamp(0.0, (right - TERMINAL_MIN_WIDTH).max(0.0));
                                            new_w = right - new_x;
                                            snap_guide_x = Some(snap_x);
                                        }
                                    }

                                    if let Some((_, snap_y, snapped_bottom_edge, _other_idx)) =
                                        best_y_snap
                                    {
                                        if snapped_bottom_edge && bottom_dragged {
                                            new_h = (snap_y - new_y).clamp(
                                                TERMINAL_MIN_HEIGHT,
                                                (content_height - new_y).max(TERMINAL_MIN_HEIGHT),
                                            );
                                            snap_guide_y = Some(snap_y);
                                        } else if !snapped_bottom_edge && top_dragged {
                                            let bottom = new_y + new_h;
                                            new_y = (snap_y).clamp(
                                                0.0,
                                                (bottom - TERMINAL_MIN_HEIGHT).max(0.0),
                                            );
                                            new_h = bottom - new_y;
                                            snap_guide_y = Some(snap_y);
                                        }
                                    }

                                    pane.desired_size.x = new_w;
                                    pane.desired_size.y = new_h;
                                    pos.x = new_x;
                                    pos.y = new_y;
                                    pane.position = Some(pos);

                                    if let Some(guide_x) = snap_guide_x {
                                        let x = content_origin.x + guide_x;
                                        ui.painter().line_segment(
                                            [
                                                Pos2::new(x, content_origin.y),
                                                Pos2::new(x, content_origin.y + content_height),
                                            ],
                                            Stroke::new(1.3, p.resize_grip_hot),
                                        );
                                    }
                                    if let Some(guide_y) = snap_guide_y {
                                        let y = content_origin.y + guide_y;
                                        ui.painter().line_segment(
                                            [
                                                Pos2::new(content_origin.x, y),
                                                Pos2::new(content_origin.x + available_width, y),
                                            ],
                                            Stroke::new(1.3, p.resize_grip_hot),
                                        );
                                    }
                                }

                                let is_active = runtime.active_terminal == Some(idx);
                                let border = if is_active {
                                    p.terminal_border_active
                                } else {
                                    p.border
                                };

                                let pane_response = ui.allocate_rect(pane_rect, Sense::click());
                                ui.scope_builder(
                                    egui::UiBuilder::new().max_rect(pane_rect),
                                    |ui| {
                                        egui::Frame::default()
                                            .fill(p.term_bg)
                                            .stroke(Stroke::new(1.0, border))
                                            .inner_margin(Margin::same(6))
                                            .show(ui, |ui| {
                                                ui.horizontal(|ui| {
                                                    ui.add(
                                                        egui::Label::new(
                                                            RichText::new(&pane.title)
                                                                .family(FontFamily::Monospace)
                                                                .size(12.0)
                                                                .color(p.text),
                                                        )
                                                        .selectable(false)
                                                        .sense(Sense::hover()),
                                                    );
                                                    ui.with_layout(
                                                        egui::Layout::right_to_left(
                                                            egui::Align::Center,
                                                        ),
                                                        |ui| {
                                                            if ui.small_button("x").clicked() {
                                                                close_idx = Some(idx);
                                                            }
                                                        },
                                                    );
                                                });
                                                ui.separator();
                                                let terminal_height =
                                                    ui.available_height().max(120.0);
                                                let terminal_size =
                                                    Vec2::new(pane.desired_size.x, terminal_height);
                                                resize_terminal_for_size(pane, terminal_size);
                                                render_terminal_grid(
                                                    ui,
                                                    pane.id,
                                                    pane.session.parser.grid(),
                                                    p,
                                                );
                                            });
                                    },
                                );

                                if pane_response.clicked() {
                                    runtime.active_terminal = Some(idx);
                                    clicked_on_pane = true;
                                    ui.ctx().memory_mut(|mem| mem.stop_text_input());
                                }
                                if pane_response.secondary_clicked() {
                                    runtime.active_terminal = Some(idx);
                                    clicked_on_pane = true;
                                    ui.ctx().memory_mut(|mem| mem.stop_text_input());
                                }
                                if near_br_corner || resize_br.hovered() || resize_br.dragged() {
                                    paint_br_resize_line(
                                        ui.painter(),
                                        br_grip_rect,
                                        resize_br.hovered() || resize_br.dragged(),
                                        p,
                                    );
                                }
                            }

                            if scroll_bg.clicked() && !clicked_on_pane {
                                runtime.active_terminal = None;
                            }

                            if let Some(idx) = close_idx {
                                let was_active = runtime.active_terminal == Some(idx);
                                runtime.terminals.remove(idx);
                                runtime.active_terminal = if runtime.terminals.is_empty() {
                                    None
                                } else if was_active {
                                    None
                                } else {
                                    runtime.active_terminal.and_then(|a| {
                                        if a > idx {
                                            Some(a - 1)
                                        } else {
                                            Some(a)
                                        }
                                    })
                                };
                            }
                        });
                });
        });

        self.cleanup_stale_color_picker();
    }

    fn on_exit(&mut self, _gl: Option<&eframe::glow::Context>) {
        save_workspace_state(self);
    }
}

impl TermiteUi {
    fn add_terminal(&mut self, spawn_pos: Option<Pos2>, anchor_terminal: Option<usize>) {
        self.ensure_workspace_runtime_slots();
        let layout = self.panel_layout;
        let selected_workspace = self.selected_workspace;
        let area_size = self.terminal_area_size;
        let viewport_h = self.terminal_workspace_viewport.y.max(TERMINAL_MIN_HEIGHT);
        let next_terminal_id = self.next_terminal_id;
        let fallback_anchor = anchor_terminal.or_else(|| {
            self.active_workspace_runtime()
                .and_then(|r| r.active_terminal)
        });
        let content_bounds = self
            .active_workspace_runtime()
            .map(|r| {
                workspace_content_height(&r.terminals, viewport_h)
                    .max(area_size.y)
                    .max(viewport_h)
            })
            .unwrap_or(viewport_h);
        let area_for_placement = Vec2::new(area_size.x, content_bounds);
        let working_dir = self
            .workspaces
            .get(selected_workspace)
            .map(|w| w.working_dir.clone())
            .unwrap_or_else(default_working_dir);
        let workspace_number = selected_workspace + 1;

        {
            let Some(runtime) = self.active_workspace_runtime_mut() else {
                return;
            };
            let next_title_index =
                next_available_terminal_number(&runtime.terminals, workspace_number);
            let terminal_title = format!(
                "Workspace {} - Terminal {}",
                workspace_number, next_title_index
            );
            let mut pane = spawn_terminal_pane(terminal_title, next_terminal_id, &working_dir);
            let stripe_w = column_stripe_width(area_for_placement.x, layout);
            pane.desired_size.x = if spawn_pos.is_some() {
                stripe_w
            } else {
                stripe_w
                    .max(TERMINAL_MIN_WIDTH)
                    .min(area_for_placement.x.max(TERMINAL_MIN_WIDTH))
            };
            let mut default_h = pane
                .desired_size
                .y
                .min(viewport_h.max(TERMINAL_MIN_HEIGHT))
                .max(TERMINAL_MIN_HEIGHT);
            if let Some(h) = layout.default_pane_height_hint(viewport_h) {
                default_h = default_h.max(h);
            }
            pane.desired_size.y = default_h;
            let position = if let Some(cursor_pos) = spawn_pos {
                let col = pick_column_at_x(cursor_pos.x, area_for_placement.x, layout);
                let default_h = pane.desired_size.y;
                let first_top = min_y_topmost_in_column(
                    &runtime.terminals,
                    area_for_placement.x,
                    col,
                    layout,
                );
                let cap = match first_top {
                    Some(y) if y > STACK_GAP_Y => y - STACK_GAP_Y,
                    Some(_) => content_bounds,
                    None => content_bounds,
                };
                let preferred_max_h = default_h.min(cap.max(1.0));
                let (pos, spawn_size) = find_spawn_column_no_overlap(
                    &runtime.terminals,
                    area_for_placement,
                    col,
                    preferred_max_h,
                    default_h,
                    layout,
                );
                pane.desired_size = spawn_size;
                pos
            } else if let Some(anchor_idx) = fallback_anchor {
                if let Some(anchor) = runtime.terminals.get(anchor_idx) {
                    let pos = anchor.position.unwrap_or(Pos2::ZERO);
                    let col = pick_column_at_x(
                        pos.x + anchor.desired_size.x * 0.5,
                        area_for_placement.x,
                        layout,
                    );
                    let preferred_y =
                        (pos.y + 24.0).min((area_for_placement.y - pane.desired_size.y).max(0.0));
                    find_non_overlapping_position_in_column(
                        &runtime.terminals,
                        area_for_placement,
                        pane.desired_size,
                        col,
                        preferred_y,
                        layout,
                    )
                } else {
                    find_non_overlapping_position(
                        &runtime.terminals,
                        area_for_placement,
                        pane.desired_size,
                    )
                }
            } else {
                find_non_overlapping_position(
                    &runtime.terminals,
                    area_for_placement,
                    pane.desired_size,
                )
            };
            pane.position = Some(position);
            runtime.terminals.push(pane);
            runtime.active_terminal = Some(runtime.terminals.len() - 1);
        }
        self.next_terminal_id = next_terminal_id + 1;
    }

    fn drain_terminals(&mut self) {
        for runtime in &mut self.workspace_runtime {
            for pane in &mut runtime.terminals {
                let _ = pane.session.drain_and_parse();
            }
        }
    }

    fn handle_keyboard_input(&mut self, ctx: &egui::Context) {
        self.ensure_workspace_runtime_slots();
        let Some(runtime) = self.active_workspace_runtime_mut() else {
            return;
        };
        let Some(active_idx) = runtime.active_terminal else {
            return;
        };
        if active_idx >= runtime.terminals.len() {
            return;
        }

        let mut shortcut_new_terminal = false;
        let events = ctx.input(|i| i.events.clone());
        for event in events {
            match event {
                egui::Event::Text(text) => {
                    if !text.is_empty() {
                        let _ = runtime.terminals[active_idx].pty.write_all(text.as_bytes());
                    }
                }
                egui::Event::Key {
                    key,
                    pressed,
                    modifiers,
                    ..
                } if pressed => {
                    if key == egui::Key::N && modifiers.shift && modifiers.command {
                        shortcut_new_terminal = true;
                        continue;
                    }
                    if let Some(bytes) = key_to_ansi_bytes(key, modifiers.shift) {
                        let _ = runtime.terminals[active_idx].pty.write_all(&bytes);
                    }
                }
                _ => {}
            }
        }

        if shortcut_new_terminal {
            self.add_terminal(None, None);
        }
    }

    fn launch_cli_tool(&mut self, target_terminal: Option<usize>, command: &str) {
        let pending_target = self.pending_context_terminal.take();
        self.ensure_workspace_runtime_slots();
        if self
            .active_workspace_runtime()
            .is_some_and(|runtime| runtime.terminals.is_empty())
        {
            self.add_terminal(None, None);
        }

        let Some(runtime) = self.active_workspace_runtime_mut() else {
            return;
        };
        if runtime.terminals.is_empty() {
            return;
        }

        let idx = pending_target
            .or(target_terminal)
            .or(runtime.active_terminal)
            .unwrap_or(0)
            .min(runtime.terminals.len().saturating_sub(1));
        runtime.active_terminal = Some(idx);
        let _ = runtime.terminals[idx]
            .pty
            .write_all(format!("{command}\n").as_bytes());
    }
}

fn spawn_terminal_pane(title: String, next_terminal_id: u64, working_dir: &str) -> TerminalPane {
    let (tx, rx) = unbounded::<Vec<u8>>();
    let shell = std::env::var("SHELL").unwrap_or_else(|_| "/bin/zsh".to_string());
    let wake_up: Box<dyn Fn() + Send + 'static> = Box::new(|| {});
    let pty =
        spawn_pty(&shell, 24, 80, tx, wake_up, Some(working_dir)).expect("spawn terminal pty");

    TerminalPane {
        id: next_terminal_id,
        title,
        session: TerminalSession::new(PaneId::new(), 24, 80, rx),
        pty,
        desired_size: Vec2::new(520.0, 280.0),
        position: None,
    }
}

fn resize_terminal_for_size(pane: &mut TerminalPane, size: Vec2) {
    let cols = (size.x / CELL_W).max(1.0) as usize;
    let rows = (size.y / CELL_H).max(1.0) as usize;
    pane.session.parser.resize(rows, cols);
    let _ = pane.pty.resize(rows as u16, cols as u16);
}

fn paint_br_resize_line(painter: &egui::Painter, grip_rect: egui::Rect, hot: bool, p: UiPalette) {
    let color = if hot {
        p.resize_grip_hot
    } else {
        p.resize_grip_cold
    };
    let stroke = Stroke::new(1.0, color);
    let corner = grip_rect.left_top() - Vec2::splat(CORNER_GRIP_OUTSET);
    // Single short diagonal mark at the corner.
    let start = corner + Vec2::new(-2.0, -2.0);
    let end = corner + Vec2::new(8.0, 8.0);
    painter.line_segment([start, end], stroke);
}

fn paint_broken_border(painter: &egui::Painter, rect: egui::Rect, color: Color32) {
    let stroke = Stroke::new(1.2, color);
    let dash = 10.0;
    let gap = 7.0;

    let mut x = rect.left();
    while x < rect.right() {
        let x2 = (x + dash).min(rect.right());
        painter.line_segment(
            [Pos2::new(x, rect.top()), Pos2::new(x2, rect.top())],
            stroke,
        );
        painter.line_segment(
            [Pos2::new(x, rect.bottom()), Pos2::new(x2, rect.bottom())],
            stroke,
        );
        x += dash + gap;
    }

    let mut y = rect.top();
    while y < rect.bottom() {
        let y2 = (y + dash).min(rect.bottom());
        painter.line_segment(
            [Pos2::new(rect.left(), y), Pos2::new(rect.left(), y2)],
            stroke,
        );
        painter.line_segment(
            [Pos2::new(rect.right(), y), Pos2::new(rect.right(), y2)],
            stroke,
        );
        y += dash + gap;
    }
}

fn color32_from_vt_color(c: Color, is_fg: bool, vt_default_fg: Color32) -> Color32 {
    match c {
        Color::Default => {
            if is_fg {
                vt_default_fg
            } else {
                Color32::TRANSPARENT
            }
        }
        Color::Indexed(i) => {
            let [r, g, b] = ansi_indexed_to_rgb(i);
            Color32::from_rgb(r, g, b)
        }
        Color::Rgb(r, g, b) => Color32::from_rgb(r, g, b),
    }
}

fn cell_text_format(
    cell: &Cell,
    font_id: FontId,
    term_bg: Color32,
    vt_default_fg: Color32,
) -> TextFormat {
    let reverse = cell.attrs.contains(CellAttrs::REVERSE);
    let (fg_src, bg_src) = if reverse {
        (cell.bg, cell.fg)
    } else {
        (cell.fg, cell.bg)
    };

    let mut fg = color32_from_vt_color(fg_src, true, vt_default_fg);
    let bg_opaque = color32_from_vt_color(bg_src, false, vt_default_fg);
    let background = if matches!(bg_src, Color::Default) {
        Color32::TRANSPARENT
    } else {
        bg_opaque
    };

    if cell.attrs.contains(CellAttrs::INVISIBLE) {
        fg = if background == Color32::TRANSPARENT {
            term_bg
        } else {
            bg_opaque
        };
    }

    let stroke_fg = fg;
    let underline = if cell.attrs.contains(CellAttrs::UNDERLINE) {
        Stroke::new(1.0, stroke_fg)
    } else {
        Stroke::NONE
    };
    let strikethrough = if cell.attrs.contains(CellAttrs::STRIKETHROUGH) {
        Stroke::new(1.0, stroke_fg)
    } else {
        Stroke::NONE
    };

    TextFormat {
        font_id,
        color: fg,
        background,
        italics: cell.attrs.contains(CellAttrs::ITALIC),
        underline,
        strikethrough,
        ..Default::default()
    }
}

fn formats_match(a: &TextFormat, b: &TextFormat) -> bool {
    a.font_id == b.font_id
        && a.color == b.color
        && a.background == b.background
        && a.italics == b.italics
        && a.underline == b.underline
        && a.strikethrough == b.strikethrough
}

/// Last column index + 1 to render on `row`, trimming only trailing unstyled spaces.
fn row_render_end(grid: &TerminalGrid, row: usize) -> usize {
    let mut end = grid.cols;
    while end > 0 {
        let cell = grid.cell(row, end - 1);
        if cell.wide == WideKind::Trailing {
            end -= 1;
            continue;
        }
        if cell.ch != ' ' {
            break;
        }
        if cell.fg != Color::Default || cell.bg != Color::Default || !cell.attrs.is_empty() {
            break;
        }
        end -= 1;
    }
    end
}

fn render_terminal_grid(ui: &mut egui::Ui, pane_id: u64, grid: &TerminalGrid, p: UiPalette) {
    let font_id = FontId::monospace(12.0);
    let newline_fmt = TextFormat {
        font_id: font_id.clone(),
        color: p.vt_default_fg,
        background: Color32::TRANSPARENT,
        ..Default::default()
    };

    let mut job = LayoutJob::default();
    for row in 0..grid.rows {
        let trim_end = row_render_end(grid, row);
        let mut col = 0;
        while col < trim_end {
            let cell = grid.cell(row, col);
            if cell.wide == WideKind::Trailing {
                col += 1;
                continue;
            }

            let fmt = cell_text_format(cell, font_id.clone(), p.term_bg, p.vt_default_fg);
            let mut chunk = String::new();
            chunk.push(cell.ch);

            let mut next = col + 1;
            if cell.wide == WideKind::Leading {
                next = col + 2;
            }

            while next < trim_end {
                let c2 = grid.cell(row, next);
                if c2.wide == WideKind::Trailing {
                    next += 1;
                    continue;
                }
                let fmt2 = cell_text_format(c2, font_id.clone(), p.term_bg, p.vt_default_fg);
                if !formats_match(&fmt, &fmt2) {
                    break;
                }
                chunk.push(c2.ch);
                if c2.wide == WideKind::Leading {
                    next += 2;
                } else {
                    next += 1;
                }
            }

            job.append(&chunk, 0.0, fmt);
            col = next;
        }
        if row + 1 < grid.rows {
            job.append("\n", 0.0, newline_fmt.clone());
        }
    }

    egui::ScrollArea::vertical()
        .id_salt(("term-scroll", pane_id))
        .auto_shrink([false, false])
        .show(ui, |ui| {
            job.wrap = TextWrapping::wrap_at_width(ui.available_width());
            ui.add(
                egui::Label::new(job)
                    .selectable(false)
                    .sense(Sense::hover())
                    .wrap_mode(egui::TextWrapMode::Wrap),
            );
        });
}

fn key_to_ansi_bytes(key: egui::Key, shift: bool) -> Option<Vec<u8>> {
    match key {
        egui::Key::Enter => Some(vec![b'\r']),
        egui::Key::Tab => Some(if shift {
            vec![0x1b, b'[', b'Z']
        } else {
            vec![b'\t']
        }),
        egui::Key::Backspace => Some(vec![0x7f]),
        egui::Key::Escape => Some(vec![0x1b]),
        egui::Key::ArrowUp => Some(b"\x1b[A".to_vec()),
        egui::Key::ArrowDown => Some(b"\x1b[B".to_vec()),
        egui::Key::ArrowRight => Some(b"\x1b[C".to_vec()),
        egui::Key::ArrowLeft => Some(b"\x1b[D".to_vec()),
        egui::Key::Home => Some(b"\x1b[H".to_vec()),
        egui::Key::End => Some(b"\x1b[F".to_vec()),
        egui::Key::Delete => Some(b"\x1b[3~".to_vec()),
        egui::Key::PageUp => Some(b"\x1b[5~".to_vec()),
        egui::Key::PageDown => Some(b"\x1b[6~".to_vec()),
        _ => None,
    }
}

fn header_tabs(ui: &mut egui::Ui, app: &mut TermiteUi, p: UiPalette) {
    let mut changed = false;
    let mut close_idx: Option<usize> = None;
    ui.horizontal(|ui| {
        ui.horizontal(|ui| {
            for idx in 0..app.workspaces.len() {
                let active = idx == app.selected_workspace;
                let fill = app.workspace_tab_fill_color(idx, active, p);
                let title = app.workspaces[idx].title.clone();
                let badge = app.workspaces[idx].badge;
                let text_color = if active { p.tab_label_active } else { p.muted };

                let _tab_frame = egui::Frame::default()
                    .fill(fill)
                    .stroke(Stroke::new(1.0, p.border))
                    .inner_margin(Margin::symmetric(6, 3))
                    .show(ui, |ui| {
                        ui.horizontal(|ui| {
                            let tab_btn = egui::Button::new(
                                RichText::new(&title)
                                    .size(12.0)
                                    .family(FontFamily::Monospace)
                                    .color(text_color),
                            )
                            .frame(false)
                            .min_size(Vec2::new(74.0, 20.0));
                            let is_editing = app.editing_workspace_idx == Some(idx);
                            if is_editing {
                                let response = ui.add(
                                    egui::TextEdit::singleline(&mut app.editing_workspace_input)
                                        .desired_width(110.0)
                                        .font(egui::TextStyle::Monospace),
                                );
                                response.request_focus();
                                let pressed_enter = ui.input(|i| i.key_pressed(egui::Key::Enter));
                                let pressed_escape = ui.input(|i| i.key_pressed(egui::Key::Escape));
                                if response.lost_focus() || pressed_enter {
                                    let trimmed = app.editing_workspace_input.trim();
                                    if !trimmed.is_empty() {
                                        app.workspaces[idx].title = trimmed.to_string();
                                        app.next_workspace_index =
                                            compute_next_workspace_index(&app.workspaces);
                                        changed = true;
                                    }
                                    app.editing_workspace_idx = None;
                                    app.editing_workspace_input.clear();
                                } else if pressed_escape {
                                    app.editing_workspace_idx = None;
                                    app.editing_workspace_input.clear();
                                }
                            } else {
                                let tab_btn_resp = ui.add(tab_btn);
                                egui::Popup::context_menu(&tab_btn_resp)
                                    .close_behavior(egui::PopupCloseBehavior::CloseOnClickOutside)
                                    .show(|ui| {
                                        workspace_tab_context_menu(ui, app, idx, &mut changed, p);
                                    });
                                if tab_btn_resp.clicked() {
                                    app.selected_workspace = idx;
                                    egui::Popup::close_all(ui.ctx());
                                    changed = true;
                                }
                            }

                            if let Some(count) = badge {
                                ui.label(
                                    RichText::new(count.to_string())
                                        .size(11.0)
                                        .family(FontFamily::Monospace)
                                        .color(p.muted),
                                );
                            }

                            let close_btn = egui::Button::new(
                                RichText::new("x")
                                    .size(11.0)
                                    .family(FontFamily::Monospace)
                                    .color(p.tab_close),
                            )
                            .fill(Color32::TRANSPARENT)
                            .stroke(Stroke::NONE)
                            .min_size(Vec2::new(12.0, 20.0));
                            let close_resp = ui.add(close_btn).on_hover_text("Close workspace");
                            if close_resp.hovered() || close_resp.is_pointer_button_down_on() {
                                let close_bg = if close_resp.is_pointer_button_down_on() {
                                    p.tab_close_active_bg
                                } else {
                                    p.tab_close_hover_bg
                                };
                                let close_fg = if close_resp.is_pointer_button_down_on() {
                                    Color32::WHITE
                                } else {
                                    p.tab_close_hover_text
                                };
                                let painter = ui.painter();
                                painter.rect_filled(close_resp.rect.expand(2.0), 3.0, close_bg);
                                painter.text(
                                    close_resp.rect.center(),
                                    egui::Align2::CENTER_CENTER,
                                    "x",
                                    egui::FontId::monospace(11.0),
                                    close_fg,
                                );
                            }
                            if close_resp.clicked() {
                                close_idx = Some(idx);
                            }
                        });
                    });
            }

            let plus_btn = egui::Button::new(
                RichText::new("+")
                    .size(14.0)
                    .family(FontFamily::Monospace)
                    .color(p.muted),
            )
            .fill(p.tab_inactive_bg)
            .stroke(Stroke::new(1.0, p.border))
            .min_size(Vec2::new(26.0, 28.0))
            .corner_radius(3.0);
            if ui.add(plus_btn).on_hover_text("New workspace").clicked() {
                let title = format!("Workspace {}", app.next_workspace_index);
                let inherit_dir = app
                    .workspaces
                    .get(app.selected_workspace)
                    .map(|w| w.working_dir.clone())
                    .unwrap_or_else(default_working_dir);
                app.next_workspace_index += 1;
                app.workspaces.push(WorkspaceTab {
                    title,
                    badge: None,
                    color_rgba: None,
                    working_dir: inherit_dir,
                });
                app.workspace_runtime.push(WorkspaceRuntime::default());
                app.selected_workspace = app.workspaces.len() - 1;
                changed = true;
            }
        });

        let settings_btn_w = 34.0_f32;
        let slack = (ui.available_width() - settings_btn_w).max(0.0);
        ui.add_space(slack);
        // Close on outside click only so DragValue / text fields inside the menu stay usable.
        let _ = egui::containers::menu::MenuButton::from_button(egui::Button::new(
            RichText::new("⚙")
                .size(16.0)
                .family(FontFamily::Monospace),
        ))
        .config(
            egui::containers::menu::MenuConfig::new().close_behavior(
                egui::containers::PopupCloseBehavior::CloseOnClickOutside,
            ),
        )
        .ui(ui, |ui| {
            settings_menu(ui, app, &mut changed);
        });
    });

    if let Some(idx) = close_idx {
        app.workspaces.remove(idx);
        if idx < app.workspace_runtime.len() {
            app.workspace_runtime.remove(idx);
        }
        if app.editing_workspace_idx == Some(idx) {
            app.editing_workspace_idx = None;
            app.editing_workspace_input.clear();
        } else if let Some(edit_idx) = app.editing_workspace_idx {
            if edit_idx > idx {
                app.editing_workspace_idx = Some(edit_idx - 1);
            }
        }
        if app.workspaces.is_empty() {
            app.selected_workspace = 0;
        } else if app.selected_workspace > idx {
            app.selected_workspace -= 1;
        } else if app.selected_workspace == idx {
            app.selected_workspace = app.selected_workspace.saturating_sub(1);
        } else if app.selected_workspace >= app.workspaces.len() {
            app.selected_workspace = app.workspaces.len().saturating_sub(1);
        }
        changed = true;
    }

    if changed {
        app.next_workspace_index = compute_next_workspace_index(&app.workspaces);
        save_workspace_state(app);
    }
}

fn settings_menu(ui: &mut egui::Ui, app: &mut TermiteUi, changed: &mut bool) {
    ui.label("Appearance");
    ui.horizontal(|ui| {
        *changed |= ui
            .selectable_value(&mut app.ui_theme, UiTheme::Dark, "Dark")
            .clicked();
        *changed |= ui
            .selectable_value(&mut app.ui_theme, UiTheme::Light, "Light")
            .clicked();
    });
    ui.separator();
    ui.label("Theme style");
    *changed |= ui
        .selectable_value(&mut app.ui_style, UiStyle::Normal, "Normal")
        .clicked();
    *changed |= ui
        .selectable_value(&mut app.ui_style, UiStyle::Glass, "Glass")
        .clicked();
    ui.separator();
    ui.label("Panel layout");
    ui.label(
        RichText::new("Auto fits column count to window width. Fixed uses the column count you set.")
            .size(11.0)
            .color(ui.visuals().weak_text_color()),
    );
    ui.horizontal(|ui| {
        *changed |= ui
            .selectable_value(&mut app.panel_layout.mode, PanelLayoutMode::Auto, "Auto")
            .clicked();
        *changed |= ui
            .selectable_value(
                &mut app.panel_layout.mode,
                PanelLayoutMode::Fixed,
                "Fixed",
            )
            .clicked();
    });
    if app.panel_layout.mode == PanelLayoutMode::Fixed {
        ui.horizontal(|ui| {
            ui.label("Columns");
            let mut c = app.panel_layout.cols as i32;
            let resp = ui.add(
                egui::DragValue::new(&mut c)
                    .range(1..=MAX_WORKSPACE_COLUMNS as i32)
                    .speed(0.15)
                    .fixed_decimals(0),
            );
            if resp.changed() {
                app.panel_layout.cols = c.clamp(1, MAX_WORKSPACE_COLUMNS as i32) as u8;
                *changed = true;
            }
        });
    }
    *changed |= ui
        .checkbox(
            &mut app.sync_terminals_to_columns,
            "Sync terminals to columns (auto-fit width)",
        )
        .on_hover_text(
            "Sets each terminal to the column stripe width, aligns to column starts, and stacks from the top of each column. Pauses while you drag or resize a terminal.",
        )
        .changed();
}

fn workspace_tab_context_menu(
    ui: &mut egui::Ui,
    app: &mut TermiteUi,
    idx: usize,
    changed: &mut bool,
    p: UiPalette,
) {
    if ui.button("Rename").clicked() {
        app.editing_workspace_idx = Some(idx);
        app.editing_workspace_input = app.workspaces[idx].title.clone();
        ui.close();
    }

    ui.label(RichText::new("Tab color").size(11.0).color(p.muted));
    ui.horizontal(|ui| {
        let preview = if app.color_picker_target_idx == Some(idx) {
            app.color_picker_draft
        } else {
            app.workspaces[idx].editor_base_color(p)
        };
        let preview_btn = egui::Button::new("")
            .min_size(Vec2::new(18.0, 18.0))
            .fill(preview)
            .stroke(Stroke::new(1.0, p.border));
        let open_picker = ui.add(preview_btn).clicked();
        ui.label(RichText::new("Pick color").size(12.0).color(p.muted));
        if open_picker {
            app.color_picker_target_idx = Some(idx);
            app.color_picker_draft = app.workspaces[idx].editor_base_color(p);
            app.color_picker_original_rgba = app.workspaces[idx].color_rgba;
            app.color_hex_input = color_to_hex_string(app.color_picker_draft);
        }
    });

    let mut picker_rect: Option<egui::Rect> = None;
    if app.color_picker_target_idx == Some(idx) {
        app.color_picker_rendered_this_frame = true;
        let picker_resp = egui::Frame::default()
            .fill(p.popover_fill)
            .stroke(Stroke::new(1.0, p.border))
            .inner_margin(Margin::same(8))
            .show(ui, |ui| {
                let picker_changed = egui::color_picker::color_picker_color32(
                    ui,
                    &mut app.color_picker_draft,
                    egui::color_picker::Alpha::Opaque,
                );
                if picker_changed {
                    // Live preview is rendered from draft while picker is open.
                    app.color_hex_input = color_to_hex_string(app.color_picker_draft);
                }
                ui.add_space(6.0);
                ui.horizontal(|ui| {
                    if ui.button("Save").clicked() {
                        let picked = app.color_picker_draft;
                        app.workspaces[idx].set_custom_color(picked);
                        app.push_color_history(picked);
                        app.color_hex_input = color_to_hex_string(picked);
                        app.color_picker_target_idx = None;
                        app.color_picker_original_rgba = None;
                        *changed = true;
                        ui.close();
                    }
                    if ui.button("Skip").clicked() {
                        app.color_picker_target_idx = None;
                        app.color_picker_original_rgba = None;
                        app.color_hex_input =
                            color_to_hex_string(app.workspaces[idx].editor_base_color(p));
                    }
                });
            });
        picker_rect = Some(picker_resp.response.rect);
    }

    if app.color_hex_target_idx != Some(idx) {
        app.color_hex_target_idx = Some(idx);
        app.color_hex_input = color_to_hex_string(app.workspaces[idx].editor_base_color(p));
    }
    ui.horizontal(|ui| {
        ui.label(RichText::new("Hex").size(11.0).color(p.muted));
        let hex_resp = ui.text_edit_singleline(&mut app.color_hex_input);
        let enter_pressed = ui.input(|i| i.key_pressed(egui::Key::Enter));
        let apply_hex = (enter_pressed && hex_resp.has_focus()) || hex_resp.lost_focus();
        if apply_hex {
            if let Some(parsed) = parse_hex_color(&app.color_hex_input) {
                if app.color_picker_target_idx == Some(idx) {
                    app.color_picker_draft = parsed;
                } else {
                    app.workspaces[idx].set_custom_color(parsed);
                    app.push_color_history(parsed);
                    *changed = true;
                }
            }
        }
    });

    if app.color_picker_target_idx == Some(idx) {
        let save_with_enter = ui.ctx().input(|i| i.key_pressed(egui::Key::Enter));
        if save_with_enter {
            let picked = app.color_picker_draft;
            app.workspaces[idx].set_custom_color(picked);
            app.push_color_history(picked);
            app.color_hex_input = color_to_hex_string(picked);
            app.color_picker_target_idx = None;
            app.color_picker_original_rgba = None;
            *changed = true;
            ui.close();
        }

        let clicked = ui.ctx().input(|i| i.pointer.any_pressed());
        let pointer_pos = ui.ctx().input(|i| i.pointer.interact_pos());
        if clicked && app.color_picker_target_idx == Some(idx) {
            let outside_picker = match (picker_rect, pointer_pos) {
                (Some(rect), Some(pos)) => !rect.contains(pos),
                _ => true,
            };
            if outside_picker {
                let picked = app.color_picker_draft;
                app.workspaces[idx].set_custom_color(picked);
                app.push_color_history(picked);
                app.color_hex_input = color_to_hex_string(picked);
                app.color_picker_target_idx = None;
                app.color_picker_original_rgba = None;
                *changed = true;
            }
        }
    }

    if !app.color_history.is_empty() {
        ui.add_space(4.0);
        ui.label(RichText::new("Recent").size(11.0).color(p.muted));
        ui.horizontal_wrapped(|ui| {
            let history = app.color_history.clone();
            for rgba in history.iter().rev().take(10) {
                let swatch = Color32::from_rgba_unmultiplied(rgba[0], rgba[1], rgba[2], rgba[3]);
                let button = egui::Button::new("")
                    .min_size(Vec2::new(16.0, 16.0))
                    .fill(swatch)
                    .stroke(Stroke::new(1.0, p.border));
                if ui.add(button).clicked() {
                    app.workspaces[idx].set_custom_color(swatch);
                    app.push_color_history(swatch);
                    *changed = true;
                }
            }
        });
    }

    if ui.button("Reset Color").clicked() {
        app.workspaces[idx].color_rgba = None;
        if app.color_picker_target_idx == Some(idx) {
            app.color_picker_target_idx = None;
            app.color_picker_original_rgba = None;
        }
        *changed = true;
        ui.close();
    }
}

fn directory_path_bar(ui: &mut egui::Ui, app: &mut TermiteUi, p: UiPalette) {
    let full_width = ui.available_width();
    let bar_stroke = if app.ui_theme == UiTheme::Light {
        Stroke::NONE
    } else {
        Stroke::new(1.0, p.path_bar_border)
    };
    egui::Frame::default()
        .fill(p.path_bar_bg)
        .stroke(bar_stroke)
        .inner_margin(Margin::symmetric(10, 4))
        .show(ui, |ui| {
            ui.set_width(full_width);
            let selected_idx = app
                .selected_workspace
                .min(app.workspaces.len().saturating_sub(1));
            let displayed_dir = app
                .workspaces
                .get(selected_idx)
                .map(|w| w.working_dir.clone())
                .unwrap_or_else(default_working_dir);
            ui.horizontal(|ui| {
                let picker_btn =
                    egui::Button::new(RichText::new("◻").size(10.0).color(p.path_picker_icon))
                        .frame(false);
                if ui
                    .add(picker_btn)
                    .on_hover_text("Choose working directory")
                    .clicked()
                {
                    let mut dialog = FileDialog::new();
                    if PathBuf::from(&displayed_dir).is_dir() {
                        dialog = dialog.set_directory(&displayed_dir);
                    }
                    if let Some(folder) = dialog.pick_folder() {
                        if let Some(path) = folder.to_str() {
                            if let Some(w) = app.workspaces.get_mut(selected_idx) {
                                w.working_dir = path.to_string();
                                save_workspace_state(app);
                            }
                            app.editing_working_dir = false;
                            app.working_dir_input.clear();
                        }
                    }
                }
                ui.add_space(2.0);
                if app.editing_working_dir {
                    let response = ui.add(
                        egui::TextEdit::singleline(&mut app.working_dir_input)
                            .desired_width((full_width - 110.0).max(220.0))
                            .font(egui::TextStyle::Monospace),
                    );
                    response.request_focus();
                    let enter_pressed = ui.input(|i| i.key_pressed(egui::Key::Enter));
                    let esc_pressed = ui.input(|i| i.key_pressed(egui::Key::Escape));
                    if esc_pressed {
                        app.editing_working_dir = false;
                        app.working_dir_input.clear();
                    } else if ui.small_button("Apply").clicked() || enter_pressed {
                        let candidate = app.working_dir_input.trim();
                        if !candidate.is_empty() && PathBuf::from(candidate).is_dir() {
                            if let Some(w) = app.workspaces.get_mut(selected_idx) {
                                w.working_dir = candidate.to_string();
                                save_workspace_state(app);
                            }
                            app.editing_working_dir = false;
                            app.working_dir_input.clear();
                        }
                    }
                } else {
                    let path_btn = egui::Button::new(
                        RichText::new(displayed_dir)
                            .size(12.0)
                            .family(FontFamily::Monospace)
                            .color(p.muted),
                    )
                    .frame(false);
                    if ui
                        .add(path_btn)
                        .on_hover_text("Click to edit working directory")
                        .clicked()
                    {
                        app.editing_working_dir = true;
                        app.working_dir_input = app
                            .workspaces
                            .get(selected_idx)
                            .map(|w| w.working_dir.clone())
                            .unwrap_or_else(default_working_dir);
                    }
                }
            });
        });
}

fn workspace_state_path() -> PathBuf {
    if let Ok(home) = std::env::var("HOME") {
        return PathBuf::from(home)
            .join(".termite")
            .join("termite-ui-workspaces.json");
    }
    PathBuf::from(".termite-ui-workspaces.json")
}

fn save_workspace_state(app: &TermiteUi) {
    let state = WorkspaceState {
        ui_theme: app.ui_theme,
        ui_style: app.ui_style,
        panel_layout: app.panel_layout.sanitized(),
        sync_terminals_to_columns: app.sync_terminals_to_columns,
        selected_workspace: app.selected_workspace,
        workspaces: app
            .workspaces
            .iter()
            .enumerate()
            .map(|(idx, tab)| WorkspaceTabState {
                title: tab.title.clone(),
                badge: tab.badge,
                color_rgba: tab.color_rgba,
                working_dir: Some(tab.working_dir.clone()),
                terminal_sessions: app
                    .workspace_runtime
                    .get(idx)
                    .map(|runtime| {
                        runtime
                            .terminals
                            .iter()
                            .map(|pane| TerminalPaneState {
                                title: pane.title.clone(),
                                width: pane.desired_size.x,
                                height: pane.desired_size.y,
                                x: pane.position.map(|p| p.x),
                                y: pane.position.map(|p| p.y),
                            })
                            .collect()
                    })
                    .unwrap_or_default(),
                active_terminal: app
                    .workspace_runtime
                    .get(idx)
                    .and_then(|runtime| runtime.active_terminal),
            })
            .collect(),
        next_workspace_index: app.next_workspace_index,
        color_history: app.color_history.clone(),
    };

    let path = workspace_state_path();
    if let Some(parent) = path.parent() {
        let _ = fs::create_dir_all(parent);
    }

    if let Ok(json) = serde_json::to_string_pretty(&state) {
        let _ = fs::write(path, json);
    }
}

fn load_workspace_state() -> Option<WorkspaceState> {
    let path = workspace_state_path();
    let json = fs::read_to_string(path).ok()?;
    let state: WorkspaceState = serde_json::from_str(&json).ok()?;
    if state.workspaces.is_empty() {
        return None;
    }
    Some(state)
}

fn compute_next_workspace_index(workspaces: &[WorkspaceTab]) -> usize {
    let mut max_number = 0usize;
    for tab in workspaces {
        if let Some(number_text) = tab.title.strip_prefix("Workspace ") {
            if let Ok(number) = number_text.trim().parse::<usize>() {
                max_number = max_number.max(number);
            }
        }
    }
    if max_number == 0 {
        workspaces.len() + 1
    } else {
        max_number + 1
    }
}

impl WorkspaceTab {
    /// Full-strength color for editors (hex / picker), not dimmed like inactive tabs.
    fn editor_base_color(&self, p: UiPalette) -> Color32 {
        if let Some([r, g, b, a]) = self.color_rgba {
            Color32::from_rgba_unmultiplied(r, g, b, a)
        } else {
            p.tab_active_bg
        }
    }

    fn tab_color(&self, active: bool, p: UiPalette) -> Color32 {
        if let Some([r, g, b, a]) = self.color_rgba {
            let base = Color32::from_rgba_unmultiplied(r, g, b, a);
            if active {
                base
            } else {
                Color32::from_rgba_unmultiplied(
                    r.saturating_div(2),
                    g.saturating_div(2),
                    b.saturating_div(2),
                    a,
                )
            }
        } else if active {
            p.tab_active_bg
        } else {
            p.tab_inactive_bg
        }
    }

    fn set_custom_color(&mut self, color: Color32) {
        self.color_rgba = Some([color.r(), color.g(), color.b(), color.a()]);
    }
}

impl TermiteUi {
    fn push_color_history(&mut self, color: Color32) {
        let rgba = [color.r(), color.g(), color.b(), color.a()];
        self.color_history.retain(|item| *item != rgba);
        self.color_history.push(rgba);
        if self.color_history.len() > 24 {
            let overflow = self.color_history.len() - 24;
            self.color_history.drain(0..overflow);
        }
    }

    fn workspace_tab_fill_color(&self, idx: usize, active: bool, p: UiPalette) -> Color32 {
        if self.color_picker_target_idx == Some(idx) {
            let preview = self.color_picker_draft;
            if active {
                preview
            } else {
                Color32::from_rgba_unmultiplied(
                    preview.r().saturating_div(2),
                    preview.g().saturating_div(2),
                    preview.b().saturating_div(2),
                    preview.a(),
                )
            }
        } else {
            self.workspaces[idx].tab_color(active, p)
        }
    }

    fn cleanup_stale_color_picker(&mut self) {
        // If draft mode is active but picker didn't render this frame, menu is gone.
        // Drop draft mode to avoid ghost previews.
        if self.color_picker_target_idx.is_some() && !self.color_picker_rendered_this_frame {
            self.color_picker_target_idx = None;
            self.color_picker_original_rgba = None;
        }
    }

    fn ensure_workspace_runtime_slots(&mut self) {
        while self.workspace_runtime.len() < self.workspaces.len() {
            self.workspace_runtime.push(WorkspaceRuntime::default());
        }
        if self.workspace_runtime.len() > self.workspaces.len() {
            self.workspace_runtime.truncate(self.workspaces.len());
        }
    }

    fn active_workspace_runtime_mut(&mut self) -> Option<&mut WorkspaceRuntime> {
        self.ensure_workspace_runtime_slots();
        if self.workspaces.is_empty() {
            return None;
        }
        let idx = self
            .selected_workspace
            .min(self.workspaces.len().saturating_sub(1));
        self.workspace_runtime.get_mut(idx)
    }

    fn active_workspace_runtime(&self) -> Option<&WorkspaceRuntime> {
        if self.workspaces.is_empty() {
            return None;
        }
        let idx = self
            .selected_workspace
            .min(self.workspaces.len().saturating_sub(1));
        self.workspace_runtime.get(idx)
    }

    fn terminal_index_at_local_pos(&self, local_pos: Pos2) -> Option<usize> {
        let runtime = self.active_workspace_runtime()?;
        for idx in (0..runtime.terminals.len()).rev() {
            let pane = &runtime.terminals[idx];
            let pos = pane.position.unwrap_or(Pos2::ZERO);
            let rect = egui::Rect::from_min_size(pos, pane.desired_size);
            if rect.contains(local_pos) {
                return Some(idx);
            }
        }
        None
    }

    fn trigger_spawn_flash(&mut self, local_pos: Pos2) {
        self.pending_spawn_flash_pos = Some(local_pos);
        self.pending_spawn_flash_until = Some(Instant::now() + Duration::from_millis(260));
    }

    fn paint_spawn_flash(
        &mut self,
        ui: &mut egui::Ui,
        area_min: Pos2,
        area_size: Vec2,
        p: UiPalette,
    ) {
        let Some(until) = self.pending_spawn_flash_until else {
            return;
        };
        let now = Instant::now();
        if now >= until {
            self.pending_spawn_flash_until = None;
            self.pending_spawn_flash_pos = None;
            return;
        }
        let Some(local_pos) = self.pending_spawn_flash_pos else {
            return;
        };

        let remaining = (until - now).as_secs_f32() / 0.26_f32;
        let alpha = (remaining * 255.0).clamp(0.0, 255.0) as u8;
        let [r, g, b] = p.spawn_flash_rgb;
        let color = Color32::from_rgba_unmultiplied(r, g, b, alpha);

        let col = pick_column_at_x(local_pos.x, area_size.x, self.panel_layout);
        let w = column_stripe_width(area_size.x, self.panel_layout);
        let x = column_band_left(area_size.x, col, self.panel_layout);
        let rect = egui::Rect::from_min_size(
            area_min + Vec2::new(x, 0.0),
            Vec2::new(w, area_size.y.max(1.0)),
        );

        paint_broken_border(ui.painter(), rect, color);
    }
}

fn workspace_content_height(terminals: &[TerminalPane], viewport_h: f32) -> f32 {
    let mut bottom = viewport_h;
    for pane in terminals {
        let pos = pane.position.unwrap_or_default();
        bottom = bottom.max(pos.y + pane.desired_size.y);
    }
    bottom + GRID_SPACING * 2.0
}

/// Minimum `y` among panes whose center lies in the given column band.
fn min_y_topmost_in_column(
    terminals: &[TerminalPane],
    area_width: f32,
    column: usize,
    layout: PanelLayout,
) -> Option<f32> {
    terminals
        .iter()
        .filter_map(|pane| {
            let pos = pane.position.unwrap_or_default();
            let cx = pos.x + pane.desired_size.x * 0.5;
            (pick_column_at_x(cx, area_width, layout) == column).then_some(pos.y)
        })
        .min_by(|a, b| a.total_cmp(b))
}

/// Left edge for a new pane that is **right-aligned** to `[stripe_left, stripe_right]`, after
/// shifting right for any existing pane that overlaps this column band vertically in `[y, y+h]`.
fn intrusion_left_right_aligned(
    terminals: &[TerminalPane],
    stripe_left: f32,
    stripe_right: f32,
    y: f32,
    h: f32,
    gap: f32,
) -> f32 {
    let mut left = stripe_left;
    for pane in terminals {
        let pos = pane.position.unwrap_or_default();
        let pr = egui::Rect::from_min_size(pos, pane.desired_size);
        if pr.max.y <= y || pr.min.y >= y + h {
            continue;
        }
        if pr.max.x <= stripe_left || pr.min.x >= stripe_right {
            continue;
        }
        left = left.max(pr.max.x + gap);
    }
    left
}

/// Spawn overlap: different nominal columns ignore sub-pixel edge kisses; same column uses
/// vertical slack so stacked panes respect [`STACK_GAP_Y`].
fn spawn_candidate_overlaps_pane(
    candidate: egui::Rect,
    pane: &TerminalPane,
    area_width: f32,
    layout: PanelLayout,
) -> bool {
    let pos = pane.position.unwrap_or_default();
    let pr = egui::Rect::from_min_size(pos, pane.desired_size);
    let cand_col = pick_column_at_x(candidate.center().x, area_width, layout);
    let pane_col = pick_column_at_x(pr.center().x, area_width, layout);

    if cand_col != pane_col {
        const H_EPS: f32 = 3.0;
        const V_EPS: f32 = 2.0;
        let ix0 = candidate.min.x.max(pr.min.x);
        let iy0 = candidate.min.y.max(pr.min.y);
        let ix1 = candidate.max.x.min(pr.max.x);
        let iy1 = candidate.max.y.min(pr.max.y);
        return ix1 > ix0 + H_EPS && iy1 > iy0 + V_EPS;
    }

    let half_gap = STACK_GAP_Y * 0.5;
    let c = candidate.expand2(Vec2::new(0.0, half_gap));
    let p = pr.expand2(Vec2::new(0.0, half_gap));
    c.intersects(p)
}

/// Prefer the top of the column band, never overlapping any existing pane (any column).
/// Width is **stripe minus intrusions** (right edge stays on the column boundary) so wide
/// neighbours in other columns do not overlap. Tries height, then scans `y` upward.
fn find_spawn_column_no_overlap(
    terminals: &[TerminalPane],
    area_size: Vec2,
    column: usize,
    preferred_max_h: f32,
    default_h: f32,
    layout: PanelLayout,
) -> (Pos2, Vec2) {
    let slot_w = column_stripe_width(area_size.x, layout);
    let stripe_left = column_band_left(area_size.x, column, layout);
    let stripe_right = stripe_left + slot_w;
    let mut h = default_h.min(preferred_max_h).max(1.0);
    let min_h = 40.0_f32.min(h).max(1.0);
    let y_step = 2.0_f32;
    let min_spawn_w = 60.0_f32;
    let area_w = area_size.x;

    let overlaps = |rect: egui::Rect| -> bool {
        terminals
            .iter()
            .any(|pane| spawn_candidate_overlaps_pane(rect, pane, area_w, layout))
    };

    while h >= min_h {
        let max_y = (area_size.y - h).max(0.0);
        let mut y = 0.0_f32;
        while y <= max_y + 0.01 {
            let left = intrusion_left_right_aligned(
                terminals,
                stripe_left,
                stripe_right,
                y,
                h,
                GRID_SPACING,
            );
            let w = stripe_right - left;
            if w < min_spawn_w {
                y += y_step;
                continue;
            }
            let cand = egui::Rect::from_min_size(Pos2::new(left, y), Vec2::new(w, h));
            if !overlaps(cand) {
                return (cand.min, cand.size());
            }
            y += y_step;
        }
        h -= CELL_H;
    }

    let h = default_h
        .min(preferred_max_h)
        .max(TERMINAL_MIN_HEIGHT)
        .min(area_size.y)
        .max(1.0);
    let pos = find_non_overlapping_position_in_column(
        terminals,
        area_size,
        Vec2::new(slot_w, h),
        column,
        0.0,
        layout,
    );
    let left =
        intrusion_left_right_aligned(terminals, stripe_left, stripe_right, pos.y, h, GRID_SPACING);
    let w = (stripe_right - left).max(min_spawn_w).min(slot_w);
    let cand = egui::Rect::from_min_size(Pos2::new(left, pos.y), Vec2::new(w, h));
    if !overlaps(cand) {
        return (cand.min, cand.size());
    }
    (pos, Vec2::new(slot_w, h))
}

fn find_non_overlapping_position(
    terminals: &[TerminalPane],
    area_size: Vec2,
    new_size: Vec2,
) -> Pos2 {
    let max_x = (area_size.x - new_size.x).max(0.0);
    let max_y = (area_size.y - new_size.y).max(0.0);
    let step = 24.0;
    let padding = 4.0;

    let mut y = 0.0;
    while y <= max_y {
        let mut x = 0.0;
        while x <= max_x {
            let candidate = egui::Rect::from_min_size(Pos2::new(x, y), new_size);
            let overlaps = terminals.iter().any(|pane| {
                let pos = pane.position.unwrap_or(Pos2::ZERO);
                let rect = egui::Rect::from_min_size(pos, pane.desired_size).expand(padding);
                candidate.intersects(rect)
            });
            if !overlaps {
                return Pos2::new(x, y);
            }
            x += step;
        }
        y += step;
    }

    // Fallback: cascade near top-left if area is saturated.
    let offset = (terminals.len() as f32 * 20.0).min(max_x.max(max_y));
    Pos2::new(offset.min(max_x), offset.min(max_y))
}

/// Snap every pane to the layout column width and restack from the top (`y = 0`) in each column.
fn reflow_panes_to_column_starts(
    terminals: &mut [TerminalPane],
    available_width: f32,
    layout: PanelLayout,
) {
    if terminals.is_empty() || !(available_width > 0.0) {
        return;
    }
    let (_, _, n_cols) = column_slot_geometry(available_width, layout);
    let n_cols = n_cols.max(1);
    let slot = column_stripe_width(available_width, layout)
        .max(TERMINAL_MIN_WIDTH)
        .min(available_width.max(1.0));
    let max_x = (available_width - slot).max(0.0);

    let mut idxs: Vec<usize> = (0..terminals.len()).collect();
    idxs.sort_by(|&a, &b| {
        let pa = terminals[a].position.unwrap_or_default();
        let pb = terminals[b].position.unwrap_or_default();
        let ca = pick_column_at_x(pa.x + slot * 0.25, available_width, layout);
        let cb = pick_column_at_x(pb.x + slot * 0.25, available_width, layout);
        ca.cmp(&cb)
            .then(pa.y.total_cmp(&pb.y))
            .then(a.cmp(&b))
    });

    let mut floor_y = vec![0.0_f32; n_cols];
    let mut updates: Vec<(usize, Pos2)> = Vec::with_capacity(idxs.len());
    for idx in idxs {
        if terminals[idx].position.is_none() {
            continue;
        }
        let pos = terminals[idx].position.unwrap();
        let h = terminals[idx].desired_size.y;
        let col = pick_column_at_x(pos.x + slot * 0.25, available_width, layout)
            .min(n_cols.saturating_sub(1));
        let x = column_band_left(available_width, col, layout).clamp(0.0, max_x);
        let y = floor_y[col];
        floor_y[col] = y + h + STACK_GAP_Y;
        updates.push((idx, Pos2::new(x, y)));
    }
    for (idx, p) in updates {
        terminals[idx].desired_size.x = slot;
        terminals[idx].position = Some(p);
    }
}

/// How many vertical columns fit at `area_width` with at least [`TERMINAL_MIN_WIDTH`] per slot.
fn workspace_column_count_auto(area_width: f32) -> usize {
    if area_width <= 0.0 {
        return 1;
    }
    let g = GRID_SPACING;
    let denom = TERMINAL_MIN_WIDTH + g;
    let n = ((area_width + g) / denom).floor() as isize;
    n.clamp(1, MAX_WORKSPACE_COLUMNS as isize) as usize
}

/// `(slot_width, gutter, column_count)` with `n * slot + (n - 1) * gutter ≈ area_width`.
fn column_slot_geometry(area_width: f32, layout: PanelLayout) -> (f32, f32, usize) {
    let n = layout.column_count(area_width);
    let gutter = GRID_SPACING;
    let usable = (area_width - (n as f32 - 1.0) * gutter).max(1.0);
    let slot_w = usable / n as f32;
    (slot_w, gutter, n)
}

/// Column slot width (spawn stripe / default column width).
fn column_stripe_width(area_width: f32, layout: PanelLayout) -> f32 {
    column_slot_geometry(area_width, layout).0
}

fn column_band_left(area_width: f32, column: usize, layout: PanelLayout) -> f32 {
    let (w, g, n) = column_slot_geometry(area_width, layout);
    let col = column.min(n.saturating_sub(1));
    col as f32 * (w + g)
}

fn pick_column_at_x(cursor_x: f32, area_width: f32, layout: PanelLayout) -> usize {
    let (w, g, n) = column_slot_geometry(area_width, layout);
    let x = cursor_x.clamp(0.0, area_width);
    if n <= 1 {
        return 0;
    }
    for i in 0..n - 1 {
        let slot_left = i as f32 * (w + g);
        let slot_right = slot_left + w;
        let gutter_right = slot_right + g;
        if x < slot_right {
            return i;
        }
        if x < gutter_right {
            let mid = slot_right + g * 0.5;
            return if x < mid { i } else { i + 1 };
        }
    }
    n - 1
}

/// Vertical X coordinates for snapping to the column layout (slot edges, gutters, workspace edge).
fn column_grid_vertical_snap_xs(area_width: f32, layout: PanelLayout) -> Vec<f32> {
    let (w, g, n) = column_slot_geometry(area_width, layout);
    if !(area_width > 0.0 && w > 0.0 && n > 0) {
        return Vec::new();
    }
    let mut xs = Vec::with_capacity(n * 2 + 1);
    for i in 0..n {
        let left = i as f32 * (w + g);
        xs.push(left);
        xs.push(left + w);
    }
    xs.push(area_width);
    xs.sort_by(|a, b| a.partial_cmp(b).unwrap());
    xs.dedup_by(|a, b| (*a - *b).abs() < 0.01);
    xs
}

fn find_non_overlapping_position_in_column(
    terminals: &[TerminalPane],
    area_size: Vec2,
    pane_size: Vec2,
    column: usize,
    preferred_y: f32,
    layout: PanelLayout,
) -> Pos2 {
    let mut x = column_band_left(area_size.x, column, layout);
    let max_x = (area_size.x - pane_size.x).max(0.0);
    x = x.clamp(0.0, max_x);
    let max_y = (area_size.y - pane_size.y).max(0.0);
    let preferred_y = preferred_y.clamp(0.0, max_y);
    let padding = 4.0;
    let step = 24.0;
    let gap = STACK_GAP_Y;

    let overlaps_at = |y: f32| -> bool {
        let candidate = egui::Rect::from_min_size(Pos2::new(x, y), pane_size);
        terminals.iter().any(|pane| {
            let pos = pane.position.unwrap_or(Pos2::ZERO);
            let rect = egui::Rect::from_min_size(pos, pane.desired_size).expand(padding);
            candidate.intersects(rect)
        })
    };

    // Work only with terminals that currently belong to the selected column.
    let mut column_rects: Vec<egui::Rect> = terminals
        .iter()
        .filter_map(|pane| {
            let pos = pane.position.unwrap_or(Pos2::ZERO);
            let rect = egui::Rect::from_min_size(pos, pane.desired_size);
            let center_x = rect.center().x;
            if pick_column_at_x(center_x, area_size.x, layout) == column {
                Some(rect)
            } else {
                None
            }
        })
        .collect();
    column_rects.sort_by(|a, b| a.min.y.total_cmp(&b.min.y));

    // If column is empty, place at the top start of that column.
    let mut start_y = 0.0;
    let mut primary_direction_down = true;
    if let Some(nearest) = column_rects.iter().min_by(|a, b| {
        let da = (preferred_y - a.center().y).abs();
        let db = (preferred_y - b.center().y).abs();
        da.total_cmp(&db)
    }) {
        primary_direction_down = preferred_y >= nearest.center().y;
        start_y = if primary_direction_down {
            nearest.max.y + gap
        } else {
            nearest.min.y - pane_size.y - gap
        }
        .clamp(0.0, max_y);
    }

    // Try from preferred anchor in primary direction first.
    if !overlaps_at(start_y) {
        return Pos2::new(x, start_y);
    }

    let mut y = start_y;
    if primary_direction_down {
        while y <= max_y {
            if !overlaps_at(y) {
                return Pos2::new(x, y);
            }
            y += step;
        }
        y = start_y;
        while y > 0.0 {
            y = (y - step).max(0.0);
            if !overlaps_at(y) {
                return Pos2::new(x, y);
            }
            if y == 0.0 {
                break;
            }
        }
    } else {
        while y > 0.0 {
            y = (y - step).max(0.0);
            if !overlaps_at(y) {
                return Pos2::new(x, y);
            }
            if y == 0.0 {
                break;
            }
        }
        y = start_y;
        while y <= max_y {
            if !overlaps_at(y) {
                return Pos2::new(x, y);
            }
            y += step;
        }
    }

    Pos2::new(x, start_y)
}

fn next_available_terminal_number(terminals: &[TerminalPane], workspace_number: usize) -> usize {
    let prefix = format!("Workspace {} - Terminal ", workspace_number);
    let max_existing = terminals
        .iter()
        .filter_map(|pane| {
            if let Some(num) = pane.title.strip_prefix(&prefix) {
                return num.trim().parse::<usize>().ok();
            }
            // Backward compatibility with older titles like "Terminal N".
            pane.title
                .strip_prefix("Terminal ")
                .and_then(|num| num.trim().parse::<usize>().ok())
        })
        .max()
        .unwrap_or(0);
    max_existing + 1
}

fn default_working_dir() -> String {
    std::env::current_dir()
        .ok()
        .and_then(|p| p.to_str().map(ToString::to_string))
        .unwrap_or_else(|| ".".to_string())
}

fn parse_hex_color(input: &str) -> Option<Color32> {
    let trimmed = input.trim().trim_start_matches('#');
    match trimmed.len() {
        6 => {
            let r = u8::from_str_radix(&trimmed[0..2], 16).ok()?;
            let g = u8::from_str_radix(&trimmed[2..4], 16).ok()?;
            let b = u8::from_str_radix(&trimmed[4..6], 16).ok()?;
            Some(Color32::from_rgb(r, g, b))
        }
        8 => {
            let r = u8::from_str_radix(&trimmed[0..2], 16).ok()?;
            let g = u8::from_str_radix(&trimmed[2..4], 16).ok()?;
            let b = u8::from_str_radix(&trimmed[4..6], 16).ok()?;
            let a = u8::from_str_radix(&trimmed[6..8], 16).ok()?;
            Some(Color32::from_rgba_unmultiplied(r, g, b, a))
        }
        _ => None,
    }
}

fn color_to_hex_string(color: Color32) -> String {
    format!("#{:02X}{:02X}{:02X}", color.r(), color.g(), color.b())
}
