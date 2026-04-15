use std::sync::Arc;

use crossbeam_channel::unbounded;
use termite_core::{pty::spawn_pty, session::TerminalSession, PaneId};
use termite_input::key_to_bytes;
use termite_render::{Compositor, GlyphAtlas, GpuContext};
use termite_ui::pane_layout::Rect;
use winit::{
    application::ApplicationHandler,
    dpi::LogicalSize,
    event::{ElementState, KeyEvent, MouseButton, WindowEvent},
    event_loop::{ActiveEventLoop, EventLoopProxy},
    keyboard::{Key, ModifiersState},
    window::{Window, WindowId},
};

use crate::user_event::UserEvent;

const FONT_SIZE: f32 = 15.0;
const WINDOW_W:  f64 = 900.0;
const WINDOW_H:  f64 = 600.0;
const MAX_PANES: usize = 4;
const DIVIDER_GRAB_RADIUS: f32 = 8.0;
const SPLIT_MIN_RATIO: f32 = 0.2;
const SPLIT_MAX_RATIO: f32 = 0.8;

struct PaneRuntime {
    session: TerminalSession,
    pty: termite_core::PtyHandle,
}

#[derive(Clone, Copy, Debug)]
enum DragDivider {
    Vertical,
    Horizontal,
}

pub struct TermiteApp {
    // Event loop proxy for waking from PTY thread
    proxy:       EventLoopProxy<UserEvent>,

    // winit / GPU
    window:      Option<Arc<Window>>,
    gpu:         Option<GpuContext>,
    compositor:  Option<Compositor>,
    atlas:       Option<GlyphAtlas>,

    // Terminal panes
    panes:       Vec<PaneRuntime>,
    active_pane: usize,
    split_x:     f32,
    split_y:     f32,
    dragging:    Option<DragDivider>,
    cursor_pos:  (f32, f32),

    // Keyboard modifier state
    mods:        ModifiersState,
}

impl TermiteApp {
    pub fn new(proxy: EventLoopProxy<UserEvent>) -> Self {
        Self {
            proxy,
            window:     None,
            gpu:        None,
            compositor: None,
            atlas:      None,
            panes:      Vec::new(),
            active_pane: 0,
            split_x:    0.5,
            split_y:    0.5,
            dragging:   None,
            cursor_pos: (0.0, 0.0),
            mods:       ModifiersState::empty(),
        }
    }

    #[allow(dead_code)]
    fn compute_terminal_size(&self) -> (usize, usize) {
        let atlas = match &self.atlas {
            Some(a) => a,
            None    => return (24, 80),
        };
        let gpu = match &self.gpu {
            Some(g) => g,
            None    => return (24, 80),
        };
        let cw = atlas.cell_width().max(1.0);
        let ch = atlas.cell_height().max(1.0);
        let cols = (gpu.surface_config.width  as f32 / cw).max(1.0) as usize;
        let rows = (gpu.surface_config.height as f32 / ch).max(1.0) as usize;
        (rows, cols)
    }

    fn spawn_pane(&self, rows: usize, cols: usize) -> PaneRuntime {
        let (tx, rx) = unbounded::<Vec<u8>>();
        let proxy = self.proxy.clone();
        let wake_up: Box<dyn Fn() + Send + 'static> = Box::new(move || {
            let _ = proxy.send_event(UserEvent::PtyData);
        });

        let shell = std::env::var("SHELL").unwrap_or_else(|_| "/bin/zsh".into());
        let pty = spawn_pty(
            &shell,
            rows as u16,
            cols as u16,
            tx,
            wake_up,
        ).expect("spawn_pty");
        let session = TerminalSession::new(PaneId::new(), rows, cols, rx);
        PaneRuntime { session, pty }
    }

    fn pane_layout(&self) -> Vec<Rect> {
        let gpu = match &self.gpu {
            Some(gpu) => gpu,
            None => return Vec::new(),
        };
        let width = gpu.surface_config.width as f32;
        let height = gpu.surface_config.height as f32;

        match self.panes.len() {
            0 => Vec::new(),
            1 => vec![Rect::new(0.0, 0.0, width, height)],
            2 => {
                let left_w = width * self.split_x;
                vec![
                    Rect::new(0.0, 0.0, left_w, height),
                    Rect::new(left_w, 0.0, width - left_w, height),
                ]
            }
            _ => {
                let left_w = width * self.split_x;
                let top_h = height * self.split_y;
                vec![
                    Rect::new(0.0, 0.0, left_w, top_h),
                    Rect::new(left_w, 0.0, width - left_w, top_h),
                    Rect::new(0.0, top_h, left_w, height - top_h),
                    Rect::new(left_w, top_h, width - left_w, height - top_h),
                ]
            }
        }
    }

    fn resize_panes_to_layout(&mut self) {
        let Some(atlas) = &self.atlas else { return; };
        let pane_rects = self.pane_layout();
        let cell_w = atlas.cell_width().max(1.0);
        let cell_h = atlas.cell_height().max(1.0);

        for (pane, rect) in self.panes.iter_mut().zip(pane_rects.iter()) {
            let cols = (rect.w / cell_w).max(1.0) as usize;
            let rows = (rect.h / cell_h).max(1.0) as usize;
            pane.session.parser.resize(rows, cols);
            let _ = pane.pty.resize(rows as u16, cols as u16);
        }
    }

    fn add_new_terminal(&mut self) {
        if self.panes.len() >= MAX_PANES {
            return;
        }
        self.panes.push(self.spawn_pane(24, 80));
        self.active_pane = self.panes.len().saturating_sub(1);
        self.resize_panes_to_layout();
    }

    fn pane_index_at(&self, x: f32, y: f32) -> Option<usize> {
        self.pane_layout()
            .iter()
            .enumerate()
            .find(|(_, rect)| {
                x >= rect.x && y >= rect.y && x <= rect.x + rect.w && y <= rect.y + rect.h
            })
            .map(|(idx, _)| idx)
    }

    fn divider_hit_test(&self, x: f32, y: f32) -> Option<DragDivider> {
        let gpu = self.gpu.as_ref()?;
        let width = gpu.surface_config.width as f32;
        let height = gpu.surface_config.height as f32;

        if self.panes.len() >= 2 {
            let split_x_px = width * self.split_x;
            if (x - split_x_px).abs() <= DIVIDER_GRAB_RADIUS {
                return Some(DragDivider::Vertical);
            }
        }
        if self.panes.len() >= 3 {
            let split_y_px = height * self.split_y;
            if (y - split_y_px).abs() <= DIVIDER_GRAB_RADIUS {
                return Some(DragDivider::Horizontal);
            }
        }
        None
    }
}

impl ApplicationHandler<UserEvent> for TermiteApp {
    fn resumed(&mut self, event_loop: &ActiveEventLoop) {
        // ── Create window ─────────────────────────────────────────────────────
        let attrs = Window::default_attributes()
            .with_title("TermITE")
            .with_inner_size(LogicalSize::new(WINDOW_W, WINDOW_H));

        let window = Arc::new(
            event_loop.create_window(attrs).expect("create window")
        );
        self.window = Some(Arc::clone(&window));

        // ── GPU context ───────────────────────────────────────────────────────
        let gpu = GpuContext::new(Arc::clone(&window)).expect("GpuContext::new");

        // ── Glyph atlas ───────────────────────────────────────────────────────
        let scale = window.scale_factor() as f32;
        let mut atlas = GlyphAtlas::new(&gpu, FONT_SIZE);
        atlas.set_raster_scale(scale);

        // ── Compositor ────────────────────────────────────────────────────────
        let compositor = Compositor::new(&gpu, &atlas).expect("Compositor::new");

        let (rows, cols) = {
            let cw = atlas.cell_width().max(1.0);
            let ch = atlas.cell_height().max(1.0);
            let w = gpu.surface_config.width  as f32;
            let h = gpu.surface_config.height as f32;
            ((h / ch).max(1.0) as usize, (w / cw).max(1.0) as usize)
        };

        self.gpu        = Some(gpu);
        self.atlas      = Some(atlas);
        self.compositor = Some(compositor);
        self.panes      = vec![self.spawn_pane(rows, cols)];
        self.active_pane = 0;

        tracing::info!("TermITE started — {}×{} cells", cols, rows);
    }

    fn window_event(
        &mut self,
        event_loop: &ActiveEventLoop,
        _window_id: WindowId,
        event: WindowEvent,
    ) {
        match event {
            WindowEvent::CloseRequested => {
                event_loop.exit();
            }

            WindowEvent::Resized(size) => {
                if let (Some(gpu), Some(atlas)) = (&mut self.gpu, &mut self.atlas) {
                    gpu.resize(size.width, size.height);
                    let _ = atlas;
                    self.resize_panes_to_layout();
                }
            }

            WindowEvent::ScaleFactorChanged { scale_factor, .. } => {
                let scale = scale_factor as f32;
                if let Some(atlas) = &mut self.atlas {
                    atlas.set_raster_scale(scale);
                }
                self.resize_panes_to_layout();
            }

            WindowEvent::ModifiersChanged(new_mods) => {
                self.mods = new_mods.state();
            }

            WindowEvent::KeyboardInput { event, .. } => {
                self.handle_keyboard(event);
            }

            WindowEvent::CursorMoved { position, .. } => {
                self.cursor_pos = (position.x as f32, position.y as f32);
                if let Some(active_drag) = self.dragging {
                    if let Some(gpu) = &self.gpu {
                        let width = gpu.surface_config.width as f32;
                        let height = gpu.surface_config.height as f32;
                        match active_drag {
                            DragDivider::Vertical => {
                                self.split_x = (self.cursor_pos.0 / width)
                                    .clamp(SPLIT_MIN_RATIO, SPLIT_MAX_RATIO);
                            }
                            DragDivider::Horizontal => {
                                self.split_y = (self.cursor_pos.1 / height)
                                    .clamp(SPLIT_MIN_RATIO, SPLIT_MAX_RATIO);
                            }
                        }
                        self.resize_panes_to_layout();
                        if let Some(window) = &self.window {
                            window.request_redraw();
                        }
                    }
                }
            }

            WindowEvent::MouseInput { state, button, .. } => {
                if button == MouseButton::Left && state == ElementState::Pressed {
                    if let Some(divider) =
                        self.divider_hit_test(self.cursor_pos.0, self.cursor_pos.1)
                    {
                        self.dragging = Some(divider);
                    } else if let Some(idx) =
                        self.pane_index_at(self.cursor_pos.0, self.cursor_pos.1)
                    {
                        self.active_pane = idx;
                    }
                }
                if button == MouseButton::Left && state == ElementState::Released {
                    self.dragging = None;
                }
            }

            WindowEvent::RedrawRequested => {
                self.redraw();
            }

            _ => {}
        }
    }

    fn user_event(&mut self, _event_loop: &ActiveEventLoop, event: UserEvent) {
        match event {
            UserEvent::PtyData => {
                for pane in &mut self.panes {
                    pane.session.drain_and_parse();
                }
                if let Some(window) = &self.window {
                    window.request_redraw();
                }
            }
        }
    }

    fn about_to_wait(&mut self, _event_loop: &ActiveEventLoop) {}
}

impl TermiteApp {
    fn handle_keyboard(&mut self, event: KeyEvent) {
        if event.state == ElementState::Pressed {
            let is_new_terminal_shortcut =
                matches!(event.logical_key, Key::Character(ref c) if c.eq_ignore_ascii_case("n"))
                    && self.mods.shift_key()
                    && (self.mods.control_key() || self.mods.super_key());
            if is_new_terminal_shortcut {
                self.add_new_terminal();
                if let Some(window) = &self.window {
                    window.request_redraw();
                }
                return;
            }
        }

        let app_cursor = self
            .panes
            .get(self.active_pane)
            .map(|p| p.session.parser.app_cursor_keys())
            .unwrap_or(false);

        if let Some(bytes) = key_to_bytes(&event, self.mods, app_cursor) {
            if !bytes.is_empty() {
                if let Some(pane) = self.panes.get(self.active_pane) {
                    let _ = pane.pty.write_all(&bytes);
                }
            }
        }
    }

    fn redraw(&mut self) {
        let pane_rects = self.pane_layout();
        if pane_rects.is_empty() || self.panes.is_empty() {
            return;
        }

        let (gpu, atlas, compositor, window) = match (
            &self.gpu,
            &mut self.atlas,
            &mut self.compositor,
            &self.window,
        ) {
            (Some(g), Some(a), Some(c), Some(w)) => (g, a, c, w),
            _ => return,
        };

        let scale = window.scale_factor() as f32;
        let pane_grids: Vec<([f32; 4], &termite_vt::TerminalGrid)> = self
            .panes
            .iter()
            .zip(pane_rects.iter())
            .map(|(pane, rect)| (rect.as_array(), pane.session.parser.grid()))
            .collect();

        if let Err(e) = compositor.render_terminal_panes(gpu, atlas, scale, &pane_grids) {
            tracing::error!("render error: {e}");
        }
    }
}
