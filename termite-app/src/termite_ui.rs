use crossbeam_channel::unbounded;
use eframe::egui::text::{LayoutJob, TextFormat};
use eframe::egui::{
    self, Color32, CursorIcon, FontFamily, FontId, Margin, Pos2, RichText, Sense, Stroke, Vec2,
};
use rfd::FileDialog;
use serde::{Deserialize, Serialize};
use std::{
    fs,
    io::{Read, Write},
    net::TcpStream,
    path::PathBuf,
    process::{Command, Stdio},
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};
use sysinfo::{
    CpuRefreshKind, MemoryRefreshKind, ProcessRefreshKind, ProcessesToUpdate, RefreshKind, System,
    get_current_pid,
};
use termite_core::{pty::spawn_pty, session::TerminalSession, PaneId, PtyHandle};
use termite_render::color::ansi_indexed_to_rgb;
use termite_render::SelectionRange;
use termite_vt::cell::{Cell, CellAttrs, Color, WideKind};
use termite_vt::TerminalGrid;

mod daemon;
mod clipboard;

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

const LINE_EDITOR_MAX_HISTORY: usize = 100;

/// A single snapshot of the line buffer: text content + cursor position.
#[derive(Clone)]
struct LineState {
    text: String,
    /// Char index from the start of `text` (0 = before first char).
    cursor: usize,
}

impl LineState {
    fn new() -> Self {
        Self { text: String::new(), cursor: 0 }
    }
}

/// Per-pane shadow line editor with undo/redo.
///
/// Tracks the full text and an absolute cursor index (chars from start),
/// so any input source — keyboard, mouse click, arrow keys — can update
/// `cursor` without converting between "from-end" and "from-start".
///
/// Undo/redo stacks store `LineState` snapshots (text + cursor) so that
/// restoring a snapshot also repositions the terminal cursor correctly.
struct LineEditor {
    current: LineState,
    undo_stack: Vec<LineState>,
    redo_stack: Vec<LineState>,
}

impl LineEditor {
    fn new() -> Self {
        Self { current: LineState::new(), undo_stack: Vec::new(), redo_stack: Vec::new() }
    }

    fn cursor_byte_pos(&self) -> usize {
        self.current.text
            .char_indices()
            .nth(self.current.cursor)
            .map(|(b, _)| b)
            .unwrap_or(self.current.text.len())
    }

    fn char_before_cursor(&self) -> Option<char> {
        if self.current.cursor == 0 { return None; }
        self.current.text.char_indices().nth(self.current.cursor - 1).map(|(_, c)| c)
    }

    fn push_snapshot(&mut self) {
        self.undo_stack.push(self.current.clone());
        if self.undo_stack.len() > LINE_EDITOR_MAX_HISTORY {
            self.undo_stack.remove(0);
        }
        self.redo_stack.clear();
    }

    fn push_text(&mut self, text: &str) {
        for ch in text.chars() {
            let is_word = ch.is_alphanumeric() || ch == '_';
            let last_is_word = self.char_before_cursor()
                .map(|c| c.is_alphanumeric() || c == '_')
                .unwrap_or(false);
            if self.current.text.is_empty() || is_word != last_is_word {
                self.push_snapshot();
            }
            let byte_pos = self.cursor_byte_pos();
            self.current.text.insert(byte_pos, ch);
            self.current.cursor += 1;
        }
    }

    fn push_backspace(&mut self) {
        if self.current.cursor == 0 { return; }
        self.push_snapshot();
        let (byte_start, _) = self.current.text
            .char_indices()
            .nth(self.current.cursor - 1)
            .expect("cursor > 0 guarantees char exists");
        self.current.text.remove(byte_start);
        self.current.cursor -= 1;
    }

    fn move_left(&mut self) {
        self.current.cursor = self.current.cursor.saturating_sub(1);
    }

    fn move_right(&mut self) {
        let max = self.current.text.chars().count();
        if self.current.cursor < max { self.current.cursor += 1; }
    }

    fn move_to_start(&mut self) { self.current.cursor = 0; }

    fn move_to_end(&mut self) {
        self.current.cursor = self.current.text.chars().count();
    }

    /// Apply a signed column delta from a mouse click (right = positive).
    fn move_cursor_delta(&mut self, delta: isize) {
        let max = self.current.text.chars().count() as isize;
        self.current.cursor = (self.current.cursor as isize + delta).clamp(0, max) as usize;
    }

    /// Returns the snapshot to restore, or `None` if nothing to undo.
    fn undo(&mut self) -> Option<LineState> {
        let prev = self.undo_stack.pop()?;
        self.redo_stack.push(self.current.clone());
        self.current = prev.clone();
        Some(prev)
    }

    /// Returns the snapshot to restore, or `None` if nothing to redo.
    fn redo(&mut self) -> Option<LineState> {
        let next = self.redo_stack.pop()?;
        self.undo_stack.push(self.current.clone());
        self.current = next.clone();
        Some(next)
    }

    fn reset(&mut self) {
        self.current = LineState::new();
        self.undo_stack.clear();
        self.redo_stack.clear();
    }
}

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
/// CPU and memory readings for the bottom status strip.
const SYSTEM_STATUS_SAMPLE_INTERVAL: Duration = Duration::from_millis(900);
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
/// [`TermiteUi::paint_spawn_flash`]. `area_w` is the visible workspace width used for column
/// stripes (not the horizontal scroll canvas). `area_h` is full workspace content height.
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
    max_y: f32,
    best_x: &mut Option<(f32, f32, f32)>,
    best_y: &mut Option<(f32, f32, f32)>,
    layout: PanelLayout,
) {
    for snap_x in column_grid_vertical_snap_xs(column_area_w, layout) {
        let d_left = (pos.x - snap_x).abs();
        if d_left <= RESIZE_SNAP_DISTANCE {
            let nx = snap_x.max(0.0);
            if best_x.is_none_or(|(bd, _, _)| d_left < bd) {
                *best_x = Some((d_left, nx, snap_x));
            }
        }
        let d_right = ((pos.x + w) - snap_x).abs();
        if d_right <= RESIZE_SNAP_DISTANCE {
            let nx = (snap_x - w).max(0.0);
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
                    let nx = snap_x.max(0.0);
                    if best_x.is_none_or(|(bd, _, _)| d_left < bd) {
                        *best_x = Some((d_left, nx, snap_x));
                    }
                }
                let d_right = ((pos.x + w) - snap_x).abs();
                if d_right <= RESIZE_SNAP_DISTANCE {
                    let nx = (snap_x - w).max(0.0);
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
    if std::env::args().any(|a| a == "--daemon") {
        let _ = daemon::run_daemon();
        return Ok(());
    }

    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_inner_size([1440.0, 860.0])
            .with_min_inner_size([1100.0, 700.0])
            .with_title("TermITE"),
        ..Default::default()
    };

    eframe::run_native(
        "TermITE",
        options,
        Box::new(|_cc| Ok(Box::<TermiteUi>::default())),
    )
}

// Session daemon protocol frame types (mirrors `termite-app/src/daemon.rs`).
const FRAME_ATTACH: u8 = 1;
const FRAME_ATTACH_ERROR: u8 = 2;
const FRAME_OUTPUT: u8 = 3;
const FRAME_INPUT: u8 = 4;
const FRAME_RESIZE: u8 = 5;

enum TerminalBackend {
    LocalPty { pty: PtyHandle },
    DaemonPty { writer: Arc<Mutex<TcpStream>> },
}

impl TerminalBackend {
    fn write_all(&self, bytes: &[u8]) {
        match self {
            TerminalBackend::LocalPty { pty } => {
                let _ = pty.write_all(bytes);
            }
            TerminalBackend::DaemonPty { writer } => {
                let Ok(mut w) = writer.lock() else {
                    return;
                };

                let len = bytes.len() as u32;
                let mut header = [0u8; 5];
                header[0] = FRAME_INPUT;
                header[1..5].copy_from_slice(&len.to_le_bytes());

                let _ = w.write_all(&header);
                if !bytes.is_empty() {
                    let _ = w.write_all(bytes);
                }
            }
        }
    }

    fn resize(&self, rows: u16, cols: u16) {
        match self {
            TerminalBackend::LocalPty { pty } => {
                let _ = pty.resize(rows, cols);
            }
            TerminalBackend::DaemonPty { writer } => {
                let Ok(mut w) = writer.lock() else {
                    return;
                };

                let mut payload = [0u8; 4];
                payload[0..2].copy_from_slice(&rows.to_le_bytes());
                payload[2..4].copy_from_slice(&cols.to_le_bytes());

                let len = payload.len() as u32;
                let mut header = [0u8; 5];
                header[0] = FRAME_RESIZE;
                header[1..5].copy_from_slice(&len.to_le_bytes());

                let _ = w.write_all(&header);
                let _ = w.write_all(&payload);
            }
        }
    }
}

struct TerminalPane {
    id: u64,
    title: String,
    tmux_session: String,
    session: TerminalSession,
    backend: TerminalBackend,
    desired_size: Vec2,
    position: Option<Pos2>,
}

#[derive(Serialize, Deserialize, Clone)]
struct TerminalPaneState {
    #[serde(default)]
    id: u64,
    title: String,
    #[serde(default)]
    tmux_session: Option<String>,
    width: f32,
    height: f32,
    x: Option<f32>,
    y: Option<f32>,
}

struct TermiteUi {
    ui_theme: UiTheme,
    ui_style: UiStyle,
    selected_workspace: usize,
    workspaces: Vec<WorkspaceTab>,
    next_workspace_index: usize,
    workspace_runtime: Vec<WorkspaceRuntime>,
    next_terminal_id: u64,
    /// Horizontal scroll canvas width and scrollable content height (spawn search, persistence).
    terminal_area_size: Vec2,
    /// Visible workspace inside the central panel. Width drives column stripes and auto-fit;
    /// height caps default new pane height.
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
    /// Live CPU / RAM / load readings (`sysinfo`).
    system: System,
    system_last_sample: Instant,
    /// Open usage panels, ordered from oldest click to newest click.
    /// `true` => termite panel, `false` => system panel.
    usage_panel_open_order: Vec<bool>,
    show_termite_only_status: bool,
    equal_size_picker_open: bool,
    equal_size_picker_selection: Option<u64>,
    equal_size_template_blink_terminal_id: Option<u64>,
    equal_size_template_blink_started_at: Option<Instant>,
}

#[derive(Default)]
struct WorkspaceRuntime {
    terminals: Vec<TerminalPane>,
    active_terminal: Option<usize>,
    equal_size_source_terminal_id: Option<u64>,
    selections: Vec<Option<SelectionRange>>,
    line_editors: Vec<LineEditor>,
}

struct WorkspaceTab {
    title: String,
    badge: Option<u8>,
    color_rgba: Option<[u8; 4]>,
    working_dir: String,
    panel_layout: PanelLayout,
    /// When set, terminal widths match column stripes and panes snap to column starts, restacked from the top (UI: "Auto-fit width").
    sync_terminals_to_columns: bool,
    /// When set, all terminals share one size in a grid that fills the workspace (overrides auto-fit width).
    uniform_equal_terminals: bool,
}

#[derive(Serialize, Deserialize)]
struct WorkspaceState {
    #[serde(default)]
    ui_theme: UiTheme,
    #[serde(default)]
    ui_style: UiStyle,
    // Legacy, pre-workspace-scoped values. Kept for backward compatibility while old
    // persisted state is being migrated.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    sync_terminals_to_columns: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    uniform_equal_terminals: Option<bool>,
    selected_workspace: usize,
    workspaces: Vec<WorkspaceTabState>,
    next_workspace_index: usize,
    #[serde(default)]
    color_history: Vec<[u8; 4]>,
    #[serde(default)]
    usage_panel_pinned_scope: Option<bool>,
    #[serde(default)]
    usage_panel_open_order: Vec<bool>,
    #[serde(default)]
    show_termite_only_status: bool,
}

#[derive(Serialize, Deserialize, Clone)]
struct WorkspaceTabState {
    title: String,
    badge: Option<u8>,
    color_rgba: Option<[u8; 4]>,
    #[serde(default)]
    panel_layout: PanelLayout,
    // Workspace-scoped values. If missing (old state), fall back to legacy root fields.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    sync_terminals_to_columns: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    uniform_equal_terminals: Option<bool>,
    #[serde(default)]
    working_dir: Option<String>,
    #[serde(default)]
    terminal_sessions: Vec<TerminalPaneState>,
    #[serde(default)]
    active_terminal: Option<usize>,
    #[serde(default)]
    equal_size_source_terminal_id: Option<u64>,
}

impl Default for TermiteUi {
    fn default() -> Self {
        if let Some(state) = load_workspace_state() {
            let ui_theme = state.ui_theme;
            let ui_style = state.ui_style;
            let theme_palette = ui_theme.palette().with_style(ui_style);
            let tab_states = state.workspaces.clone();
            let workspaces: Vec<WorkspaceTab> = tab_states
                .iter()
                .map(|tab| WorkspaceTab {
                    title: tab.title.clone(),
                    badge: tab.badge,
                    color_rgba: tab.color_rgba,
                    working_dir: tab.working_dir.clone().unwrap_or_else(default_working_dir),
                    panel_layout: tab.panel_layout.sanitized(),
                    sync_terminals_to_columns: tab
                        .sync_terminals_to_columns
                        .or(state.sync_terminals_to_columns)
                        .unwrap_or(false),
                    uniform_equal_terminals: tab
                        .uniform_equal_terminals
                        .or(state.uniform_equal_terminals)
                        .unwrap_or(false),
                })
                .collect();
            let next_workspace_index = compute_next_workspace_index(&workspaces);
            let runtime_count = workspaces.len();
            let mut app = Self {
                ui_theme,
                ui_style,
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
                system: system_status_probe_new(),
                system_last_sample: Instant::now() - SYSTEM_STATUS_SAMPLE_INTERVAL,
                usage_panel_open_order: if state.usage_panel_open_order.is_empty() {
                    state.usage_panel_pinned_scope.into_iter().collect()
                } else {
                    state.usage_panel_open_order
                },
                show_termite_only_status: state.show_termite_only_status,
                equal_size_picker_open: false,
                equal_size_picker_selection: None,
                equal_size_template_blink_terminal_id: None,
                equal_size_template_blink_started_at: None,
            };
            // Restore terminal sessions per workspace from persisted metadata.
            for idx in 0..app.workspaces.len() {
                if let Some(saved_tab) = tab_states.get(idx) {
                    let working_dir = app.workspaces[idx].working_dir.clone();
                    let mut restored: Vec<TerminalPane> = Vec::new();
                    for pane_state in &saved_tab.terminal_sessions {
                        let terminal_id = pane_state.id.max(app.next_terminal_id);
                        let tmux_session = pane_state
                            .tmux_session
                            .clone()
                            .unwrap_or_else(|| tmux_session_name(idx, terminal_id));
                        let mut pane = spawn_terminal_pane(
                            pane_state.title.clone(),
                            terminal_id,
                            &working_dir,
                            &tmux_session,
                        );
                        app.next_terminal_id = terminal_id + 1;
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
                        runtime.equal_size_source_terminal_id = saved_tab.equal_size_source_terminal_id;
                    }
                }
            }
            return app;
        }

        Self {
            ui_theme: UiTheme::default(),
            ui_style: UiStyle::default(),
            selected_workspace: 2,
            workspaces: vec![
                WorkspaceTab {
                    title: "Workspace 1".to_string(),
                    badge: Some(2),
                    color_rgba: None,
                    working_dir: default_working_dir(),
                    panel_layout: PanelLayout::default(),
                    sync_terminals_to_columns: false,
                    uniform_equal_terminals: false,
                },
                WorkspaceTab {
                    title: "Workspace 2".to_string(),
                    badge: None,
                    color_rgba: None,
                    working_dir: default_working_dir(),
                    panel_layout: PanelLayout::default(),
                    sync_terminals_to_columns: false,
                    uniform_equal_terminals: false,
                },
                WorkspaceTab {
                    title: "Workspace 3".to_string(),
                    badge: Some(11),
                    color_rgba: None,
                    working_dir: default_working_dir(),
                    panel_layout: PanelLayout::default(),
                    sync_terminals_to_columns: false,
                    uniform_equal_terminals: false,
                },
                WorkspaceTab {
                    title: "Workspace 4".to_string(),
                    badge: Some(5),
                    color_rgba: None,
                    working_dir: default_working_dir(),
                    panel_layout: PanelLayout::default(),
                    sync_terminals_to_columns: false,
                    uniform_equal_terminals: false,
                },
                WorkspaceTab {
                    title: "Workspace 5".to_string(),
                    badge: None,
                    color_rgba: None,
                    working_dir: default_working_dir(),
                    panel_layout: PanelLayout::default(),
                    sync_terminals_to_columns: false,
                    uniform_equal_terminals: false,
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
            system: system_status_probe_new(),
            system_last_sample: Instant::now() - SYSTEM_STATUS_SAMPLE_INTERVAL,
            usage_panel_open_order: Vec::new(),
            show_termite_only_status: false,
            equal_size_picker_open: false,
            equal_size_picker_selection: None,
            equal_size_template_blink_terminal_id: None,
            equal_size_template_blink_started_at: None,
        }
    }
}

fn system_status_probe_new() -> System {
    System::new_with_specifics(
        RefreshKind::nothing()
            .with_cpu(CpuRefreshKind::everything())
            .with_memory(MemoryRefreshKind::everything()),
    )
}

fn format_gib(n: u64) -> String {
    format!("{:.1} GiB", n as f64 / (1024.0 * 1024.0 * 1024.0))
}

fn usage_meter_row(
    ui: &mut egui::Ui,
    label: &str,
    ratio: f32,
    value_text: String,
    fill: Color32,
    p: UiPalette,
) {
    ui.horizontal(|ui| {
        ui.label(
            RichText::new(label)
                .size(10.0)
                .family(FontFamily::Monospace)
                .color(p.muted),
        );
        ui.add(
            egui::ProgressBar::new(ratio)
                .desired_width(126.0)
                .fill(fill)
                .show_percentage(),
        );
        ui.label(
            RichText::new(value_text)
                .size(10.0)
                .family(FontFamily::Monospace)
                .color(p.text),
        );
    });
}

fn new_terminal_context_menu(
    ui: &mut egui::Ui,
    app: &mut TermiteUi,
    target_terminal: Option<usize>,
) {
    let mut changed = false;
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
    if is_cli_command_available("gemini") && ui.button("New Gemini").clicked() {
        let spawn_pos = app.pending_terminal_spawn_pos.take();
        let anchor_terminal = app.pending_context_terminal.take().or(target_terminal);
        app.add_terminal(spawn_pos, anchor_terminal);
        app.launch_cli_tool(None, "gemini");
        app.pending_context_terminal = None;
        ui.close();
    }
    if is_cli_command_available("agent") && ui.button("New Cursor").clicked() {
        let spawn_pos = app.pending_terminal_spawn_pos.take();
        let anchor_terminal = app.pending_context_terminal.take().or(target_terminal);
        app.add_terminal(spawn_pos, anchor_terminal);
        app.launch_cli_tool(None, "agent");
        app.pending_context_terminal = None;
        ui.close();
    }
    ui.separator();
    if app.active_workspace_tab_mut().is_some() {
        let mut panel_layout = app.active_panel_layout();
        let ws_idx = app.selected_workspace.min(app.workspaces.len().saturating_sub(1));
        let mut sync_terminals_to_columns = app.workspaces[ws_idx].sync_terminals_to_columns;
        let mut uniform_equal_terminals = app.workspaces[ws_idx].uniform_equal_terminals;
        let mut open_equal_picker = false;

        ui.menu_button("Panel layout", |ui| {
            changed |= ui
                .selectable_value(&mut panel_layout.mode, PanelLayoutMode::Auto, "Auto")
                .clicked();
            ui.horizontal(|ui| {
                let fixed_selected = panel_layout.mode == PanelLayoutMode::Fixed;
                if ui.selectable_label(fixed_selected, "Fixed").clicked() {
                    panel_layout.mode = PanelLayoutMode::Fixed;
                    changed = true;
                };
                let mut c = panel_layout.cols as i32;
                let resp = ui.add(
                    egui::DragValue::new(&mut c)
                        .range(1..=MAX_WORKSPACE_COLUMNS as i32)
                        .speed(0.15)
                        .fixed_decimals(0),
                );
                if resp.changed() {
                    panel_layout.mode = PanelLayoutMode::Fixed;
                    panel_layout.cols = c.clamp(1, MAX_WORKSPACE_COLUMNS as i32) as u8;
                    changed = true;
                }
            });
            ui.separator();
            changed |= ui
                .checkbox(
                    &mut sync_terminals_to_columns,
                    "Auto-fit width",
                )
                .on_hover_text(
                    "Auto-fits width to each column stripe and auto-positions panes at column starts, restacked from the top in each column. Pauses while you drag or resize a terminal.",
                )
                .changed();
            let equal_toggle_changed = ui
                .checkbox(
                    &mut uniform_equal_terminals,
                    "Equal-size grid (fit all terminals)",
                )
                .on_hover_text(
                    "Arranges every terminal in a grid with the same width and height, filling the workspace. Pauses while the mouse button is held. When both are on, this overrides auto-fit width.",
                )
                .changed();
            changed |= equal_toggle_changed;
            if equal_toggle_changed && uniform_equal_terminals {
                open_equal_picker = true;
            }
        });

        if let Some(tab) = app.active_workspace_tab_mut() {
            tab.panel_layout = panel_layout.sanitized();
            tab.sync_terminals_to_columns = sync_terminals_to_columns;
            tab.uniform_equal_terminals = uniform_equal_terminals;
        }
        if open_equal_picker {
            app.open_equal_size_picker_for_active_workspace();
        } else if !uniform_equal_terminals {
            app.equal_size_picker_open = false;
            app.equal_size_picker_selection = None;
        }
    }
    if changed {
        save_workspace_state(app);
    }
}

impl eframe::App for TermiteUi {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        // Keep refreshing so PTY output appears live without explicit wakeups.
        self.sync_all_workspace_runtime_buffers();
        ctx.request_repaint_after(Duration::from_millis(16));
        self.drain_terminals();
        self.handle_keyboard_input(ctx);
        self.color_picker_rendered_this_frame = false;
        self.refresh_system_status_if_due();

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

        egui::TopBottomPanel::bottom("system_status")
            .resizable(false)
            .exact_height(30.0)
            .frame(
                egui::Frame::default()
                    .fill(p.path_bar_bg)
                    .stroke(Stroke::new(1.0, p.path_bar_border))
                    .inner_margin(Margin::symmetric(10, 6)),
            )
            .show(ctx, |ui| {
                ui.horizontal(|ui| {
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        let term_resp = ui.add(
                            egui::Label::new(
                                RichText::new("Termite")
                                    .size(11.0)
                                    .color(p.text),
                            )
                            .sense(Sense::click()),
                        )
                        .on_hover_cursor(CursorIcon::PointingHand);
                        if term_resp.clicked() {
                            self.toggle_usage_panel(true);
                        }

                        let sep_resp = ui.label(RichText::new("|").size(11.0).color(p.muted));
                        let mut selector_resp = term_resp.union(sep_resp);

                        let sys_resp = ui.add(
                            egui::Label::new(
                                RichText::new("System")
                                    .size(11.0)
                                    .color(p.text),
                            )
                            .sense(Sense::click()),
                        )
                        .on_hover_cursor(CursorIcon::PointingHand);
                        if sys_resp.clicked() {
                            self.toggle_usage_panel(false);
                        }
                        selector_resp = selector_resp.union(sys_resp);

                        ui.add_space(6.0);
                        let usage_resp = ui.label(
                            RichText::new("Usage:")
                                .size(11.0)
                                .strong()
                                .color(p.muted),
                        );
                        selector_resp = selector_resp.union(usage_resp);
                        let _ = selector_resp;
                    });
                });
            });

        if !self.usage_panel_open_order.is_empty() {
            const USAGE_PANEL_STEP: f32 = 118.0;
            const USAGE_PANEL_RIGHT_MARGIN: f32 = 8.0;
            const USAGE_PANEL_BOTTOM_MARGIN: f32 = 42.0;
            let open_scopes = self.usage_panel_open_order.clone();
            for (idx, scope) in open_scopes.into_iter().enumerate() {
                egui::Area::new(egui::Id::new(("usage_panel_pinned", scope)))
                    .order(egui::Order::Foreground)
                    .anchor(
                        egui::Align2::RIGHT_BOTTOM,
                        Vec2::new(
                            -USAGE_PANEL_RIGHT_MARGIN,
                            -(USAGE_PANEL_BOTTOM_MARGIN + USAGE_PANEL_STEP * idx as f32),
                        ),
                    )
                    .show(ctx, |ui| {
                        egui::Frame::popup(ui.style())
                            .fill(p.popover_fill)
                            .stroke(Stroke::new(1.0, p.path_bar_border))
                            .inner_margin(Margin::symmetric(10, 8))
                            .show(ui, |ui| {
                                self.draw_usage_hover_panel(ui, p, scope);
                            });
                    });
            }
        }

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
                            // Right-clicking clears the template selection highlight.
                            self.equal_size_picker_selection = None;
                            self.equal_size_template_blink_terminal_id = None;
                            self.equal_size_template_blink_started_at = None;
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
                        egui::Popup::context_menu(&response)
                            .close_behavior(egui::PopupCloseBehavior::CloseOnClickOutside)
                            .show(|ui| new_terminal_context_menu(ui, self, target_terminal));
                        self.paint_spawn_flash(ui, area_rect.min, viewport, p);
                        let hint = ui.label(
                            RichText::new("Create a workspace tab to start terminals.")
                                .size(13.0)
                                .color(p.muted),
                        );
                        if hint.secondary_clicked() {
                            // Right-click clears the template selection highlight.
                            self.equal_size_picker_selection = None;
                            self.equal_size_template_blink_terminal_id = None;
                            self.equal_size_template_blink_started_at = None;
                            self.pending_context_terminal = None;
                            if let Some(pointer_pos) = hint.interact_pointer_pos() {
                                let local_pos = pointer_pos - area_rect.min.to_vec2();
                                self.pending_terminal_spawn_pos = Some(local_pos);
                                self.trigger_spawn_flash(local_pos);
                            }
                        }
                        egui::Popup::context_menu(&hint)
                            .close_behavior(egui::PopupCloseBehavior::CloseOnClickOutside)
                            .show(|ui| new_terminal_context_menu(ui, self, None));
                        self.terminal_area_size = viewport;
                        return;
                    };

                    let content_h = workspace_content_height(&runtime_ref.terminals, viewport.y);
                    let content_w = workspace_content_width(&runtime_ref.terminals, viewport.x);

                    egui::ScrollArea::both()
                        .id_salt(("termite_ws_scroll", self.selected_workspace))
                        .auto_shrink([false, false])
                        .show(ui, |ui| {
                            ui.set_min_size(Vec2::new(content_w, content_h));
                            self.terminal_area_size = Vec2::new(content_w, content_h);

                            let content_origin = ui.min_rect().min;
                            let content_rect = ui.max_rect();
                            // Column layout uses the *visible* workspace width. Using the scroll
                            // canvas width here grows stripes when a pane is wider than the screen
                            // (horizontal scroll), which breaks auto-fit / equal-size.
                            let canvas_width = content_rect.width();
                            let layout_width = viewport.x.max(1.0);
                            let scroll_bg = ui.interact(
                                content_rect,
                                ui.id().with(("ws_scroll_bg", self.selected_workspace)),
                                Sense::click(),
                            );
                            if scroll_bg.secondary_clicked() {
                                // Right-clicking clears the template selection highlight.
                                self.equal_size_picker_selection = None;
                                self.equal_size_template_blink_terminal_id = None;
                                self.equal_size_template_blink_started_at = None;
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
                            egui::Popup::context_menu(&scroll_bg)
                                .close_behavior(egui::PopupCloseBehavior::CloseOnClickOutside)
                                .show(|ui| new_terminal_context_menu(ui, self, target_terminal));
                            self.paint_spawn_flash(
                                ui,
                                content_origin,
                                Vec2::new(layout_width, content_h),
                                p,
                            );

                            let active_layout = self.active_panel_layout();
                            let spawn_flash_edges = spawn_flash_stripe_local_edges(
                                self.pending_spawn_flash_until,
                                self.pending_spawn_flash_pos,
                                layout_width,
                                content_h,
                                active_layout,
                            );
                            let layout = active_layout;
                            let (sync_terminals_to_columns, uniform_equal_terminals) = self
                                .active_workspace_tab()
                                .map(|t| (t.sync_terminals_to_columns, t.uniform_equal_terminals))
                                .unwrap_or((false, false));
                            let equal_size_picker_open = self.equal_size_picker_open;
                            let equal_size_template_blink_terminal_id =
                                self.equal_size_template_blink_terminal_id;
                            let equal_size_template_blink_started_at =
                                self.equal_size_template_blink_started_at;
                            let equal_size_template_blink_now = Instant::now();

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
                                egui::Popup::context_menu(&hint)
                                    .close_behavior(egui::PopupCloseBehavior::CloseOnClickOutside)
                                    .show(|ui| new_terminal_context_menu(ui, self, None));
                                return;
                            }

                            let mut close_idx: Option<usize> = None;
                            let mut clicked_on_pane = false;
                            let content_height = content_h;
                            // Equal grid must not use full `content_h`: `workspace_content_height` adds
                            // `2 * GRID_SPACING` below the deepest pane, so dividing that height makes the
                            // grid grow by that padding every frame (slow "animation"). Use body height only.
                            let uniform_grid_body_h = (content_h - 2.0 * GRID_SPACING)
                                .max(viewport.y)
                                .max(TERMINAL_MIN_HEIGHT);
                            let (_, _, n_cols) = column_slot_geometry(layout_width, layout);
                            let slot_w = column_stripe_width(layout_width, layout);

                            let ptr_up = !ui.input(|i| i.pointer.any_down());
                            if ptr_up {
                                if uniform_equal_terminals && !equal_size_picker_open {
                                    let template_size = runtime
                                        .equal_size_source_terminal_id
                                        .and_then(|source_id| {
                                            runtime
                                                .terminals
                                                .iter()
                                                .find(|pane| pane.id == source_id)
                                                .map(|pane| pane.desired_size)
                                        });
                                    reflow_panes_uniform_equal(
                                        &mut runtime.terminals,
                                        layout_width,
                                        uniform_grid_body_h,
                                        layout,
                                        template_size,
                                    );
                                } else if sync_terminals_to_columns {
                                    reflow_panes_to_column_starts(
                                        &mut runtime.terminals,
                                        layout_width,
                                        layout,
                                    );
                                }
                            }

                            // Next Y for stacking in each column (must use the pane *above*'s height, not row index).
                            let mut column_floor_y = vec![0.0_f32; n_cols];
                            for pane in runtime.terminals.iter() {
                                if let Some(pos) = pane.position {
                                    let col = pick_column_at_x(
                                        pos.x + pane.desired_size.x * 0.5,
                                        layout_width,
                                        layout,
                                    );
                                    if col < n_cols {
                                        let bottom = pos.y + pane.desired_size.y + STACK_GAP_Y;
                                        column_floor_y[col] = column_floor_y[col].max(bottom);
                                    }
                                }
                            }

                            let total_panes = runtime.terminals.len();
                            for idx in 0..total_panes {
                                let (left_group, right_group) = runtime.terminals.split_at_mut(idx);
                                let Some((pane, right_group)) = right_group.split_first_mut()
                                else {
                                    continue;
                                };

                                if pane.position.is_none() {
                                    // `slot_w` splits `layout_width` across `n` columns with gutters.
                                    pane.desired_size.x =
                                        slot_w.clamp(1.0, layout_width.max(1.0));
                                    let mut h =
                                        pane.desired_size.y.max(content_height.max(260.0));
                                    if let Some(rh) = layout.default_pane_height_hint(viewport.y) {
                                        h = h.max(rh);
                                    }
                                    pane.desired_size.y = h;
                                    let col = idx % n_cols.max(1);
                                    let x = column_band_left(layout_width, col, layout);
                                    let y = column_floor_y[col];
                                    pane.position = Some(Pos2::new(x, y));
                                    column_floor_y[col] = y + pane.desired_size.y + STACK_GAP_Y;
                                }

                                let mut pos = pane.position.unwrap_or(Pos2::ZERO);
                                // With horizontal scrolling the scroll area grows to fit all
                                // terminals, so we only clamp x to be non-negative. Previously
                                // clamping to the scroll canvas width caused right-side terminals to
                                // be pushed left and overlap neighbours when the window shrank.
                                // `content_height` is captured early in the frame. If a pane is
                                // spawned lower in this same frame, clamping to that stale value
                                // pulls it upward and can visually overlap neighbours.
                                let max_y = (content_height - pane.desired_size.y)
                                    .max(0.0)
                                    .max(pos.y);
                                pos.x = pos.x.max(0.0);
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
                                let drag_response = ui
                                    .interact(
                                        header_rect,
                                        ui.id().with(("pane_drag", pane.id)),
                                        Sense::click_and_drag(),
                                    )
                                    .on_hover_cursor(CursorIcon::Grab);
                                if drag_response.dragged() {
                                    ui.ctx().set_cursor_icon(CursorIcon::Grabbing);
                                }
                                if drag_response.dragged() {
                                    let delta = ui.input(|i| i.pointer.delta());
                                    pos.x = (pos.x + delta.x).max(0.0);
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
                                                    let nx = snap_x.max(0.0);
                                                    if best_x_snap
                                                        .is_none_or(|(best, _, _)| d_left < best)
                                                    {
                                                        best_x_snap = Some((d_left, nx, snap_x));
                                                    }
                                                }
                                                let d_right = ((pos.x + w) - snap_x).abs();
                                                if d_right <= RESIZE_SNAP_DISTANCE {
                                                    let nx = (snap_x - w).max(0.0);
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
                                        layout_width,
                                        spawn_flash_edges,
                                        pos,
                                        w,
                                        h,
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
                                                Pos2::new(content_origin.x + canvas_width, y),
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
                                    ui.ctx().set_cursor_icon(CursorIcon::ResizeNwSe);
                                } else if tr_active || bl_active {
                                    ui.ctx().set_cursor_icon(CursorIcon::ResizeNeSw);
                                } else if left_active || right_active {
                                    ui.ctx().set_cursor_icon(CursorIcon::ResizeHorizontal);
                                } else if top_active || bottom_active {
                                    ui.ctx().set_cursor_icon(CursorIcon::ResizeVertical);
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
                                        new_w = (new_w + delta.x).max(TERMINAL_MIN_WIDTH);
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
                                        layout_width,
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
                                            new_w = (snap_x - new_x).max(TERMINAL_MIN_WIDTH);
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
                                                Pos2::new(content_origin.x + canvas_width, y),
                                            ],
                                            Stroke::new(1.3, p.resize_grip_hot),
                                        );
                                    }
                                }

                                let is_active = runtime.active_terminal == Some(idx)
                                    || (runtime.active_terminal.is_none() && total_panes == 1);
                                let mut border = if is_active { p.terminal_border_active } else { p.border };
                                let mut stroke_w: f32 = 2.0;
                                if let (Some(blink_id), Some(started_at)) = (equal_size_template_blink_terminal_id, equal_size_template_blink_started_at) {
                                    if pane.id == blink_id && equal_size_picker_open {
                                        let elapsed = equal_size_template_blink_now.duration_since(started_at);
                                        // Keep blinking continuously while picker is open.
                                        stroke_w = 5.0;
                                        const BLINK_PERIOD_MS: u128 = 550;
                                        let phase_ms =
                                            (elapsed.as_millis() % BLINK_PERIOD_MS) as f32;
                                        let phase = phase_ms / (BLINK_PERIOD_MS as f32);
                                        // Smooth sine-based fade for a less "jittery" look.
                                        let intensity = 0.5 + 0.5 * (std::f32::consts::TAU * phase).sin(); // 0..1

                                        let base = p.border;
                                        let peak = p.terminal_border_active;
                                        let lerp_u8 = |a: u8, b: u8, t: f32| -> u8 {
                                            (a as f32 + (b as f32 - a as f32) * t)
                                                .round()
                                                .clamp(0.0, 255.0) as u8
                                        };
                                        border = Color32::from_rgba_unmultiplied(
                                            lerp_u8(base.r(), peak.r(), intensity),
                                            lerp_u8(base.g(), peak.g(), intensity),
                                            lerp_u8(base.b(), peak.b(), intensity),
                                            lerp_u8(base.a(), peak.a(), intensity),
                                        );
                                    }
                                }

                                let pane_response = ui.allocate_rect(pane_rect, Sense::click());
                                let mut clicked_cell_from_grid: Option<(usize, usize)> = None;
                                ui.scope_builder(
                                    egui::UiBuilder::new().max_rect(pane_rect),
                                    |ui| {
                                        egui::Frame::default()
                                            .fill(p.term_bg)
                                            .stroke(Stroke::new(stroke_w, border))
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
                                                let selection = runtime
                                                    .selections
                                                    .get_mut(idx)
                                                    .expect("selection slot should exist");
                                                let grid = pane.session.parser.grid();
                                                let show_caret = pane.session.parser.cursor_visible()
                                                    || pane.session.parser.app_cursor_keys()
                                                    || grid.in_alt;
                                                clicked_cell_from_grid = render_terminal_grid(
                                                    ui,
                                                    pane.id,
                                                    grid,
                                                    p,
                                                    selection,
                                                    is_active,
                                                    show_caret,
                                                );

                                                if let Some((clicked_row, clicked_col)) = clicked_cell_from_grid
                                                {
                                                    runtime.active_terminal = Some(idx);
                                                    clicked_on_pane = true;
                                                    let grid = pane.session.parser.grid();
                                                    if grid.cols > 0 {
                                                        let target_row = clicked_row
                                                            .min(grid.rows.saturating_sub(1));
                                                        let row_end = row_render_end(grid, target_row)
                                                            .min(grid.cols.saturating_sub(1));
                                                        let target_col = clicked_col
                                                            .min(row_end.saturating_add(1))
                                                            .min(grid.cols.saturating_sub(1));

                                                        let mut bytes = Vec::new();
                                                        // Horizontal targeting only (do not send Up/Down,
                                                        // which can trigger shell/TUI history navigation).
                                                        if clicked_col >= row_end {
                                                            // For readline-like prompts, this reliably lands at line end.
                                                            bytes.push(0x05); // Ctrl+E
                                                            if let Some(ed) = runtime.line_editors.get_mut(idx) {
                                                                ed.move_to_end();
                                                            }
                                                        } else if target_col > grid.cursor.col {
                                                            let steps = target_col - grid.cursor.col;
                                                            bytes.reserve(steps * 3);
                                                            for _ in 0..steps {
                                                                bytes.extend_from_slice(b"\x1b[C");
                                                            }
                                                            if let Some(ed) = runtime.line_editors.get_mut(idx) {
                                                                ed.move_cursor_delta(steps as isize);
                                                            }
                                                        } else if target_col < grid.cursor.col {
                                                            let steps = grid.cursor.col - target_col;
                                                            bytes.reserve(steps * 3);
                                                            for _ in 0..steps {
                                                                bytes.extend_from_slice(b"\x1b[D");
                                                            }
                                                            if let Some(ed) = runtime.line_editors.get_mut(idx) {
                                                                ed.move_cursor_delta(-(steps as isize));
                                                            }
                                                        }

                                                        if !bytes.is_empty() {
                                                            pane.backend.write_all(&bytes);
                                                        }
                                                    }
                                                }
                                            });
                                    },
                                );

                                if pane_response.clicked() {
                                    runtime.active_terminal = Some(idx);
                                    clicked_on_pane = true;
                                    // If user clicked inside pane but not on rendered text content,
                                    // treat it as "move caret to end of current input line".
                                    if clicked_cell_from_grid.is_none() {
                                        pane.backend.write_all(&[0x05]); // Ctrl+E
                                    }
                                    // Allow egui to forward `Event::Text` into the PTY.
                                    // Without this, we only see some Key events (e.g. delete/backspace).
                                }
                                if pane_response.secondary_clicked() {
                                    runtime.active_terminal = Some(idx);
                                    clicked_on_pane = true;
                                    // Allow egui to forward `Event::Text` into the PTY.
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

                            if scroll_bg.clicked() {
                                // Clear active terminal only when the click was truly outside all
                                // pane rectangles (border/title clicks can be ambiguous with the per-pane
                                // `clicked_on_pane` flag).
                                if let Some(pointer_pos) = scroll_bg.interact_pointer_pos() {
                                    let local_pos = Pos2::new(
                                        pointer_pos.x - content_origin.x,
                                        pointer_pos.y - content_origin.y,
                                    );
                                    let clicked_on_terminal = runtime.terminals.iter().any(|pane| {
                                        let pos = pane.position.unwrap_or(Pos2::ZERO);
                                        let rect = egui::Rect::from_min_size(pos, pane.desired_size);
                                        rect.contains(local_pos)
                                    });
                                    if !clicked_on_terminal {
                                        runtime.active_terminal = None;
                                    }
                                } else if !clicked_on_pane {
                                    runtime.active_terminal = None;
                                }
                            }

                            if let Some(idx) = close_idx {
                                let was_active = runtime.active_terminal == Some(idx);
                                let removed_id = runtime.terminals.get(idx).map(|pane| pane.id);
                                runtime.terminals.remove(idx);
                                runtime.selections.remove(idx);
                                if runtime
                                    .equal_size_source_terminal_id
                                    .is_some_and(|id| Some(id) == removed_id)
                                {
                                    runtime.equal_size_source_terminal_id =
                                        runtime.terminals.first().map(|pane| pane.id);
                                }
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

        self.draw_equal_size_picker(ctx, p);
        self.cleanup_stale_color_picker();
    }

    fn on_exit(&mut self, _gl: Option<&eframe::glow::Context>) {
        save_workspace_state(self);
    }
}

impl TermiteUi {
    fn trigger_equal_size_template_blink(&mut self, terminal_id: u64) {
        self.equal_size_template_blink_terminal_id = Some(terminal_id);
        self.equal_size_template_blink_started_at = Some(Instant::now());
    }

    fn refresh_system_status_if_due(&mut self) {
        if self.system_last_sample.elapsed() < SYSTEM_STATUS_SAMPLE_INTERVAL {
            return;
        }
        self.system_last_sample = Instant::now();
        self.system.refresh_cpu_usage();
        self.system.refresh_memory();
        self.system.refresh_processes_specifics(
            ProcessesToUpdate::All,
            true,
            ProcessRefreshKind::nothing().with_memory().with_cpu(),
        );
    }

    fn toggle_usage_panel(&mut self, termite_only: bool) {
        if let Some(existing_idx) = self
            .usage_panel_open_order
            .iter()
            .position(|scope| *scope == termite_only)
        {
            self.usage_panel_open_order.remove(existing_idx);
            return;
        }
        self.show_termite_only_status = termite_only;
        self.usage_panel_open_order.push(termite_only);
    }

    fn draw_usage_hover_panel(&mut self, ui: &mut egui::Ui, p: UiPalette, termite_only: bool) {
        ui.set_min_width(300.0);
        ui.label(
            RichText::new(if termite_only {
                "Termite process usage"
            } else {
                "System usage"
            })
            .size(12.0)
            .strong()
            .color(p.text),
        );
        ui.add_space(4.0);

        if termite_only {
            let Some(pid) = get_current_pid().ok() else {
                ui.label(RichText::new("Termite process unavailable").size(11.0).color(p.muted));
                return;
            };
            let Some(proc_) = self.system.process(pid) else {
                ui.label(RichText::new("Termite process unavailable").size(11.0).color(p.muted));
                return;
            };

            let total_mem = self.system.total_memory().max(1);
            let proc_cpu = proc_.cpu_usage().max(0.0);
            let proc_ram_ratio = (proc_.memory() as f32 / total_mem as f32).clamp(0.0, 1.0);

            usage_meter_row(
                ui,
                "CPU",
                (proc_cpu / 100.0).clamp(0.0, 1.0),
                format!("{proc_cpu:.1}%"),
                Color32::from_rgb(87, 156, 255),
                p,
            );
            usage_meter_row(
                ui,
                "RAM",
                proc_ram_ratio,
                format!("{} / {}", format_gib(proc_.memory()), format_gib(total_mem)),
                Color32::from_rgb(91, 189, 122),
                p,
            );
            ui.add_space(2.0);
            ui.label(
                RichText::new(format!("PID {}", proc_.pid().as_u32()))
                    .size(10.0)
                    .color(p.muted),
            );
            return;
        }

        let cpu = self.system.global_cpu_usage().max(0.0);
        let total_mem = self.system.total_memory().max(1);
        let used_mem = self.system.used_memory();
        let mem_ratio = (used_mem as f32 / total_mem as f32).clamp(0.0, 1.0);
        let total_swap = self.system.total_swap();
        let used_swap = self.system.used_swap();
        let swap_ratio = if total_swap > 0 {
            (used_swap as f32 / total_swap as f32).clamp(0.0, 1.0)
        } else {
            0.0
        };

        usage_meter_row(
            ui,
            "CPU",
            (cpu / 100.0).clamp(0.0, 1.0),
            format!("{cpu:.0}%"),
            Color32::from_rgb(87, 156, 255),
            p,
        );
        usage_meter_row(
            ui,
            "RAM",
            mem_ratio,
            format!("{} / {}", format_gib(used_mem), format_gib(total_mem)),
            Color32::from_rgb(91, 189, 122),
            p,
        );
        if total_swap > 0 {
            usage_meter_row(
                ui,
                "Swap",
                swap_ratio,
                format!("{} / {}", format_gib(used_swap), format_gib(total_swap)),
                Color32::from_rgb(234, 162, 86),
                p,
            );
        }
    }

    fn add_terminal(&mut self, spawn_pos: Option<Pos2>, anchor_terminal: Option<usize>) {
        self.ensure_workspace_runtime_slots();
        let layout = self.active_panel_layout();
        let selected_workspace = self.selected_workspace;
        let area_size = self.terminal_area_size;
        let viewport_w = self.terminal_workspace_viewport.x.max(1.0);
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
            let tmux_session = tmux_session_name(selected_workspace, next_terminal_id);
            let mut pane = spawn_terminal_pane(
                terminal_title,
                next_terminal_id,
                &working_dir,
                &tmux_session,
            );
            let stripe_w = column_stripe_width(viewport_w, layout);
            pane.desired_size.x = stripe_w.clamp(1.0, viewport_w.max(1.0));
            let mut default_h = pane
                .desired_size
                .y
                .min(viewport_h.max(TERMINAL_MIN_HEIGHT))
                .max(TERMINAL_MIN_HEIGHT);
            if let Some(h) = layout.default_pane_height_hint(viewport_h) {
                // Keep a stable default spawn size; do not upsize to full viewport height.
                default_h = default_h.min(h.max(TERMINAL_MIN_HEIGHT));
            }
            pane.desired_size.y = default_h;
            let position = if let Some(cursor_pos) = spawn_pos {
                let col = pick_spawn_column_preferring_empty_slot(
                    &runtime.terminals,
                    viewport_w,
                    cursor_pos.x,
                    layout,
                );
                let default_h = pane.desired_size.y;
                let first_top = min_y_topmost_in_column(
                    &runtime.terminals,
                    viewport_w,
                    col,
                    layout,
                );
                let cap = match first_top {
                    Some(y) if y > STACK_GAP_Y => y - STACK_GAP_Y,
                    Some(_) => content_bounds,
                    None => content_bounds,
                };
                // Allow context-menu spawns to search below current content so a "full" column can
                // still append at the bottom instead of being forced back near the top.
                let spawn_search_area = Vec2::new(
                    area_for_placement.x,
                    area_for_placement.y + default_h + STACK_GAP_Y,
                );
                let preferred_y = cursor_pos
                    .y
                    .clamp(0.0, (spawn_search_area.y - default_h).max(0.0));
                let preferred_max_h = default_h.min(cap.max(1.0));
                let (pos, spawn_size) = find_spawn_column_no_overlap(
                    &runtime.terminals,
                    spawn_search_area,
                    viewport_w,
                    col,
                    preferred_y,
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
                        viewport_w,
                        layout,
                    );
                    let preferred_y =
                        (pos.y + 24.0).min((area_for_placement.y - pane.desired_size.y).max(0.0));
                    find_non_overlapping_position_in_column(
                        &runtime.terminals,
                        Vec2::new(viewport_w, area_for_placement.y),
                        pane.desired_size,
                        col,
                        preferred_y,
                        layout,
                    )
                } else {
                    let cols = layout.column_count(viewport_w).max(1);
                    let col = runtime.terminals.len() % cols;
                    find_non_overlapping_position_in_column(
                        &runtime.terminals,
                        Vec2::new(viewport_w, area_for_placement.y),
                        pane.desired_size,
                        col,
                        0.0,
                        layout,
                    )
                }
            } else {
                let cols = layout.column_count(viewport_w).max(1);
                let col = runtime.terminals.len() % cols;
                find_non_overlapping_position_in_column(
                    &runtime.terminals,
                    Vec2::new(viewport_w, area_for_placement.y),
                    pane.desired_size,
                    col,
                    0.0,
                    layout,
                )
            };
            pane.position = Some(position);
            runtime.terminals.push(pane);
            runtime.selections.push(None);
            runtime.line_editors.push(LineEditor::new());
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
        if runtime.selections.len() < runtime.terminals.len() {
            runtime.selections.resize(runtime.terminals.len(), None);
        } else if runtime.selections.len() > runtime.terminals.len() {
            runtime.selections.truncate(runtime.terminals.len());
        }
        if runtime.line_editors.len() < runtime.terminals.len() {
            runtime.line_editors.resize_with(runtime.terminals.len(), LineEditor::new);
        } else if runtime.line_editors.len() > runtime.terminals.len() {
            runtime.line_editors.truncate(runtime.terminals.len());
        }
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
                    // Skip text generated by modifier shortcuts (e.g. Cmd+Z firing a
                    // stray "z" Text event after the Key event already handled undo).
                    let mods = ctx.input(|i| i.modifiers);
                    if mods.command || mods.ctrl {
                        continue;
                    }
                    if !text.is_empty() {
                        runtime.line_editors[active_idx].push_text(&text);
                        runtime.terminals[active_idx].backend.write_all(text.as_bytes());
                    }
                }
                egui::Event::Paste(text) => {
                    if !text.is_empty() {
                        runtime.line_editors[active_idx].reset();
                        let bytes = clipboard::clipboard_text_to_pty_bytes(&text);
                        runtime.terminals[active_idx].backend.write_all(&bytes);
                    }
                    runtime.selections[active_idx] = None;
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

                    let cmd = modifiers.command;
                    let ctrl = modifiers.ctrl;
                    let shift = modifiers.shift;

                    // ── Undo / redo typed text ────────────────────────────────
                    if cmd && !shift && key == egui::Key::Z {
                        let ed = &mut runtime.line_editors[active_idx];
                        let total_chars = ed.current.text.chars().count();
                        if let Some(state) = ed.undo() {
                            let bytes = build_restore_bytes(total_chars, &state);
                            runtime.terminals[active_idx].backend.write_all(&bytes);
                        }
                        continue;
                    }
                    if cmd && shift && key == egui::Key::Z {
                        let ed = &mut runtime.line_editors[active_idx];
                        let total_chars = ed.current.text.chars().count();
                        if let Some(state) = ed.redo() {
                            let bytes = build_restore_bytes(total_chars, &state);
                            runtime.terminals[active_idx].backend.write_all(&bytes);
                        }
                        continue;
                    }

                    // macOS-like shortcuts:
                    // - Cmd/Ctrl+Shift+A = Select all
                    // - Cmd/Ctrl+Shift+C = Copy (rich text via ANSI SGR)
                    // - Cmd/Ctrl+Shift+V = Paste
                    // - Shift+Insert     = Paste
                    let is_select_all = (cmd && key == egui::Key::A)
                        || (cmd && shift && key == egui::Key::A)
                        || (ctrl && shift && key == egui::Key::A);
                    if is_select_all {
                        let grid = runtime.terminals[active_idx].session.parser.grid();
                        if grid.rows > 0 && grid.cols > 0 {
                            runtime.selections[active_idx] = Some(SelectionRange {
                                start_row: 0,
                                start_col: 0,
                                end_row: grid.rows - 1,
                                end_col: grid.cols - 1,
                                active: true,
                            });
                        }
                        continue;
                    }

                    let is_copy =
                        (cmd && key == egui::Key::C) || (ctrl && shift && key == egui::Key::C);
                    if is_copy {
                        if let Some(range) =
                            runtime.selections.get(active_idx).copied().unwrap_or(None)
                        {
                            let grid =
                                runtime.terminals[active_idx].session.parser.grid();
                            let text = clipboard::selection_to_ansi_sgr_text(grid, range);
                            if let Err(_e) = clipboard::set_clipboard_text(&text) {}
                        }
                        // Even without an internal rich selection, don't forward the
                        // keystroke into the PTY (Cmd/C is typically used for copy).
                        continue;
                    }

                    if matches!(key, egui::Key::Backspace | egui::Key::Delete) {
                        if let Some(range) =
                            runtime.selections.get(active_idx).copied().unwrap_or(None)
                        {
                            let grid = runtime.terminals[active_idx].session.parser.grid();
                            let bytes = selection_delete_bytes(grid, range, key);
                            if !bytes.is_empty() {
                                runtime.terminals[active_idx].backend.write_all(&bytes);
                            }
                            runtime.selections[active_idx] = None;
                            continue;
                        }
                        // Track single backspace in line editor.
                        if key == egui::Key::Backspace {
                            runtime.line_editors[active_idx].push_backspace();
                        }
                    }

                    // Update cursor offset for navigation keys so mid-line insertions
                    // are tracked accurately; reset only when the line context is lost.
                    let ed = &mut runtime.line_editors[active_idx];
                    match key {
                        egui::Key::ArrowLeft  => ed.move_left(),
                        egui::Key::ArrowRight => ed.move_right(),
                        egui::Key::Home       => ed.move_to_start(),
                        egui::Key::End        => ed.move_to_end(),
                        egui::Key::Enter
                        | egui::Key::ArrowUp
                        | egui::Key::ArrowDown
                        | egui::Key::PageUp
                        | egui::Key::PageDown
                        | egui::Key::Escape   => ed.reset(),
                        _ if ctrl && key == egui::Key::C => ed.reset(),
                        _ if ctrl && key == egui::Key::U => ed.reset(),
                        _ if ctrl && key == egui::Key::W => ed.reset(),
                        _ if ctrl && key == egui::Key::A => ed.move_to_start(),
                        _ if ctrl && key == egui::Key::E => ed.move_to_end(),
                        _ => {}
                    }

                    if let Some(bytes) =
                        key_to_ansi_bytes(key, shift, modifiers.ctrl)
                    {
                        runtime.terminals[active_idx].backend.write_all(&bytes);
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
            .backend
            .write_all(format!("{command}\n").as_bytes());
    }

}

fn is_cli_command_available(command: &str) -> bool {
    if !command
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
    {
        return false;
    }

    Command::new("sh")
        .arg("-lc")
        .arg(format!("command -v {command} >/dev/null 2>&1"))
        .status()
        .is_ok_and(|status| status.success())
}

fn read_frame_tcp(stream: &mut TcpStream) -> std::io::Result<(u8, Vec<u8>)> {
    let mut header = [0u8; 5];
    stream.read_exact(&mut header)?;
    let frame_type = header[0];
    let len = u32::from_le_bytes(header[1..5].try_into().expect("len")) as usize;
    let mut payload = vec![0u8; len];
    if len > 0 {
        stream.read_exact(&mut payload)?;
    }
    Ok((frame_type, payload))
}

fn write_frame_tcp(stream: &mut TcpStream, frame_type: u8, payload: &[u8]) -> std::io::Result<()> {
    let len = payload.len() as u32;
    let mut header = [0u8; 5];
    header[0] = frame_type;
    header[1..5].copy_from_slice(&len.to_le_bytes());
    stream.write_all(&header)?;
    if !payload.is_empty() {
        stream.write_all(payload)?;
    }
    Ok(())
}

fn connect_daemon() -> Option<TcpStream> {
    if std::env::var("TERMITE_DAEMON_DISABLED").ok().as_deref() == Some("1") {
        return None;
    }

    for _attempt in 0..30 {
        if let Ok(port_file) = daemon::daemon_port_file_path() {
            if let Ok(port_s) = fs::read_to_string(&port_file) {
                if let Ok(port) = port_s.trim().parse::<u16>() {
                    if let Ok(stream) = TcpStream::connect(("127.0.0.1", port)) {
                        return Some(stream);
                    }
                }
            }
        }

        // Start daemon once if it isn't running yet.
        if _attempt == 0 {
            let exe = std::env::current_exe().ok()?;
            let _ = Command::new(exe)
                .arg("--daemon")
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .spawn();
        }

        std::thread::sleep(Duration::from_millis(100));
    }

    None
}

fn spawn_terminal_pane(
    title: String,
    next_terminal_id: u64,
    working_dir: &str,
    tmux_session: &str,
) -> TerminalPane {
    let (tx, rx) = unbounded::<Vec<u8>>();

    // Prefer the built-in daemon so terminal state (e.g. a running Claude session)
    // survives closing/reopening this UI.
    if let Some(mut stream) = connect_daemon() {
        if let Ok(mut reader) = stream.try_clone() {
            // Attach request payload:
            // [u16 key_len][key bytes][u16 rows][u16 cols]
            let key_bytes = tmux_session.as_bytes();
            if key_bytes.len() <= u16::MAX as usize {
                let mut payload = Vec::with_capacity(2 + key_bytes.len() + 2 + 2);
                payload.extend_from_slice(&(key_bytes.len() as u16).to_le_bytes());
                payload.extend_from_slice(key_bytes);
                payload.extend_from_slice(&(24u16).to_le_bytes());
                payload.extend_from_slice(&(80u16).to_le_bytes());

                if write_frame_tcp(&mut stream, FRAME_ATTACH, &payload).is_ok() {
                    if let Ok((ft, first_payload)) = read_frame_tcp(&mut reader) {
                        if ft == FRAME_OUTPUT {
                            let _ = tx.send(first_payload);

                            let writer = Arc::new(Mutex::new(stream));
                            let tx_thread = tx;

                            std::thread::spawn(move || {
                                loop {
                                    let Ok((ft, payload)) = read_frame_tcp(&mut reader) else {
                                        break;
                                    };
                                    if ft == FRAME_OUTPUT {
                                        let _ = tx_thread.send(payload);
                                    } else if ft == FRAME_ATTACH_ERROR {
                                        break;
                                    }
                                }
                            });

                            return TerminalPane {
                                id: next_terminal_id,
                                title,
                                tmux_session: tmux_session.to_string(),
                                session: TerminalSession::new(PaneId::new(), 24, 80, rx),
                                backend: TerminalBackend::DaemonPty { writer },
                                desired_size: Vec2::new(520.0, 280.0),
                                position: None,
                            };
                        }
                    }
                }
            }
        }
    }

    // Fallback: local PTY (no persistence across restarts).
    let shell = std::env::var("SHELL").unwrap_or_else(|_| "/bin/zsh".to_string());
    let wake_up: Box<dyn Fn() + Send + 'static> = Box::new(|| {});
    let pty = spawn_pty(
        &shell,
        24,
        80,
        tx,
        wake_up,
        Some(working_dir),
        None,
    )
    .expect("spawn terminal pty");

    TerminalPane {
        id: next_terminal_id,
        title,
        tmux_session: tmux_session.to_string(),
        session: TerminalSession::new(PaneId::new(), 24, 80, rx),
        backend: TerminalBackend::LocalPty { pty },
        desired_size: Vec2::new(520.0, 280.0),
        position: None,
    }
}

fn tmux_session_name(workspace_idx: usize, terminal_id: u64) -> String {
    format!("termite-w{}-t{}", workspace_idx + 1, terminal_id)
}

fn resize_terminal_for_size(pane: &mut TerminalPane, size: Vec2) {
    let cols = (size.x / CELL_W).max(1.0) as usize;
    let rows = (size.y / CELL_H).max(1.0) as usize;
    pane.session.parser.resize(rows, cols);
    pane.backend.resize(rows as u16, cols as u16);
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

    // Under reverse, a Default source has to resolve concretely: the displayed fg
    // becomes the terminal bg, and the displayed bg becomes the default fg color.
    // Otherwise reverse-on-default renders as transparent and is invisible (e.g.
    // Claude Code's `\e[7m \e[27m` cursor block).
    let mut fg = match (reverse, fg_src) {
        (true, Color::Default) => term_bg,
        _ => color32_from_vt_color(fg_src, true, vt_default_fg),
    };
    let bg_opaque = match (reverse, bg_src) {
        (true, Color::Default) => vt_default_fg,
        _ => color32_from_vt_color(bg_src, false, vt_default_fg),
    };
    let background = if reverse {
        bg_opaque
    } else if matches!(bg_src, Color::Default) {
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
    // Styling that makes an otherwise-blank space visually non-trivial
    // (reverse swaps fg/bg, underline/strikethrough draw strokes). Trimming such
    // cells hides TUI cursors that render as `\e[7m \e[27m` on a default bg.
    let visible_space_attrs =
        CellAttrs::REVERSE | CellAttrs::UNDERLINE | CellAttrs::STRIKETHROUGH;

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
        if cell.bg != Color::Default {
            break;
        }
        if cell.attrs.intersects(visible_space_attrs) {
            break;
        }
        end -= 1;
    }
    end
}

fn render_terminal_grid(
    ui: &mut egui::Ui,
    pane_id: u64,
    grid: &TerminalGrid,
    p: UiPalette,
    selection: &mut Option<SelectionRange>,
    is_focused_terminal: bool,
    _show_caret: bool,
) -> Option<(usize, usize)> {
    let font_id = FontId::monospace(12.0);
    let newline_fmt = TextFormat {
        font_id: font_id.clone(),
        color: p.vt_default_fg,
        background: Color32::TRANSPARENT,
        ..Default::default()
    };

    // Bake the block cursor directly into the LayoutJob so it is pixel-perfectly
    // aligned with the glyph grid (avoids painter-overlay Y drift).
    let blink_visible = ui
        .ctx()
        .input(|i| ((i.time / 0.5).floor() as i64).rem_euclid(2) == 0);
    // Some apps (including Claude Code) can report cursor visibility in ways
    // that don't map cleanly to VT cursor state, so keep blink tied to focus.
    let show_block_cursor = is_focused_terminal && blink_visible;
    let cursor_row = grid.cursor.row.min(grid.rows.saturating_sub(1));
    let cursor_col = grid.cursor.col.min(grid.cols.saturating_sub(1));

    let mut job = LayoutJob::default();
    for row in 0..grid.rows {
        let mut trim_end = row_render_end(grid, row);
        // Extend the rendered region to cover the cursor cell so the block
        // cursor is visible even when it sits on a trailing space.
        if show_block_cursor && row == cursor_row && cursor_col < grid.cols {
            trim_end = trim_end.max(cursor_col + 1);
        }
        let mut col = 0;
        while col < trim_end {
            let cell = grid.cell(row, col);
            if cell.wide == WideKind::Trailing {
                col += 1;
                continue;
            }

            let mut fmt = cell_text_format(cell, font_id.clone(), p.term_bg, p.vt_default_fg);
            let is_selected = selection
                .map_or(false, |sel| sel.contains(row, col, grid.rows, grid.cols));
            if is_selected {
                // Invert the displayed fg/bg to highlight selection.
                let normal_fg = fmt.color;
                let normal_bg = if fmt.background == Color32::TRANSPARENT {
                    p.term_bg
                } else {
                    fmt.background
                };
                fmt.color = normal_bg;
                fmt.background = normal_fg;
            }
            // Some TUIs render the cursor by toggling reverse-video on the
            // cursor cell itself. When blink is in the "off" phase, undo that
            // cursor-cell reverse so the caret actually disappears.
            if is_focused_terminal
                && !show_block_cursor
                && row == cursor_row
                && col == cursor_col
                && cell.attrs.contains(CellAttrs::REVERSE)
            {
                let effective_bg = if fmt.background == Color32::TRANSPARENT {
                    p.term_bg
                } else {
                    fmt.background
                };
                let effective_fg = fmt.color;
                fmt.color = effective_bg;
                fmt.background = effective_fg;
            }
            // Block cursor: invert this cell's colors so the cursor is a
            // filled rectangle aligned with the rest of the glyph grid.
            if show_block_cursor && row == cursor_row && col == cursor_col {
                let effective_bg = if fmt.background == Color32::TRANSPARENT {
                    p.term_bg
                } else {
                    fmt.background
                };
                fmt.background = fmt.color;
                fmt.color = effective_bg;
            }
            let mut chunk = String::new();
            chunk.push(cell.ch);

            let mut next = col + 1;
            if cell.wide == WideKind::Leading {
                next = col + 2;
            }

            while next < trim_end {
                // Never merge the cursor cell into a preceding run so the
                // block-cursor format is applied to exactly one segment.
                if show_block_cursor && row == cursor_row && next == cursor_col {
                    break;
                }
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

    let mut clicked_cell: Option<(usize, usize)> = None;
    egui::ScrollArea::both()
        .id_salt(("term-scroll", pane_id))
        .auto_shrink([false, false])
        .show(ui, |ui| {
            let row_h = ui.fonts_mut(|f| f.row_height(&font_id)).max(1.0);
            let glyph_w = ui.fonts_mut(|f| f.glyph_width(&font_id, 'W')).max(1.0);
            let response = ui
                .add(
                    egui::Label::new(job)
                        .selectable(true)
                        .sense(Sense::click_and_drag())
                        .wrap_mode(egui::TextWrapMode::Extend),
                )
                .on_hover_cursor(CursorIcon::Text);
            if response.hovered() {
                ui.ctx().set_cursor_icon(CursorIcon::Text);
            }

            let pointer_to_cell = |pointer: Pos2| -> (usize, usize) {
                let local = pointer - response.rect.min;
                let row = (local.y / row_h)
                    .floor()
                    .max(0.0)
                    .min(grid.rows.saturating_sub(1) as f32) as usize;
                let col = (local.x / glyph_w)
                    .floor()
                    .max(0.0)
                    .min(grid.cols.saturating_sub(1) as f32) as usize;
                (row, col)
            };

            if response.clicked() {
                if let Some(pointer) = response.interact_pointer_pos() {
                    let (row, col) = pointer_to_cell(pointer);
                    clicked_cell = Some((row, col));
                }
            }

            if response.drag_started() {
                if let Some(pointer) = response.interact_pointer_pos() {
                    let (row, col) = pointer_to_cell(pointer);
                    *selection = Some(SelectionRange {
                        start_row: row,
                        start_col: col,
                        end_row: row,
                        end_col: col,
                        active: true,
                    });
                }
            } else if response.dragged() {
                if let (Some(pointer), Some(range)) = (response.interact_pointer_pos(), selection.as_mut())
                {
                    let (row, col) = pointer_to_cell(pointer);
                    range.end_row = row;
                    range.end_col = col;
                    range.active = true;
                }
            }

            // Fallback cursor overlay: guarantees a visible caret even when the
            // current cell is a trailing space run that the text layout may trim.
            if show_block_cursor && grid.rows > 0 && grid.cols > 0 {
                let caret_row = cursor_row.min(grid.rows.saturating_sub(1));
                let caret_col = cursor_col.min(grid.cols.saturating_sub(1));
                let caret_x = response.rect.min.x + caret_col as f32 * glyph_w;
                let caret_y = response.rect.min.y + caret_row as f32 * row_h;
                let caret_rect = egui::Rect::from_min_size(
                    Pos2::new(caret_x, caret_y),
                    Vec2::new(glyph_w.clamp(6.0, 12.0), row_h.max(10.0)),
                );
                ui.painter().rect_filled(caret_rect, 0.0, Color32::WHITE);
                ui.painter().rect_stroke(
                    caret_rect,
                    0.0,
                    Stroke::new(1.0, Color32::BLACK),
                    egui::StrokeKind::Outside,
                );
                // Keep the caret in view for TUIs that place the prompt near the bottom.
                ui.scroll_to_rect(caret_rect, Some(egui::Align::Center));
            }
        });
    clicked_cell
}

/// Build the PTY bytes that restore a `LineState` snapshot.
///
/// Protocol:
///   1. `\x05` (Ctrl+E) — move to end of line regardless of current cursor pos.
///   2. `\x7f` × `total_chars` — backspace the entire line.
///   3. Type `state.text` — the restored content.
///   4. `\x1b[D` × N — move cursor left to `state.cursor` within the new text.
///
/// Using Ctrl+E instead of tracking cursor_offset eliminates the drift that
/// caused undo after a mouse-click to corrupt the line.
fn build_restore_bytes(total_chars: usize, state: &LineState) -> Vec<u8> {
    let text_char_len = state.text.chars().count();
    let steps_left = text_char_len.saturating_sub(state.cursor);
    let mut bytes = Vec::with_capacity(1 + total_chars + state.text.len() + steps_left * 3);
    bytes.push(0x05); // Ctrl+E: move to end of line
    bytes.extend(std::iter::repeat(0x7fu8).take(total_chars));
    bytes.extend_from_slice(state.text.as_bytes());
    for _ in 0..steps_left {
        bytes.extend_from_slice(b"\x1b[D"); // cursor left × N to reach stored position
    }
    bytes
}

fn key_to_ansi_bytes(key: egui::Key, shift: bool, ctrl: bool) -> Option<Vec<u8>> {
    match key {
        egui::Key::Enter => Some(vec![b'\r']),
        egui::Key::Tab => Some(if shift {
            vec![0x1b, b'[', b'Z']
        } else {
            vec![b'\t']
        }),
        egui::Key::Backspace => Some(vec![0x7f]),
        egui::Key::Escape => Some(vec![0x1b]),
        egui::Key::Insert => Some(b"\x1b[2~".to_vec()),
        // Printable characters (including space) are delivered via `egui::Event::Text`.
        // Avoid emitting them from `Event::Key` to prevent double-insertion.
        egui::Key::Space => None,

        // Ctrl+letter -> control codes.
        // (Used when egui doesn't deliver `Event::Text`.)
        k @ egui::Key::A
        | k @ egui::Key::B
        | k @ egui::Key::C
        | k @ egui::Key::D
        | k @ egui::Key::E
        | k @ egui::Key::F
        | k @ egui::Key::G
        | k @ egui::Key::H
        | k @ egui::Key::I
        | k @ egui::Key::J
        | k @ egui::Key::K
        | k @ egui::Key::L
        | k @ egui::Key::M
        | k @ egui::Key::N
        | k @ egui::Key::O
        | k @ egui::Key::P
        | k @ egui::Key::Q
        | k @ egui::Key::R
        | k @ egui::Key::S
        | k @ egui::Key::T
        | k @ egui::Key::U
        | k @ egui::Key::V
        | k @ egui::Key::W
        | k @ egui::Key::X
        | k @ egui::Key::Y
        | k @ egui::Key::Z => {
            // Avoid double-insertion: letters are delivered via `egui::Event::Text`.
            // Here we only handle Ctrl+letter as control codes.
            if !ctrl {
                return None;
            }

            // ctrl pressed: encode control code.
            let upper = match k {
                egui::Key::A => b'A',
                egui::Key::B => b'B',
                egui::Key::C => b'C',
                egui::Key::D => b'D',
                egui::Key::E => b'E',
                egui::Key::F => b'F',
                egui::Key::G => b'G',
                egui::Key::H => b'H',
                egui::Key::I => b'I',
                egui::Key::J => b'J',
                egui::Key::K => b'K',
                egui::Key::L => b'L',
                egui::Key::M => b'M',
                egui::Key::N => b'N',
                egui::Key::O => b'O',
                egui::Key::P => b'P',
                egui::Key::Q => b'Q',
                egui::Key::R => b'R',
                egui::Key::S => b'S',
                egui::Key::T => b'T',
                egui::Key::U => b'U',
                egui::Key::V => b'V',
                egui::Key::W => b'W',
                egui::Key::X => b'X',
                egui::Key::Y => b'Y',
                egui::Key::Z => b'Z',
                _ => 0,
            };
            if upper == 0 {
                return None;
            }
            // A -> 0x01 ... Z -> 0x1A
            let code = (upper - b'A' + 1) & 0x1F;
            Some(vec![code])
        }

        // Digits (and shifted symbols).
        d @ egui::Key::Num0
        | d @ egui::Key::Num1
        | d @ egui::Key::Num2
        | d @ egui::Key::Num3
        | d @ egui::Key::Num4
        | d @ egui::Key::Num5
        | d @ egui::Key::Num6
        | d @ egui::Key::Num7
        | d @ egui::Key::Num8
        | d @ egui::Key::Num9 => {
            // Digits are delivered via `egui::Event::Text`; avoid duplicating.
            let _ = (d, shift, ctrl);
            None
        }

        egui::Key::ArrowUp => Some(b"\x1b[A".to_vec()),
        egui::Key::ArrowDown => Some(b"\x1b[B".to_vec()),
        egui::Key::ArrowRight => Some(b"\x1b[C".to_vec()),
        egui::Key::ArrowLeft => Some(b"\x1b[D".to_vec()),
        egui::Key::Home => Some(b"\x1b[H".to_vec()),
        egui::Key::End => Some(b"\x1b[F".to_vec()),
        egui::Key::Delete => Some(b"\x1b[3~".to_vec()),
        egui::Key::PageUp => Some(b"\x1b[5~".to_vec()),
        egui::Key::PageDown => Some(b"\x1b[6~".to_vec()),

        // Printable punctuation is delivered via `egui::Event::Text`.
        // Keep `Event::Key` focused on control/navigation keys to avoid double insertion.

        _ => None,
    }
}

fn selection_delete_bytes(grid: &TerminalGrid, mut range: SelectionRange, key: egui::Key) -> Vec<u8> {
    if !range.active || grid.rows == 0 || grid.cols == 0 {
        return key_to_ansi_bytes(key, false, false).unwrap_or_default();
    }

    range.clamp_to_grid(grid.rows, grid.cols);
    let ((start_row, start_col), (end_row, end_col)) = range.normalized_start_end();
    let selected_len = if start_row == end_row {
        let mut count = 0usize;
        for col in start_col..=end_col {
            let cell = grid.cell(start_row, col);
            if cell.wide != WideKind::Trailing {
                count += 1;
            }
        }
        count
    } else {
        0
    };

    // Reliable deletion for line-editor prompts: normalize cursor to the selected span
    // and emit delete/backspace the right number of times.
    if selected_len > 0 && start_row == grid.cursor.row {
        let mut bytes = Vec::new();
        match key {
            egui::Key::Backspace => {
                let target_col_after_selection = end_col.saturating_add(1);
                if grid.cursor.col > target_col_after_selection {
                    for _ in 0..(grid.cursor.col - target_col_after_selection) {
                        bytes.extend_from_slice(b"\x1b[D");
                    }
                } else if grid.cursor.col < target_col_after_selection {
                    for _ in 0..(target_col_after_selection - grid.cursor.col) {
                        bytes.extend_from_slice(b"\x1b[C");
                    }
                }
                for _ in 0..selected_len {
                    bytes.push(0x7f);
                }
            }
            egui::Key::Delete => {
                if grid.cursor.col > start_col {
                    for _ in 0..(grid.cursor.col - start_col) {
                        bytes.extend_from_slice(b"\x1b[D");
                    }
                } else if grid.cursor.col < start_col {
                    for _ in 0..(start_col - grid.cursor.col) {
                        bytes.extend_from_slice(b"\x1b[C");
                    }
                }
                for _ in 0..selected_len {
                    bytes.extend_from_slice(b"\x1b[3~");
                }
            }
            _ => {}
        }
        if !bytes.is_empty() {
            return bytes;
        }
    }

    key_to_ansi_bytes(key, false, false).unwrap_or_default()
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
                let inherit_layout = app
                    .workspaces
                    .get(app.selected_workspace)
                    .map(|w| w.panel_layout)
                    .unwrap_or_default();
                app.next_workspace_index += 1;
                app.workspaces.push(WorkspaceTab {
                    title,
                    badge: None,
                    color_rgba: None,
                    working_dir: inherit_dir,
                    panel_layout: inherit_layout,
                    sync_terminals_to_columns: app
                        .workspaces
                        .get(app.selected_workspace)
                        .map(|w| w.sync_terminals_to_columns)
                        .unwrap_or(false),
                    uniform_equal_terminals: app
                        .workspaces
                        .get(app.selected_workspace)
                        .map(|w| w.uniform_equal_terminals)
                        .unwrap_or(false),
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
        sync_terminals_to_columns: None,
        uniform_equal_terminals: None,
        selected_workspace: app.selected_workspace,
        workspaces: app
            .workspaces
            .iter()
            .enumerate()
            .map(|(idx, tab)| WorkspaceTabState {
                title: tab.title.clone(),
                badge: tab.badge,
                color_rgba: tab.color_rgba,
                panel_layout: tab.panel_layout.sanitized(),
                sync_terminals_to_columns: Some(tab.sync_terminals_to_columns),
                uniform_equal_terminals: Some(tab.uniform_equal_terminals),
                working_dir: Some(tab.working_dir.clone()),
                terminal_sessions: app
                    .workspace_runtime
                    .get(idx)
                    .map(|runtime| {
                        runtime
                            .terminals
                            .iter()
                            .map(|pane| TerminalPaneState {
                                id: pane.id,
                                title: pane.title.clone(),
                                tmux_session: Some(pane.tmux_session.clone()),
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
                equal_size_source_terminal_id: app
                    .workspace_runtime
                    .get(idx)
                    .and_then(|runtime| runtime.equal_size_source_terminal_id),
            })
            .collect(),
        next_workspace_index: app.next_workspace_index,
        color_history: app.color_history.clone(),
        usage_panel_pinned_scope: app.usage_panel_open_order.last().copied(),
        usage_panel_open_order: app.usage_panel_open_order.clone(),
        show_termite_only_status: app.show_termite_only_status,
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

    /// Keep `selections` / `line_editors` aligned with `terminals` for every workspace.
    ///
    /// `handle_keyboard_input` only touches the active workspace, but the UI can switch
    /// workspaces in the same frame *after* keyboard handling runs; painting then assumed
    /// `selections[idx]` exists and panicked. Restoring persisted terminals also assigned
    /// `terminals` without growing these sidecar vectors until that workspace became active.
    fn sync_workspace_runtime_buffers(runtime: &mut WorkspaceRuntime) {
        if runtime.selections.len() < runtime.terminals.len() {
            runtime.selections.resize(runtime.terminals.len(), None);
        } else if runtime.selections.len() > runtime.terminals.len() {
            runtime.selections.truncate(runtime.terminals.len());
        }
        if runtime.line_editors.len() < runtime.terminals.len() {
            runtime
                .line_editors
                .resize_with(runtime.terminals.len(), LineEditor::new);
        } else if runtime.line_editors.len() > runtime.terminals.len() {
            runtime.line_editors.truncate(runtime.terminals.len());
        }
    }

    fn sync_all_workspace_runtime_buffers(&mut self) {
        self.ensure_workspace_runtime_slots();
        for runtime in &mut self.workspace_runtime {
            Self::sync_workspace_runtime_buffers(runtime);
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

    fn active_workspace_tab_mut(&mut self) -> Option<&mut WorkspaceTab> {
        if self.workspaces.is_empty() {
            return None;
        }
        let idx = self
            .selected_workspace
            .min(self.workspaces.len().saturating_sub(1));
        self.workspaces.get_mut(idx)
    }

    fn active_panel_layout(&self) -> PanelLayout {
        if self.workspaces.is_empty() {
            return PanelLayout::default();
        }
        let idx = self
            .selected_workspace
            .min(self.workspaces.len().saturating_sub(1));
        self.workspaces
            .get(idx)
            .map(|w| w.panel_layout.sanitized())
            .unwrap_or_default()
    }

    fn active_workspace_tab(&self) -> Option<&WorkspaceTab> {
        if self.workspaces.is_empty() {
            return None;
        }
        let idx = self
            .selected_workspace
            .min(self.workspaces.len().saturating_sub(1));
        self.workspaces.get(idx)
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

    fn open_equal_size_picker_for_active_workspace(&mut self) {
        let Some(runtime) = self.active_workspace_runtime_mut() else {
            self.equal_size_picker_open = false;
            self.equal_size_picker_selection = None;
            return;
        };
        let fallback = runtime
            .active_terminal
            .and_then(|idx| runtime.terminals.get(idx).map(|pane| pane.id))
            .or_else(|| runtime.terminals.first().map(|pane| pane.id));
        let current = runtime
            .equal_size_source_terminal_id
            .or(fallback);
        self.equal_size_picker_selection = current;
        self.equal_size_picker_open = current.is_some();
        if let Some(id) = current {
            self.trigger_equal_size_template_blink(id);
        }
    }

    fn draw_equal_size_picker(&mut self, ctx: &egui::Context, p: UiPalette) {
        let uniform_equal_terminals = self
            .active_workspace_tab()
            .map(|t| t.uniform_equal_terminals)
            .unwrap_or(false);
        if !uniform_equal_terminals || !self.equal_size_picker_open {
            return;
        }
        let Some(runtime) = self.active_workspace_runtime() else {
            return;
        };
        if runtime.terminals.is_empty() {
            return;
        }
        let terminals: Vec<(u64, String, Vec2)> = runtime
            .terminals
            .iter()
            .map(|pane| (pane.id, pane.title.clone(), pane.desired_size))
            .collect();
        let mut selection = self.equal_size_picker_selection;
        let mut apply_selection = false;
        let mut close_only = false;

        egui::Window::new("Equal-size template terminal")
            .collapsible(false)
            .resizable(false)
            .anchor(egui::Align2::CENTER_CENTER, Vec2::ZERO)
            .frame(
                egui::Frame::window(&ctx.style())
                    .fill(p.popover_fill)
                    .stroke(Stroke::new(1.0, p.border)),
            )
            .show(ctx, |ui| {
                ui.label("Choose a terminal size to apply to all panes in equal-size grid:");
                ui.add_space(6.0);
                for (id, title, size) in &terminals {
                    let selected = selection == Some(*id);
                    let label = format!("{title} ({:.0} x {:.0})", size.x, size.y);
                    if ui.selectable_label(selected, label).clicked() {
                        selection = Some(*id);
                        self.trigger_equal_size_template_blink(*id);
                    }
                }
                ui.add_space(8.0);
                ui.horizontal(|ui| {
                    if ui.button("Apply").clicked() {
                        apply_selection = true;
                    }
                    if ui.button("Cancel").clicked() {
                        close_only = true;
                    }
                });
            });

        self.equal_size_picker_selection = selection;
        if apply_selection {
            if let Some(runtime_mut) = self.active_workspace_runtime_mut() {
                runtime_mut.equal_size_source_terminal_id = selection;
            }
            self.equal_size_picker_open = false;
            self.equal_size_template_blink_terminal_id = None;
            self.equal_size_template_blink_started_at = None;
            save_workspace_state(self);
        } else if close_only {
            // Canceling the picker should disable the equal-size grid toggle as well.
            if let Some(tab) = self.active_workspace_tab_mut() {
                tab.uniform_equal_terminals = false;
            }
            self.equal_size_picker_open = false;
            self.equal_size_picker_selection = None;
            if let Some(runtime_mut) = self.active_workspace_runtime_mut() {
                runtime_mut.equal_size_source_terminal_id = None;
            }
            self.equal_size_template_blink_terminal_id = None;
            self.equal_size_template_blink_started_at = None;
            save_workspace_state(self);
        }
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

        let layout = self.active_panel_layout();
        let col = pick_column_at_x(local_pos.x, area_size.x, layout);
        let w = column_stripe_width(area_size.x, layout);
        let x = column_band_left(area_size.x, col, layout);
        let rect = egui::Rect::from_min_size(
            area_min + Vec2::new(x, 0.0),
            Vec2::new(w, area_size.y.max(1.0)),
        );

        paint_broken_border(ui.painter(), rect, color);
    }
}

fn workspace_content_height(terminals: &[TerminalPane], viewport_h: f32) -> f32 {
    if terminals.is_empty() {
        return viewport_h;
    }
    let mut bottom = 0.0_f32;
    for pane in terminals {
        let pos = pane.position.unwrap_or_default();
        bottom = bottom.max(pos.y + pane.desired_size.y);
    }
    // Compare pane extent to the viewport *before* adding bottom margin. If we used
    // `(bottom + margin) <= viewport` then a stack that exactly fills the height would
    // still count as "overflow" (`bottom + 2*GRID > viewport`), inflating scroll content
    // every frame — `reflow_panes_to_column_starts` then sees a wider area and grows panes.
    //
    // When content is *taller* than the viewport, do not add trailing padding to the
    // scroll height: reflow/sync makes `bottom` match the previous scroll height, so
    // `bottom + 2*GRID` would grow without bound (same class of bug as horizontal resize).
    if bottom <= viewport_h {
        viewport_h
    } else {
        bottom
    }
}

fn workspace_content_width(terminals: &[TerminalPane], viewport_w: f32) -> f32 {
    if terminals.is_empty() {
        return viewport_w;
    }
    let mut right = 0.0_f32;
    for pane in terminals {
        let pos = pane.position.unwrap_or_default();
        right = right.max(pos.x + pane.desired_size.x);
    }
    // When `right <= viewport`, use viewport (no horizontal scroll). When wider, size
    // scroll to `right` only: adding padding here makes `content_w` exceed pane bounds;
    // sync reflow fills that width so `right` equals old `content_w` next frame → loop.
    if right <= viewport_w {
        viewport_w
    } else {
        right
    }
}

/// Minimum `y` among panes whose center lies in the given column band.
fn min_y_topmost_in_column(
    terminals: &[TerminalPane],
    area_width: f32,
    column: usize,
    layout: PanelLayout,
) -> Option<f32> {
    let (stripe_left, stripe_right) = column_slot_x_range(area_width, column, layout);
    terminals
        .iter()
        .filter_map(|pane| {
            let pos = pane.position.unwrap_or_default();
            let pr = egui::Rect::from_min_size(pos, pane.desired_size);
            let ix0 = pr.min.x.max(stripe_left);
            let ix1 = pr.max.x.min(stripe_right);
            (ix1 > ix0 + 1.0).then_some(pos.y)
        })
        .min_by(|a, b| a.total_cmp(b))
}

/// Maximum bottom `y` among panes whose center lies in the given column band.
fn max_y_bottom_in_column(
    terminals: &[TerminalPane],
    area_width: f32,
    column: usize,
    layout: PanelLayout,
) -> Option<f32> {
    let (stripe_left, stripe_right) = column_slot_x_range(area_width, column, layout);
    terminals
        .iter()
        .filter_map(|pane| {
            let pos = pane.position.unwrap_or_default();
            let pr = egui::Rect::from_min_size(pos, pane.desired_size);
            let ix0 = pr.min.x.max(stripe_left);
            let ix1 = pr.max.x.min(stripe_right);
            (ix1 > ix0 + 1.0).then_some(pos.y + pane.desired_size.y)
        })
        .max_by(|a, b| a.total_cmp(b))
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
    _area_width: f32,
    _layout: PanelLayout,
) -> bool {
    let pos = pane.position.unwrap_or_default();
    let pr = egui::Rect::from_min_size(pos, pane.desired_size);
    const H_EPS: f32 = 1.0;
    const V_EPS: f32 = 1.0;
    let ix0 = candidate.min.x.max(pr.min.x);
    let iy0 = candidate.min.y.max(pr.min.y);
    let ix1 = candidate.max.x.min(pr.max.x);
    let iy1 = candidate.max.y.min(pr.max.y);
    ix1 > ix0 + H_EPS && iy1 > iy0 + V_EPS
}

/// Prefer the top of the column band, never overlapping any existing pane (any column).
/// Width is **stripe minus intrusions** (right edge stays on the column boundary) so wide
/// neighbours in other columns do not overlap. Tries height, then scans `y` upward.
fn find_spawn_column_no_overlap(
    terminals: &[TerminalPane],
    area_size: Vec2,
    column_layout_width: f32,
    column: usize,
    preferred_y: f32,
    preferred_max_h: f32,
    default_h: f32,
    layout: PanelLayout,
) -> (Pos2, Vec2) {
    let lw = column_layout_width.max(1.0);
    let slot_w = column_stripe_width(lw, layout);
    let stripe_left = column_band_left(lw, column, layout);
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
        let start_y = preferred_y.clamp(0.0, max_y);
        let max_steps = ((max_y / y_step).ceil() as u32).saturating_add(2);
        for k in 0..=max_steps {
            let delta = k as f32 * y_step;
            let y_candidates = if k == 0 {
                [Some(start_y), None]
            } else {
                let down = (start_y + delta <= max_y + 0.01).then_some((start_y + delta).clamp(0.0, max_y));
                let up = (start_y >= delta).then_some((start_y - delta).clamp(0.0, max_y));
                [down, up]
            };

            for y in y_candidates.into_iter().flatten() {
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
                    continue;
                }
                let cand = egui::Rect::from_min_size(Pos2::new(left, y), Vec2::new(w, h));
                if !overlaps(cand) {
                    return (cand.min, cand.size());
                }
            }
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
        Vec2::new(lw, area_size.y),
        Vec2::new(slot_w, h),
        column,
        preferred_y,
        layout,
    );
    let left =
        intrusion_left_right_aligned(terminals, stripe_left, stripe_right, pos.y, h, GRID_SPACING);
    let w = (stripe_right - left).max(min_spawn_w).min(slot_w);
    let cand = egui::Rect::from_min_size(Pos2::new(left, pos.y), Vec2::new(w, h));
    if !overlaps(cand) {
        return (cand.min, cand.size());
    }

    // Hard fallback: never return an overlapping position. Append below the current stack in
    // this column (growing workspace height as needed).
    let bottom = max_y_bottom_in_column(terminals, lw, column, layout).unwrap_or(0.0);
    let y = (bottom + STACK_GAP_Y).max(0.0);
    let left = intrusion_left_right_aligned(terminals, stripe_left, stripe_right, y, h, GRID_SPACING);
    let w = (stripe_right - left).max(min_spawn_w).min(slot_w);
    (Pos2::new(left, y), Vec2::new(w, h))
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
        .clamp(1.0, available_width.max(1.0));
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

/// Same width and height for every pane, row-major grid using the current column layout count.
/// `body_height` is the vertical span to fill with panes (not including [`workspace_content_height`]'s bottom margin).
fn reflow_panes_uniform_equal(
    terminals: &mut [TerminalPane],
    available_width: f32,
    body_height: f32,
    layout: PanelLayout,
    template_size: Option<Vec2>,
) {
    let n = terminals.len();
    if n == 0 || !(available_width > 0.0 && body_height > 0.0) {
        return;
    }
    let (_, _, n_cols) = column_slot_geometry(available_width, layout);
    let cols = n_cols.max(1);
    let rows = (n + cols - 1) / cols;
    let grid_cell_w = column_stripe_width(available_width, layout)
        .clamp(1.0, available_width.max(1.0));
    let gap_y = STACK_GAP_Y;
    let gap_total = (rows.saturating_sub(1)) as f32 * gap_y;
    let grid_cell_h = ((body_height - gap_total) / rows as f32).max(TERMINAL_MIN_HEIGHT);
    let cell_w = template_size
        .map(|s| s.x)
        .unwrap_or(grid_cell_w)
        .clamp(1.0, grid_cell_w);
    let cell_h = template_size
        .map(|s| s.y)
        .unwrap_or(grid_cell_h)
        .clamp(TERMINAL_MIN_HEIGHT, grid_cell_h.max(TERMINAL_MIN_HEIGHT));
    let max_x = (available_width - cell_w).max(0.0);

    for (i, pane) in terminals.iter_mut().enumerate() {
        let col = i % cols;
        let row = i / cols;
        let x = column_band_left(available_width, col, layout).clamp(0.0, max_x);
        let y = row as f32 * (cell_h + gap_y);
        pane.desired_size = Vec2::new(cell_w, cell_h);
        pane.position = Some(Pos2::new(x, y));
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

/// Horizontal span of the layout slot for `column` (one grid column, excluding neighbour gutters).
fn column_slot_x_range(area_width: f32, column: usize, layout: PanelLayout) -> (f32, f32) {
    let left = column_band_left(area_width, column, layout);
    let w = column_stripe_width(area_width, layout);
    (left, left + w)
}

/// True when some pane's bbox overlaps this column's slot on the x-axis (any row). Used so a
/// right-click "New terminal" can target a visually empty column instead of the stripe under the
/// cursor that is already covered by a wide pane.
fn column_slot_has_pane_overlap(
    terminals: &[TerminalPane],
    area_width: f32,
    column: usize,
    layout: PanelLayout,
) -> bool {
    let (stripe_left, stripe_right) = column_slot_x_range(area_width, column, layout);
    terminals.iter().any(|pane| {
        let pos = pane.position.unwrap_or_default();
        let pr = egui::Rect::from_min_size(pos, pane.desired_size);
        let ix0 = pr.min.x.max(stripe_left);
        let ix1 = pr.max.x.min(stripe_right);
        ix1 > ix0 + 1.0
    })
}

/// True when a pane belongs to this column by center point (normal column ownership).
fn column_has_native_pane(
    terminals: &[TerminalPane],
    area_width: f32,
    column: usize,
    layout: PanelLayout,
) -> bool {
    terminals.iter().any(|pane| {
        let pos = pane.position.unwrap_or_default();
        let pr = egui::Rect::from_min_size(pos, pane.desired_size);
        pick_column_at_x(pr.center().x, area_width, layout) == column
    })
}

/// Column under the cursor if that slot is unused; otherwise the unused slot closest by index.
fn pick_spawn_column_preferring_empty_slot(
    terminals: &[TerminalPane],
    area_width: f32,
    cursor_x: f32,
    layout: PanelLayout,
) -> usize {
    let preferred = pick_column_at_x(cursor_x, area_width, layout);
    let preferred_overlapped =
        column_slot_has_pane_overlap(terminals, area_width, preferred, layout);
    let preferred_has_native =
        column_has_native_pane(terminals, area_width, preferred, layout);
    if !preferred_overlapped || preferred_has_native {
        return preferred;
    }
    let (_, _, n) = column_slot_geometry(area_width, layout);
    let n = n.max(1);
    let mut best: Option<(u32, usize)> = None;
    for col in 0..n {
        if column_slot_has_pane_overlap(terminals, area_width, col, layout) {
            continue;
        }
        let dist = (col as i32 - preferred as i32).unsigned_abs();
        if best
            .as_ref()
            .map_or(true, |(d, c)| dist < *d || (dist == *d && col < *c))
        {
            best = Some((dist, col));
        }
    }
    best.map(|(_, c)| c).unwrap_or(preferred)
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
