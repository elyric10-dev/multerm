use crossbeam_channel::unbounded;
use eframe::egui::{self, Color32, CursorIcon, FontFamily, Margin, Pos2, RichText, Sense, Stroke, Vec2};
use serde::{Deserialize, Serialize};
use std::{fs, path::PathBuf, time::Duration};
use termite_core::{pty::spawn_pty, session::TerminalSession, PaneId, PtyHandle};
use termite_vt::TerminalGrid;

const BG: Color32 = Color32::from_rgb(7, 10, 16);
const PANEL_BG: Color32 = Color32::from_rgb(11, 17, 28);
const BORDER: Color32 = Color32::from_rgb(33, 52, 84);
const TEXT: Color32 = Color32::from_rgb(195, 213, 242);
const MUTED: Color32 = Color32::from_rgb(118, 137, 172);
const TAB_ACTIVE_BG: Color32 = Color32::from_rgb(30, 67, 116);
const TAB_INACTIVE_BG: Color32 = Color32::from_rgb(17, 27, 43);
const TAB_CLOSE: Color32 = Color32::from_rgb(166, 180, 208);
const TAB_CLOSE_HOVER_BG: Color32 = Color32::from_rgb(119, 44, 56);
const TAB_CLOSE_ACTIVE_BG: Color32 = Color32::from_rgb(146, 56, 70);
const TAB_CLOSE_HOVER_TEXT: Color32 = Color32::from_rgb(255, 241, 246);
const PATH_BAR_BG: Color32 = Color32::from_rgb(13, 22, 36);
const PATH_BAR_BORDER: Color32 = Color32::from_rgb(29, 48, 76);
const TERM_BG: Color32 = Color32::from_rgb(5, 8, 12);
const CELL_W: f32 = 9.0;
const CELL_H: f32 = 18.0;
const GRID_SPACING: f32 = 10.0;
const GRID_COLUMNS: usize = 4;
const TERMINAL_MIN_WIDTH: f32 = 260.0;
const TERMINAL_MIN_HEIGHT: f32 = 180.0;
const RESIZE_HANDLE_SIZE: f32 = 14.0;
const RESIZE_EDGE_THICKNESS: f32 = 6.0;
const RESIZE_CORNER_HOTSPOT: f32 = 20.0;
/// Pixels past the pane outer edge where the BR diagonal grip lives (outside the border).
const CORNER_GRIP_OUTSET: f32 = 2.0;
/// Extra radius around BR corner to show resize cursor early.
const BR_CURSOR_HOVER_RADIUS: f32 = 14.0;

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

struct TermiteUi {
    selected_workspace: usize,
    workspaces: Vec<WorkspaceTab>,
    next_workspace_index: usize,
    terminals: Vec<TerminalPane>,
    next_terminal_index: usize,
    next_terminal_id: u64,
    active_terminal: Option<usize>,
    editing_workspace_idx: Option<usize>,
    editing_workspace_input: String,
    color_history: Vec<[u8; 4]>,
    color_hex_target_idx: Option<usize>,
    color_hex_input: String,
    color_picker_target_idx: Option<usize>,
    color_picker_draft: Color32,
    color_picker_original_rgba: Option<[u8; 4]>,
    color_picker_rendered_this_frame: bool,
}

struct WorkspaceTab {
    title: String,
    badge: Option<u8>,
    color_rgba: Option<[u8; 4]>,
}

#[derive(Serialize, Deserialize)]
struct WorkspaceState {
    selected_workspace: usize,
    workspaces: Vec<WorkspaceTabState>,
    next_workspace_index: usize,
    #[serde(default)]
    color_history: Vec<[u8; 4]>,
}

#[derive(Serialize, Deserialize)]
struct WorkspaceTabState {
    title: String,
    badge: Option<u8>,
    color_rgba: Option<[u8; 4]>,
}

impl Default for TermiteUi {
    fn default() -> Self {
        if let Some(state) = load_workspace_state() {
            let workspaces: Vec<WorkspaceTab> = state
                .workspaces
                .into_iter()
                .map(|tab| WorkspaceTab {
                    title: tab.title,
                    badge: tab.badge,
                    color_rgba: tab.color_rgba,
                })
                .collect();
            let next_workspace_index = compute_next_workspace_index(&workspaces);
            return Self {
                selected_workspace: state
                    .selected_workspace
                    .min(workspaces.len().saturating_sub(1)),
                workspaces,
                next_workspace_index,
                terminals: Vec::new(),
                next_terminal_index: 1,
                next_terminal_id: 1,
                active_terminal: None,
                editing_workspace_idx: None,
                editing_workspace_input: String::new(),
                color_history: state.color_history,
                color_hex_target_idx: None,
                color_hex_input: String::new(),
                color_picker_target_idx: None,
                color_picker_draft: TAB_ACTIVE_BG,
                color_picker_original_rgba: None,
                color_picker_rendered_this_frame: false,
            };
        }

        Self {
            selected_workspace: 2,
            workspaces: vec![
                WorkspaceTab {
                    title: "Workspace 1".to_string(),
                    badge: Some(2),
                    color_rgba: None,
                },
                WorkspaceTab {
                    title: "Workspace 2".to_string(),
                    badge: None,
                    color_rgba: None,
                },
                WorkspaceTab {
                    title: "Workspace 3".to_string(),
                    badge: Some(11),
                    color_rgba: None,
                },
                WorkspaceTab {
                    title: "Workspace 4".to_string(),
                    badge: Some(5),
                    color_rgba: None,
                },
                WorkspaceTab {
                    title: "Workspace 5".to_string(),
                    badge: None,
                    color_rgba: None,
                },
            ],
            next_workspace_index: 6,
            terminals: Vec::new(),
            next_terminal_index: 1,
            next_terminal_id: 1,
            active_terminal: None,
            editing_workspace_idx: None,
            editing_workspace_input: String::new(),
            color_history: Vec::new(),
            color_hex_target_idx: None,
            color_hex_input: String::new(),
            color_picker_target_idx: None,
            color_picker_draft: TAB_ACTIVE_BG,
            color_picker_original_rgba: None,
            color_picker_rendered_this_frame: false,
        }
    }
}

fn new_terminal_context_menu(ui: &mut egui::Ui, app: &mut TermiteUi) {
    if ui.button("New Terminal").clicked() {
        app.add_terminal();
        ui.close();
    }
}

impl eframe::App for TermiteUi {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        // Keep refreshing so PTY output appears live without explicit wakeups.
        ctx.request_repaint_after(Duration::from_millis(16));
        self.drain_terminals();
        self.handle_keyboard_input(ctx);
        self.color_picker_rendered_this_frame = false;

        let mut visuals = egui::Visuals::dark();
        visuals.override_text_color = Some(TEXT);
        visuals.panel_fill = BG;
        visuals.window_fill = BG;
        visuals.widgets.noninteractive.bg_fill = PANEL_BG;
        visuals.widgets.noninteractive.bg_stroke = Stroke::new(1.0, BORDER);
        ctx.set_visuals(visuals);

        egui::TopBottomPanel::top("workspace_tabs")
            .resizable(false)
            .exact_height(70.0)
            .frame(
                egui::Frame::default()
                    .fill(Color32::from_rgb(9, 13, 21))
                    .inner_margin(Margin::same(6)),
            )
            .show(ctx, |ui| {
                header_tabs(ui, self);
                ui.add_space(5.0);
                directory_path_bar(ui);
            });

        egui::CentralPanel::default().show(ctx, |ui| {
            egui::Frame::default()
                .fill(BG)
                .inner_margin(Margin::same(10))
                .show(ui, |ui| {
                    let area_rect = ui.max_rect();
                    let response = ui.interact(
                        area_rect,
                        ui.id().with("terminal_context_area"),
                        Sense::click(),
                    );
                    response.context_menu(|ui| new_terminal_context_menu(ui, self));

                    if self.terminals.is_empty() {
                        let hint = ui.label(
                            RichText::new("Right-click and choose \"New Terminal\".")
                                .size(13.0)
                                .color(MUTED),
                        );
                        hint.context_menu(|ui| new_terminal_context_menu(ui, self));
                        return;
                    }

                    let mut close_idx: Option<usize> = None;
                    let available_width = area_rect.width();
                    let available_height = area_rect.height();
                    let total_spacing = GRID_SPACING * (GRID_COLUMNS.saturating_sub(1) as f32);
                    let slot_width = ((available_width - total_spacing) / GRID_COLUMNS as f32)
                        .max(260.0);

                    for idx in 0..self.terminals.len() {
                        let pane = &mut self.terminals[idx];

                        if pane.position.is_none() {
                            pane.desired_size.x = pane.desired_size.x.max(slot_width);
                            pane.desired_size.y = pane.desired_size.y.max(available_height.max(260.0));
                            let col = idx % GRID_COLUMNS;
                            let row = idx / GRID_COLUMNS;
                            pane.position = Some(Pos2::new(
                                col as f32 * (slot_width + GRID_SPACING),
                                row as f32 * (pane.desired_size.y + GRID_SPACING),
                            ));
                        }

                        let mut pos = pane.position.unwrap_or(Pos2::ZERO);
                        let max_x = (available_width - pane.desired_size.x).max(0.0);
                        let max_y = (available_height - pane.desired_size.y).max(0.0);
                        pos.x = pos.x.clamp(0.0, max_x);
                        pos.y = pos.y.clamp(0.0, max_y);
                        pane.position = Some(pos);

                        let pane_rect = egui::Rect::from_min_size(
                            area_rect.min + pos.to_vec2(),
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
                            pane.position = Some(pos);
                        }

                        let left_rect = egui::Rect::from_min_max(
                            pane_rect.min,
                            Pos2::new(pane_rect.min.x + RESIZE_EDGE_THICKNESS, pane_rect.max.y),
                        );
                        let right_rect = egui::Rect::from_min_max(
                            Pos2::new(pane_rect.max.x - RESIZE_EDGE_THICKNESS, pane_rect.min.y),
                            pane_rect.max,
                        );
                        let top_rect = egui::Rect::from_min_max(
                            pane_rect.min,
                            Pos2::new(pane_rect.max.x, pane_rect.min.y + RESIZE_EDGE_THICKNESS),
                        );
                        let bottom_rect = egui::Rect::from_min_max(
                            Pos2::new(pane_rect.min.x, pane_rect.max.y - RESIZE_EDGE_THICKNESS),
                            pane_rect.max,
                        );

                        let tl_rect = egui::Rect::from_min_size(pane_rect.min, Vec2::splat(RESIZE_HANDLE_SIZE));
                        let tr_rect = egui::Rect::from_min_size(
                            Pos2::new(pane_rect.max.x - RESIZE_HANDLE_SIZE, pane_rect.min.y),
                            Vec2::splat(RESIZE_HANDLE_SIZE),
                        );
                        let bl_rect = egui::Rect::from_min_size(
                            Pos2::new(pane_rect.min.x, pane_rect.max.y - RESIZE_HANDLE_SIZE),
                            Vec2::splat(RESIZE_HANDLE_SIZE),
                        );
                        // Bottom-right grip: interaction + visuals sit outside the pane border.
                        let br_grip_rect = egui::Rect::from_min_size(
                            pane_rect.right_bottom() + Vec2::new(CORNER_GRIP_OUTSET, CORNER_GRIP_OUTSET),
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
                        let bottom_active = resize_bottom.hovered() || resize_bottom.dragged();
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

                            // Bottom-right corner: always diagonal resize (width + height).
                            if resize_br.dragged() {
                                let max_w = (available_width - pos.x).max(TERMINAL_MIN_WIDTH);
                                let max_h = (available_height - pos.y).max(TERMINAL_MIN_HEIGHT);
                                new_w = (new_w + delta.x).clamp(TERMINAL_MIN_WIDTH, max_w);
                                new_h = (new_h + delta.y).clamp(TERMINAL_MIN_HEIGHT, max_h);
                                pane.desired_size.x = new_w;
                                pane.desired_size.y = new_h;
                                pane.position = Some(pos);
                            } else {
                            let left_dragged = resize_left.dragged() || resize_tl.dragged() || resize_bl.dragged();
                            let right_dragged = resize_right.dragged() || resize_tr.dragged();
                            let top_dragged = resize_top.dragged() || resize_tl.dragged() || resize_tr.dragged();
                            let bottom_dragged = resize_bottom.dragged() || resize_bl.dragged();

                            if left_dragged {
                                let right = pos.x + pane.desired_size.x;
                                let max_left = (right - TERMINAL_MIN_WIDTH).max(0.0);
                                let proposed_left = (pos.x + delta.x).clamp(0.0, max_left);
                                new_x = proposed_left;
                                new_w = right - proposed_left;
                            }
                            if right_dragged {
                                let max_w = (available_width - new_x).max(TERMINAL_MIN_WIDTH);
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
                                let max_h = (available_height - new_y).max(TERMINAL_MIN_HEIGHT);
                                new_h = (new_h + delta.y).clamp(TERMINAL_MIN_HEIGHT, max_h);
                            }

                            pane.desired_size.x = new_w;
                            pane.desired_size.y = new_h;
                            pos.x = new_x;
                            pos.y = new_y;
                            pane.position = Some(pos);
                            }
                        }

                        let is_active = self.active_terminal == Some(idx);
                        let border = if is_active {
                            Color32::from_rgb(88, 142, 222)
                        } else {
                            BORDER
                        };

                        let pane_response = ui.allocate_rect(pane_rect, Sense::click());
                        ui.scope_builder(egui::UiBuilder::new().max_rect(pane_rect), |ui| {
                            egui::Frame::default()
                                .fill(TERM_BG)
                                .stroke(Stroke::new(1.0, border))
                                .inner_margin(Margin::same(6))
                                .show(ui, |ui| {
                                    ui.horizontal(|ui| {
                                        ui.label(
                                            RichText::new(&pane.title)
                                                .family(FontFamily::Monospace)
                                                .size(12.0)
                                                .color(TEXT),
                                        );
                                        ui.with_layout(
                                            egui::Layout::right_to_left(egui::Align::Center),
                                            |ui| {
                                                if ui.small_button("x").clicked() {
                                                    close_idx = Some(idx);
                                                }
                                            },
                                        );
                                    });
                                    ui.separator();
                                    let terminal_height = ui.available_height().max(120.0);
                                    let terminal_size = Vec2::new(pane.desired_size.x, terminal_height);
                                    resize_terminal_for_size(pane, terminal_size);
                                    render_terminal_grid(
                                        ui,
                                        pane.id,
                                        pane.session.parser.grid(),
                                    );
                                });
                        });

                        if pane_response.clicked() {
                            self.active_terminal = Some(idx);
                        }
                        if near_br_corner || resize_br.hovered() || resize_br.dragged() {
                            paint_br_resize_line(
                                ui.painter(),
                                br_grip_rect,
                                resize_br.hovered() || resize_br.dragged(),
                            );
                        }

                    }

                    if let Some(idx) = close_idx {
                        self.terminals.remove(idx);
                        self.active_terminal = if self.terminals.is_empty() {
                            None
                        } else {
                            Some(self.active_terminal.unwrap_or(0).min(self.terminals.len() - 1))
                        };
                    }
                });
        });

        self.cleanup_stale_color_picker();
    }
}

impl TermiteUi {
    fn add_terminal(&mut self) {
        let pane = spawn_terminal_pane(self.next_terminal_index, self.next_terminal_id);
        self.next_terminal_index += 1;
        self.next_terminal_id += 1;
        self.terminals.push(pane);
        self.active_terminal = Some(self.terminals.len() - 1);
    }

    fn drain_terminals(&mut self) {
        for pane in &mut self.terminals {
            let _ = pane.session.drain_and_parse();
        }
    }

    fn handle_keyboard_input(&mut self, ctx: &egui::Context) {
        let Some(active_idx) = self.active_terminal else { return; };
        if active_idx >= self.terminals.len() {
            return;
        }

        let mut shortcut_new_terminal = false;
        let events = ctx.input(|i| i.events.clone());
        for event in events {
            match event {
                egui::Event::Text(text) => {
                    if !text.is_empty() {
                        let _ = self.terminals[active_idx].pty.write_all(text.as_bytes());
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
                        let _ = self.terminals[active_idx].pty.write_all(&bytes);
                    }
                }
                _ => {}
            }
        }

        if shortcut_new_terminal {
            self.add_terminal();
        }
    }
}

fn spawn_terminal_pane(next_terminal_index: usize, next_terminal_id: u64) -> TerminalPane {
    let (tx, rx) = unbounded::<Vec<u8>>();
    let shell = std::env::var("SHELL").unwrap_or_else(|_| "/bin/zsh".to_string());
    let wake_up: Box<dyn Fn() + Send + 'static> = Box::new(|| {});
    let pty = spawn_pty(&shell, 24, 80, tx, wake_up).expect("spawn terminal pty");

    TerminalPane {
        id: next_terminal_id,
        title: format!("Terminal {}", next_terminal_index),
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

fn paint_br_resize_line(painter: &egui::Painter, grip_rect: egui::Rect, hot: bool) {
    let color = if hot {
        Color32::from_rgb(160, 196, 245)
    } else {
        Color32::from_rgb(96, 130, 184)
    };
    let stroke = Stroke::new(1.0, color);
    let corner = grip_rect.left_top() - Vec2::splat(CORNER_GRIP_OUTSET);
    // Single short diagonal mark at the corner.
    let start = corner + Vec2::new(-2.0, -2.0);
    let end = corner + Vec2::new(8.0, 8.0);
    painter.line_segment([start, end], stroke);
}

fn render_terminal_grid(ui: &mut egui::Ui, pane_id: u64, grid: &TerminalGrid) {
    let mut content = String::new();
    for row in 0..grid.rows {
        let mut row_text = String::with_capacity(grid.cols);
        for col in 0..grid.cols {
            row_text.push(grid.cell(row, col).ch);
        }
        content.push_str(row_text.trim_end_matches(' '));
        if row + 1 < grid.rows {
            content.push('\n');
        }
    }

    egui::ScrollArea::vertical()
        .id_salt(("term-scroll", pane_id))
        .auto_shrink([false, false])
        .show(ui, |ui| {
            ui.add(
                egui::Label::new(
                    RichText::new(content)
                        .family(FontFamily::Monospace)
                        .size(12.0)
                        .color(TEXT),
                )
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

fn header_tabs(ui: &mut egui::Ui, app: &mut TermiteUi) {
    let mut changed = false;
    let mut close_idx: Option<usize> = None;
    ui.horizontal(|ui| {
        for idx in 0..app.workspaces.len() {
            let active = idx == app.selected_workspace;
            let fill = app.workspace_tab_fill_color(idx, active);
            let title = app.workspaces[idx].title.clone();
            let badge = app.workspaces[idx].badge;
            let text_color = if active { Color32::WHITE } else { MUTED };

            let _tab_frame = egui::Frame::default()
                .fill(fill)
                .stroke(Stroke::new(1.0, BORDER))
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
                                    workspace_tab_context_menu(ui, app, idx, &mut changed);
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
                                    .color(MUTED),
                            );
                        }

                        let close_btn = egui::Button::new(
                            RichText::new("x")
                                .size(11.0)
                                .family(FontFamily::Monospace)
                                .color(TAB_CLOSE),
                        )
                        .fill(Color32::TRANSPARENT)
                        .stroke(Stroke::NONE)
                        .min_size(Vec2::new(12.0, 20.0));
                        let close_resp = ui.add(close_btn).on_hover_text("Close workspace");
                        if close_resp.hovered() || close_resp.is_pointer_button_down_on() {
                            let close_bg = if close_resp.is_pointer_button_down_on() {
                                TAB_CLOSE_ACTIVE_BG
                            } else {
                                TAB_CLOSE_HOVER_BG
                            };
                            let close_fg = if close_resp.is_pointer_button_down_on() {
                                Color32::WHITE
                            } else {
                                TAB_CLOSE_HOVER_TEXT
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
                .color(MUTED),
        )
        .fill(TAB_INACTIVE_BG)
        .stroke(Stroke::new(1.0, BORDER))
        .min_size(Vec2::new(26.0, 28.0))
        .corner_radius(3.0);
        if ui.add(plus_btn).on_hover_text("New workspace").clicked() {
            let title = format!("Workspace {}", app.next_workspace_index);
            app.next_workspace_index += 1;
            app.workspaces.push(WorkspaceTab {
                title,
                badge: None,
                color_rgba: None,
            });
            app.selected_workspace = app.workspaces.len() - 1;
            changed = true;
        }
    });

    if let Some(idx) = close_idx {
        app.workspaces.remove(idx);
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

fn workspace_tab_context_menu(
    ui: &mut egui::Ui,
    app: &mut TermiteUi,
    idx: usize,
    changed: &mut bool,
) {
    if ui.button("Rename").clicked() {
        app.editing_workspace_idx = Some(idx);
        app.editing_workspace_input = app.workspaces[idx].title.clone();
        ui.close();
    }

    ui.label(RichText::new("Tab color").size(11.0).color(MUTED));
    ui.horizontal(|ui| {
        let preview = if app.color_picker_target_idx == Some(idx) {
            app.color_picker_draft
        } else {
            app.workspaces[idx].editor_base_color()
        };
        let preview_btn = egui::Button::new("")
            .min_size(Vec2::new(18.0, 18.0))
            .fill(preview)
            .stroke(Stroke::new(1.0, BORDER));
        let open_picker = ui.add(preview_btn).clicked();
        ui.label(RichText::new("Pick color").size(12.0).color(MUTED));
        if open_picker {
            app.color_picker_target_idx = Some(idx);
            app.color_picker_draft = app.workspaces[idx].editor_base_color();
            app.color_picker_original_rgba = app.workspaces[idx].color_rgba;
            app.color_hex_input = color_to_hex_string(app.color_picker_draft);
        }
    });

    let mut picker_rect: Option<egui::Rect> = None;
    if app.color_picker_target_idx == Some(idx) {
        app.color_picker_rendered_this_frame = true;
        let picker_resp = egui::Frame::default()
            .fill(Color32::from_rgb(8, 14, 24))
            .stroke(Stroke::new(1.0, BORDER))
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
                            color_to_hex_string(app.workspaces[idx].editor_base_color());
                    }
                });
            });
        picker_rect = Some(picker_resp.response.rect);
    }

    if app.color_hex_target_idx != Some(idx) {
        app.color_hex_target_idx = Some(idx);
        app.color_hex_input = color_to_hex_string(app.workspaces[idx].editor_base_color());
    }
    ui.horizontal(|ui| {
        ui.label(RichText::new("Hex").size(11.0).color(MUTED));
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
        ui.label(RichText::new("Recent").size(11.0).color(MUTED));
        ui.horizontal_wrapped(|ui| {
            let history = app.color_history.clone();
            for rgba in history.iter().rev().take(10) {
                let swatch = Color32::from_rgba_unmultiplied(rgba[0], rgba[1], rgba[2], rgba[3]);
                let button = egui::Button::new("")
                    .min_size(Vec2::new(16.0, 16.0))
                    .fill(swatch)
                    .stroke(Stroke::new(1.0, BORDER));
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

fn directory_path_bar(ui: &mut egui::Ui) {
    let full_width = ui.available_width();
    egui::Frame::default()
        .fill(PATH_BAR_BG)
        .stroke(Stroke::new(1.0, PATH_BAR_BORDER))
        .inner_margin(Margin::symmetric(10, 4))
        .show(ui, |ui| {
            ui.set_width(full_width);
            ui.horizontal(|ui| {
                ui.label(RichText::new("●").size(10.0).color(Color32::from_rgb(52, 217, 113)));
                ui.add_space(2.0);
                ui.label(
                    RichText::new("~/Users/zevzairen/Desktop/bridgecode")
                        .size(12.0)
                        .family(FontFamily::Monospace)
                        .color(MUTED),
                );
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
        selected_workspace: app.selected_workspace,
        workspaces: app
            .workspaces
            .iter()
            .map(|tab| WorkspaceTabState {
                title: tab.title.clone(),
                badge: tab.badge,
                color_rgba: tab.color_rgba,
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
    fn editor_base_color(&self) -> Color32 {
        if let Some([r, g, b, a]) = self.color_rgba {
            Color32::from_rgba_unmultiplied(r, g, b, a)
        } else {
            TAB_ACTIVE_BG
        }
    }

    fn tab_color(&self, active: bool) -> Color32 {
        if let Some([r, g, b, a]) = self.color_rgba {
            let base = Color32::from_rgba_unmultiplied(r, g, b, a);
            if active {
                base
            } else {
                Color32::from_rgba_unmultiplied(r.saturating_div(2), g.saturating_div(2), b.saturating_div(2), a)
            }
        } else if active {
            TAB_ACTIVE_BG
        } else {
            TAB_INACTIVE_BG
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

    fn workspace_tab_fill_color(&self, idx: usize, active: bool) -> Color32 {
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
            self.workspaces[idx].tab_color(active)
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
    format!(
        "#{:02X}{:02X}{:02X}",
        color.r(),
        color.g(),
        color.b()
    )
}
