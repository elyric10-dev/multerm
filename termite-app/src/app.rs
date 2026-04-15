use std::sync::Arc;

use crossbeam_channel::unbounded;
use termite_core::{pty::spawn_pty, session::TerminalSession, PaneId};
use termite_input::key_to_bytes;
use termite_render::{Compositor, GlyphAtlas, GpuContext};
use termite_ui::pane_layout::Rect;
use winit::{
    application::ApplicationHandler,
    dpi::LogicalSize,
    event::{KeyEvent, WindowEvent},
    event_loop::{ActiveEventLoop, EventLoopProxy},
    keyboard::ModifiersState,
    window::{Window, WindowId},
};

use crate::user_event::UserEvent;

const FONT_SIZE: f32 = 15.0;
const WINDOW_W:  f64 = 900.0;
const WINDOW_H:  f64 = 600.0;

pub struct TermiteApp {
    // Event loop proxy for waking from PTY thread
    proxy:       EventLoopProxy<UserEvent>,

    // winit / GPU
    window:      Option<Arc<Window>>,
    gpu:         Option<GpuContext>,
    compositor:  Option<Compositor>,
    atlas:       Option<GlyphAtlas>,

    // Terminal
    session:     Option<TerminalSession>,
    pty:         Option<termite_core::PtyHandle>,

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
            session:    None,
            pty:        None,
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

        // ── PTY ───────────────────────────────────────────────────────────────
        let (tx, rx) = unbounded::<Vec<u8>>();
        let (rows, cols) = {
            let cw = atlas.cell_width().max(1.0);
            let ch = atlas.cell_height().max(1.0);
            let w = gpu.surface_config.width  as f32;
            let h = gpu.surface_config.height as f32;
            ((h / ch).max(1.0) as usize, (w / cw).max(1.0) as usize)
        };

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

        self.gpu        = Some(gpu);
        self.atlas      = Some(atlas);
        self.compositor = Some(compositor);
        self.session    = Some(session);
        self.pty        = Some(pty);

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
                    let (rows, cols) = {
                        let cw = atlas.cell_width().max(1.0);
                        let ch = atlas.cell_height().max(1.0);
                        ((size.height as f32 / ch).max(1.0) as usize,
                         (size.width  as f32 / cw).max(1.0) as usize)
                    };
                    if let Some(s) = &mut self.session {
                        s.parser.resize(rows, cols);
                    }
                    if let Some(p) = &self.pty {
                        let _ = p.resize(rows as u16, cols as u16);
                    }
                }
            }

            WindowEvent::ScaleFactorChanged { scale_factor, .. } => {
                let scale = scale_factor as f32;
                if let Some(atlas) = &mut self.atlas {
                    atlas.set_raster_scale(scale);
                }
                // Re-do resize with new cell sizes
                if let (Some(gpu), Some(atlas)) = (&self.gpu, &self.atlas) {
                    let cw = atlas.cell_width().max(1.0);
                    let ch = atlas.cell_height().max(1.0);
                    let w  = gpu.surface_config.width  as f32;
                    let h  = gpu.surface_config.height as f32;
                    let rows = (h / ch).max(1.0) as usize;
                    let cols = (w / cw).max(1.0) as usize;
                    if let Some(s) = &mut self.session {
                        s.parser.resize(rows, cols);
                    }
                    if let Some(p) = &self.pty {
                        let _ = p.resize(rows as u16, cols as u16);
                    }
                }
            }

            WindowEvent::ModifiersChanged(new_mods) => {
                self.mods = new_mods.state();
            }

            WindowEvent::KeyboardInput { event, .. } => {
                self.handle_keyboard(event);
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
                if let Some(session) = &mut self.session {
                    session.drain_and_parse();
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
        let app_cursor = self
            .session
            .as_ref()
            .map(|s| s.parser.app_cursor_keys())
            .unwrap_or(false);

        if let Some(bytes) = key_to_bytes(&event, self.mods, app_cursor) {
            if !bytes.is_empty() {
                if let Some(pty) = &self.pty {
                    let _ = pty.write_all(&bytes);
                }
            }
        }
    }

    fn redraw(&mut self) {
        let (gpu, atlas, compositor, session, window) = match (
            &self.gpu,
            &mut self.atlas,
            &mut self.compositor,
            &self.session,
            &self.window,
        ) {
            (Some(g), Some(a), Some(c), Some(s), Some(w)) => (g, a, c, s, w),
            _ => return,
        };

        let scale = window.scale_factor() as f32;
        let rect  = Rect::new(
            0.0, 0.0,
            gpu.surface_config.width  as f32,
            gpu.surface_config.height as f32,
        );

        let grid = session.parser.grid();
        if let Err(e) = compositor.render_terminal_frame(gpu, atlas, scale, rect.as_array(), grid) {
            tracing::error!("render error: {e}");
        }
    }
}
