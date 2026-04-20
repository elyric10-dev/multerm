use std::{
    collections::VecDeque,
    fs,
    io::{Read, Write},
    net::TcpStream,
    process::{Command, Stdio},
    sync::Arc,
    time::Duration,
};

use crossbeam_channel::unbounded;
use multerm_core::{pty::spawn_pty, session::TerminalSession, PaneId};
use multerm_input::key_to_bytes;
use multerm_render::{Compositor, CursorState, GlyphAtlas, GpuContext, SelectionRange};
use multerm_ui::pane_layout::Rect;
use winit::{
    application::ApplicationHandler,
    dpi::LogicalSize,
    event::{ElementState, KeyEvent, MouseButton, WindowEvent},
    event_loop::{ActiveEventLoop, EventLoopProxy},
    keyboard::{Key, ModifiersState, NamedKey},
    window::{Window, WindowId},
};

use crate::{clipboard, user_event::UserEvent};

const FONT_SIZE: f32 = 15.0;
const WINDOW_W: f64 = 900.0;
const WINDOW_H: f64 = 600.0;
const MAX_PANES: usize = 4;
const DIVIDER_GRAB_RADIUS: f32 = 8.0;
const SPLIT_MIN_RATIO: f32 = 0.2;
const SPLIT_MAX_RATIO: f32 = 0.8;
const SNAPSHOT_CAPACITY: usize = 50;

struct PaneHistory {
    snapshots: VecDeque<multerm_vt::TerminalGrid>,
    /// How many steps back we are from live. 0 = live view.
    steps_back: usize,
}

impl PaneHistory {
    fn new() -> Self {
        Self {
            snapshots: VecDeque::new(),
            steps_back: 0,
        }
    }

    /// Push a snapshot of the current grid (called before each PTY data batch).
    /// Ignored while the user is browsing history so indices stay stable.
    fn push(&mut self, grid: multerm_vt::TerminalGrid) {
        if self.steps_back > 0 {
            return;
        }
        self.snapshots.push_front(grid);
        if self.snapshots.len() > SNAPSHOT_CAPACITY {
            self.snapshots.pop_back();
        }
    }

    /// Step one snapshot further back. Returns false if already at the oldest.
    fn undo(&mut self) -> bool {
        let next = self.steps_back + 1;
        if next > self.snapshots.len() {
            return false;
        }
        self.steps_back = next;
        true
    }

    /// Step one snapshot toward live. Returns false if already live.
    fn redo(&mut self) -> bool {
        if self.steps_back == 0 {
            return false;
        }
        self.steps_back -= 1;
        true
    }

    fn resume_live(&mut self) {
        self.steps_back = 0;
    }

    fn is_live(&self) -> bool {
        self.steps_back == 0
    }

    /// Returns the snapshot grid to display, or `None` when live.
    fn current_snapshot(&self) -> Option<&multerm_vt::TerminalGrid> {
        if self.steps_back == 0 {
            return None;
        }
        self.snapshots.get(self.steps_back - 1)
    }
}

// Protocol frames for the session daemon (mirrors `daemon.rs`).
const FRAME_ATTACH: u8 = 1;
const FRAME_ATTACH_ERROR: u8 = 2;
const FRAME_OUTPUT: u8 = 3;
const FRAME_INPUT: u8 = 4;
const FRAME_RESIZE: u8 = 5;

struct PaneRuntime {
    session: TerminalSession,
    backend: PaneBackend,
}

enum PaneBackend {
    LocalPty { pty: multerm_core::PtyHandle },
    DaemonPty { writer: TcpStream },
}

impl PaneBackend {
    fn write_input(&mut self, data: &[u8]) {
        match self {
            PaneBackend::LocalPty { pty } => {
                let _ = pty.write_all(data);
            }
            PaneBackend::DaemonPty { writer } => {
                let _ = MultermApp::write_frame_tcp(writer, FRAME_INPUT, data);
            }
        }
    }

    fn resize(&mut self, rows: u16, cols: u16) {
        match self {
            PaneBackend::LocalPty { pty } => {
                let _ = pty.resize(rows, cols);
            }
            PaneBackend::DaemonPty { writer } => {
                let payload = [rows.to_le_bytes(), cols.to_le_bytes()].concat();
                let _ = MultermApp::write_frame_tcp(writer, FRAME_RESIZE, &payload);
            }
        }
    }
}

#[derive(Clone, Copy, Debug)]
enum DragDivider {
    Vertical,
    Horizontal,
}

#[derive(Clone, Copy, Debug)]
struct SelectionDrag {
    pane_idx: usize,
    start: (usize, usize),
}

pub struct MultermApp {
    // Event loop proxy for waking from PTY thread
    proxy: EventLoopProxy<UserEvent>,

    // winit / GPU
    window: Option<Arc<Window>>,
    gpu: Option<GpuContext>,
    compositor: Option<Compositor>,
    atlas: Option<GlyphAtlas>,

    // Terminal panes
    panes: Vec<PaneRuntime>,
    active_pane: usize,
    split_x: f32,
    split_y: f32,
    dragging: Option<DragDivider>,
    selections: Vec<Option<SelectionRange>>,
    selection_drag: Option<SelectionDrag>,
    cursor_pos: (f32, f32),
    histories: Vec<PaneHistory>,

    // Keyboard modifier state
    mods: ModifiersState,
}

impl MultermApp {
    pub fn new(proxy: EventLoopProxy<UserEvent>) -> Self {
        Self {
            proxy,
            window: None,
            gpu: None,
            compositor: None,
            atlas: None,
            panes: Vec::new(),
            active_pane: 0,
            split_x: 0.5,
            split_y: 0.5,
            dragging: None,
            selections: Vec::new(),
            selection_drag: None,
            cursor_pos: (0.0, 0.0),
            histories: Vec::new(),
            mods: ModifiersState::empty(),
        }
    }

    #[allow(dead_code)]
    fn tmux_startup_command_for_pane(pane_index: usize) -> Option<String> {
        // Opt out if desired.
        if std::env::var("MULTERM_TMUX_DISABLED").ok().as_deref() == Some("1") {
            return None;
        }

        // Session naming needs to be stable across app restarts so you can reattach.
        // Users can override the prefix if they run multiple multerm instances.
        let prefix =
            std::env::var("MULTERM_TMUX_SESSION_PREFIX").unwrap_or_else(|_| "multerm".into());

        // Sanitize to avoid weird tmux targeting behavior.
        let sanitize = |s: &str| {
            s.chars()
                .map(|c| match c {
                    'a'..='z' | 'A'..='Z' | '0'..='9' | '_' | '-' => c,
                    _ => '_',
                })
                .collect::<String>()
        };

        let session_name = format!("{}-pane-{}", sanitize(&prefix), pane_index);
        let session_name_q = session_name.replace('\'', "_"); // should be impossible after sanitize

        // `spawn_pty` runs the shell as `SHELL -lc <startup_command>`, so we must `exec`
        // the interactive thing we want to keep running.
        Some(format!(
            r#"
if command -v tmux >/dev/null 2>&1; then
  tmux has-session -t '{session}' 2>/dev/null || tmux new-session -s '{session}' -d
  exec tmux attach -t '{session}'
fi

# tmux isn't installed. Optionally try a best-effort install (macOS: Homebrew).
# This is opt-in to avoid surprising users / doing network installs silently.
if [ "${{MULTERM_TMUX_AUTO_INSTALL:-0}}" = "1" ] && command -v brew >/dev/null 2>&1; then
  echo "[multerm] tmux not found; installing via brew..."
  brew install tmux >/dev/null 2>&1 || brew install tmux || true
fi

if command -v tmux >/dev/null 2>&1; then
  tmux has-session -t '{session}' 2>/dev/null || tmux new-session -s '{session}' -d
  exec tmux attach -t '{session}'
fi

echo "[multerm] tmux is not installed; falling back to a normal shell."
exec "${{SHELL:-/bin/zsh}}" -i
"#,
            session = session_name_q
        ))
    }

    #[allow(dead_code)]
    fn compute_terminal_size(&self) -> (usize, usize) {
        let atlas = match &self.atlas {
            Some(a) => a,
            None => return (24, 80),
        };
        let gpu = match &self.gpu {
            Some(g) => g,
            None => return (24, 80),
        };
        let cw = atlas.cell_width().max(1.0);
        let ch = atlas.cell_height().max(1.0);
        let cols = (gpu.surface_config.width as f32 / cw).max(1.0) as usize;
        let rows = (gpu.surface_config.height as f32 / ch).max(1.0) as usize;
        (rows, cols)
    }

    fn daemon_session_key_for_pane(pane_index: usize) -> String {
        let prefix =
            std::env::var("MULTERM_DAEMON_SESSION_PREFIX").unwrap_or_else(|_| "multerm".into());

        // Sanitize to tmux-like charset so it is safe for the daemon.
        let sanitize = |s: &str| {
            s.chars()
                .map(|c| match c {
                    'a'..='z' | 'A'..='Z' | '0'..='9' | '_' | '-' => c,
                    _ => '_',
                })
                .collect::<String>()
        };

        format!("{}-pane-{}", sanitize(&prefix), pane_index)
    }

    fn read_frame_tcp(stream: &mut TcpStream) -> anyhow::Result<(u8, Vec<u8>)> {
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

    fn write_frame_tcp(
        stream: &mut TcpStream,
        frame_type: u8,
        payload: &[u8],
    ) -> anyhow::Result<()> {
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

    fn connect_daemon(&self) -> anyhow::Result<TcpStream> {
        if std::env::var("MULTERM_DAEMON_DISABLED").ok().as_deref() == Some("1") {
            anyhow::bail!("multerm daemon disabled");
        }

        // First, try the port file if it exists.
        let mut spawned = false;
        for _attempt in 0..30 {
            if let Ok(port_file) = crate::daemon::daemon_port_file_path() {
                if let Ok(port_s) = fs::read_to_string(&port_file) {
                    if let Ok(port) = port_s.trim().parse::<u16>() {
                        if let Ok(stream) = TcpStream::connect(("127.0.0.1", port)) {
                            return Ok(stream);
                        }
                    }
                }
            }

            if !spawned {
                spawned = true;
                let exe = std::env::current_exe()?;
                let _ = Command::new(exe)
                    .arg("--daemon")
                    .stdin(Stdio::null())
                    .stdout(Stdio::null())
                    .stderr(Stdio::null())
                    .spawn();
            }

            std::thread::sleep(Duration::from_millis(100));
        }

        anyhow::bail!("could not connect to multerm session daemon");
    }

    fn spawn_pane(&self, rows: usize, cols: usize, pane_index: usize) -> PaneRuntime {
        let (tx, rx) = unbounded::<Vec<u8>>();
        let session_key = Self::daemon_session_key_for_pane(pane_index);

        // Try the daemon first so the process can survive restarts.
        if let Ok(stream) = self.connect_daemon() {
            let mut reader = stream.try_clone().expect("clone daemon stream");
            let mut writer = stream;

            let cwd = std::env::current_dir()
                .ok()
                .and_then(|p| p.to_str().map(|s| s.to_string()));
            let payload = crate::daemon::attach_request_payload(
                &session_key,
                rows as u16,
                cols as u16,
                cwd.as_deref(),
            );

            if let Some(payload) = payload {
                if Self::write_frame_tcp(&mut writer, FRAME_ATTACH, &payload).is_ok() {
                    if let Ok((frame_type, first_payload)) = Self::read_frame_tcp(&mut reader) {
                        if frame_type == FRAME_OUTPUT {
                            let _ = tx.send(first_payload);
                            let _ = self.proxy.send_event(UserEvent::PtyData);

                            let proxy = self.proxy.clone();
                            let out_tx = tx.clone();
                            let wake_proxy = proxy.clone();
                            std::thread::spawn(move || loop {
                                let Ok((ft, payload)) = Self::read_frame_tcp(&mut reader) else {
                                    break;
                                };
                                if ft == FRAME_OUTPUT {
                                    let _ = out_tx.send(payload);
                                    let _ = wake_proxy.send_event(UserEvent::PtyData);
                                } else if ft == FRAME_ATTACH_ERROR {
                                    break;
                                }
                            });

                            let session = TerminalSession::new(PaneId::new(), rows, cols, rx);
                            return PaneRuntime {
                                session,
                                backend: PaneBackend::DaemonPty { writer },
                            };
                        }
                    }
                }
            }
        }

        // Fallback: local PTY (no persistence across restarts).
        let proxy = self.proxy.clone();
        let wake_up: Box<dyn Fn() + Send + 'static> = Box::new(move || {
            let _ = proxy.send_event(UserEvent::PtyData);
        });

        let shell = std::env::var("SHELL").unwrap_or_else(|_| "/bin/zsh".into());
        let pty = spawn_pty(&shell, rows as u16, cols as u16, tx, wake_up, None, None)
            .expect("spawn_pty");

        let session = TerminalSession::new(PaneId::new(), rows, cols, rx);
        PaneRuntime {
            session,
            backend: PaneBackend::LocalPty { pty },
        }
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
        let Some(atlas) = &self.atlas else {
            return;
        };
        let pane_rects = self.pane_layout();
        let cell_w = atlas.cell_width().max(1.0);
        let cell_h = atlas.cell_height().max(1.0);

        for (pane_idx, (pane, rect)) in self.panes.iter_mut().zip(pane_rects.iter()).enumerate() {
            let cols = (rect.w / cell_w).max(1.0) as usize;
            let rows = (rect.h / cell_h).max(1.0) as usize;
            pane.session.parser.resize(rows, cols);
            pane.backend.resize(rows as u16, cols as u16);

            if let Some(sel) = self.selections.get_mut(pane_idx).and_then(|s| s.as_mut()) {
                sel.clamp_to_grid(rows, cols);
            }
        }
    }

    fn add_new_terminal(&mut self) {
        if self.panes.len() >= MAX_PANES {
            return;
        }
        let pane_index = self.panes.len();
        self.panes.push(self.spawn_pane(24, 80, pane_index));
        self.selections.push(None);
        self.histories.push(PaneHistory::new());
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

    fn cell_pos_at_cursor_in_pane(&self, pane_idx: usize) -> Option<(usize, usize)> {
        let _ = self.gpu.as_ref()?;
        let atlas = self.atlas.as_ref()?;

        let pane_rects = self.pane_layout();
        let rect = pane_rects.get(pane_idx)?;
        let grid = self.panes.get(pane_idx)?.session.parser.grid();

        let cell_w = atlas.cell_width().max(1.0);
        let cell_h = atlas.cell_height().max(1.0);

        let local_x = (self.cursor_pos.0 - rect.x).max(0.0);
        let local_y = (self.cursor_pos.1 - rect.y).max(0.0);

        let mut col = (local_x / cell_w) as usize;
        let mut row = (local_y / cell_h) as usize;

        row = row.min(grid.rows.saturating_sub(1));
        col = col.min(grid.cols.saturating_sub(1));

        // Normalize wide glyph clicks: map the trailing half back to the leading cell.
        if grid.cell(row, col).wide == multerm_vt::WideKind::Trailing && col > 0 {
            col -= 1;
        }

        Some((row, col))
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

impl ApplicationHandler<UserEvent> for MultermApp {
    fn resumed(&mut self, event_loop: &ActiveEventLoop) {
        // ── Create window ─────────────────────────────────────────────────────
        let attrs = Window::default_attributes()
            .with_title("Multerm")
            .with_inner_size(LogicalSize::new(WINDOW_W, WINDOW_H));

        let window = Arc::new(event_loop.create_window(attrs).expect("create window"));
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
            let w = gpu.surface_config.width as f32;
            let h = gpu.surface_config.height as f32;
            ((h / ch).max(1.0) as usize, (w / cw).max(1.0) as usize)
        };

        self.gpu = Some(gpu);
        self.atlas = Some(atlas);
        self.compositor = Some(compositor);
        self.panes = vec![self.spawn_pane(rows, cols, 0)];
        self.selections = vec![None];
        self.histories = vec![PaneHistory::new()];
        self.selection_drag = None;
        self.active_pane = 0;

        tracing::info!("Multerm started — {}×{} cells", cols, rows);
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
                if let Some(drag) = self.selection_drag {
                    if let Some((row, col)) = self.cell_pos_at_cursor_in_pane(drag.pane_idx) {
                        self.selection_drag = Some(SelectionDrag {
                            pane_idx: drag.pane_idx,
                            start: drag.start,
                        });
                        self.selections[drag.pane_idx] = Some(SelectionRange {
                            start_row: drag.start.0,
                            start_col: drag.start.1,
                            end_row: row,
                            end_col: col,
                            active: true,
                        });
                        if let Some(window) = &self.window {
                            window.request_redraw();
                        }
                    }
                } else if let Some(active_drag) = self.dragging {
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
                        self.selection_drag = None;
                    } else if let Some(idx) =
                        self.pane_index_at(self.cursor_pos.0, self.cursor_pos.1)
                    {
                        self.active_pane = idx;
                        if let Some((row, col)) = self.cell_pos_at_cursor_in_pane(idx) {
                            self.selection_drag = Some(SelectionDrag {
                                pane_idx: idx,
                                start: (row, col),
                            });
                            self.selections[idx] = Some(SelectionRange {
                                start_row: row,
                                start_col: col,
                                end_row: row,
                                end_col: col,
                                active: true,
                            });
                        }
                        if let Some(window) = &self.window {
                            window.request_redraw();
                        }
                    }
                }
                if button == MouseButton::Left && state == ElementState::Released {
                    self.dragging = None;
                    self.selection_drag = None;
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
                for (pane, history) in self.panes.iter_mut().zip(self.histories.iter_mut()) {
                    if history.is_live() {
                        history.push(pane.session.parser.grid().clone());
                    }
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

impl MultermApp {
    fn active_selection(&self) -> Option<SelectionRange> {
        self.selections
            .get(self.active_pane)
            .and_then(|s| *s)
            .filter(|r| r.active)
    }

    fn select_all_in_active_pane(&mut self) {
        let Some(pane) = self.panes.get(self.active_pane) else {
            return;
        };
        let grid = pane.session.parser.grid();
        if let Some(sel) = clipboard::selection_range_select_input_block(grid) {
            self.selections[self.active_pane] = Some(sel);
        }

        if let Some(window) = &self.window {
            window.request_redraw();
        }
    }

    fn copy_active_selection_to_clipboard(&mut self) {
        let Some(range) = self.active_selection() else {
            return;
        };
        let Some(pane) = self.panes.get(self.active_pane) else {
            return;
        };
        let grid = pane.session.parser.grid();

        let text = clipboard::selection_to_plain_text(grid, range);
        if let Err(e) = clipboard::set_clipboard_text(&text) {
            tracing::warn!("failed to set clipboard text: {e}");
        }
    }

    fn copy_active_selection_rich_to_clipboard(&mut self) {
        let Some(range) = self.active_selection() else {
            return;
        };
        let Some(pane) = self.panes.get(self.active_pane) else {
            return;
        };
        let grid = pane.session.parser.grid();

        let text = clipboard::selection_to_ansi_sgr_text(grid, range);
        if let Err(e) = clipboard::set_clipboard_text(&text) {
            tracing::warn!("failed to set clipboard text: {e}");
        }
    }

    fn paste_clipboard_into_active_pane(&mut self) {
        let text = match clipboard::get_clipboard_text() {
            Ok(t) => t,
            Err(e) => {
                tracing::warn!("failed to read clipboard text: {e}");
                return;
            }
        };
        if text.is_empty() {
            return;
        }

        let bytes = clipboard::clipboard_text_to_pty_bytes(&text);
        if let Some(pane) = self.panes.get_mut(self.active_pane) {
            pane.backend.write_input(&bytes);
        }

        self.selections[self.active_pane] = None;
        if let Some(window) = &self.window {
            window.request_redraw();
        }
    }

    fn handle_keyboard(&mut self, event: KeyEvent) {
        if event.state == ElementState::Pressed {
            let ctrl = self.mods.control_key();
            let shift = self.mods.shift_key();
            let cmd = self.mods.super_key();

            let key_char = match &event.logical_key {
                Key::Character(s) => s.chars().next(),
                _ => None,
            };

            let has_sel = self.active_selection().is_some();

            // ── Snapshot undo / redo ──────────────────────────────────────────
            let is_undo = cmd && !shift && key_char.is_some_and(|c| c.eq_ignore_ascii_case(&'z'));
            let is_redo = cmd && shift && key_char.is_some_and(|c| c.eq_ignore_ascii_case(&'z'));

            if is_undo {
                if let Some(h) = self.histories.get_mut(self.active_pane) {
                    h.undo();
                }
                if let Some(window) = &self.window {
                    window.request_redraw();
                }
                return;
            }
            if is_redo {
                if let Some(h) = self.histories.get_mut(self.active_pane) {
                    h.redo();
                }
                if let Some(window) = &self.window {
                    window.request_redraw();
                }
                return;
            }

            // Any other key press while viewing a snapshot resumes live first.
            let in_snapshot = self
                .histories
                .get(self.active_pane)
                .map(|h| !h.is_live())
                .unwrap_or(false);
            if in_snapshot {
                if let Some(h) = self.histories.get_mut(self.active_pane) {
                    h.resume_live();
                }
                if let Some(window) = &self.window {
                    window.request_redraw();
                }
                // Fall through so the key is also sent normally.
            }

            let is_new_terminal_shortcut = matches!(event.logical_key, Key::Character(ref c) if c.eq_ignore_ascii_case("n"))
                && self.mods.shift_key()
                && (self.mods.control_key() || self.mods.super_key());
            if is_new_terminal_shortcut {
                self.add_new_terminal();
                if let Some(window) = &self.window {
                    window.request_redraw();
                }
                return;
            }

            // Select-all
            let is_select_all = (cmd && key_char.is_some_and(|c| c.eq_ignore_ascii_case(&'a')))
                || (ctrl && key_char.is_some_and(|c| c.eq_ignore_ascii_case(&'a')));
            if is_select_all {
                self.select_all_in_active_pane();
                return;
            }

            // Copy: plain (Cmd/Ctrl+C), rich ANSI (Cmd+Shift+C).
            let is_copy_rich =
                has_sel && cmd && shift && key_char.is_some_and(|c| c.eq_ignore_ascii_case(&'c'));
            let is_copy_plain = has_sel
                && ((cmd && !shift && key_char.is_some_and(|c| c.eq_ignore_ascii_case(&'c')))
                    || (ctrl && key_char.is_some_and(|c| c.eq_ignore_ascii_case(&'c'))));
            if is_copy_rich {
                self.copy_active_selection_rich_to_clipboard();
                return;
            }
            if is_copy_plain {
                self.copy_active_selection_to_clipboard();
                return;
            }

            // Prevent Cmd/Ctrl+C from inserting "c" when there is no selection.
            if (cmd || ctrl) && key_char.is_some_and(|c| c.eq_ignore_ascii_case(&'c')) && !has_sel {
                return;
            }

            // Paste
            let is_paste_cmd_v = cmd && key_char.is_some_and(|c| c.eq_ignore_ascii_case(&'v'));
            let is_paste_ctrl_shift_v =
                ctrl && shift && key_char.is_some_and(|c| c.eq_ignore_ascii_case(&'v'));
            let is_paste_ctrl_v =
                ctrl && !shift && key_char.is_some_and(|c| c.eq_ignore_ascii_case(&'v'));
            let is_paste_shift_insert =
                shift && matches!(event.logical_key, Key::Named(NamedKey::Insert));

            if is_paste_cmd_v || is_paste_ctrl_shift_v || is_paste_ctrl_v || is_paste_shift_insert {
                self.paste_clipboard_into_active_pane();
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
                if let Some(pane) = self.panes.get_mut(self.active_pane) {
                    pane.backend.write_input(&bytes);
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
        let pane_grids: Vec<([f32; 4], &multerm_vt::TerminalGrid)> = {
            let mut grids = Vec::with_capacity(self.panes.len());
            for (i, (pane, rect)) in self.panes.iter().zip(pane_rects.iter()).enumerate() {
                let grid = self
                    .histories
                    .get(i)
                    .and_then(|h| h.current_snapshot())
                    .unwrap_or_else(|| pane.session.parser.grid());
                grids.push((rect.as_array(), grid));
            }
            grids
        };

        let pane_selections: Vec<Option<SelectionRange>> = self
            .panes
            .iter()
            .enumerate()
            .map(|(i, _)| self.selections.get(i).copied().unwrap_or(None))
            .collect();
        let pane_cursors: Vec<Option<CursorState>> = self
            .panes
            .iter()
            .enumerate()
            .map(|(i, pane)| {
                if i != self.active_pane || !pane.session.parser.cursor_visible() {
                    return None;
                }
                let cursor = pane.session.parser.grid().cursor;
                Some(CursorState {
                    row: cursor.row,
                    col: cursor.col,
                    visible: true,
                })
            })
            .collect();

        if let Err(e) = compositor.render_terminal_panes(
            gpu,
            atlas,
            scale,
            &pane_grids,
            &pane_selections,
            &pane_cursors,
        ) {
            tracing::error!("render error: {e}");
        }
    }
}
