use crossbeam_channel::unbounded;
use eframe::egui::text::{LayoutJob, TextFormat};
use eframe::egui::{
    self, Align2, Color32, CursorIcon, FontFamily, FontId, Margin, Pos2, RichText, Sense, Shape,
    Stroke, TextEdit, Vec2, ViewportBuilder, ViewportClass, ViewportId,
};
use multerm_core::{pty::spawn_pty, session::TerminalSession, PaneId, PtyHandle};
use multerm_render::color::ansi_indexed_to_rgb;
use multerm_render::SelectionRange;
use multerm_vt::cell::{Cell, CellAttrs, Color, WideKind};
use multerm_vt::{TerminalGrid, TerminalParser};
use rfd::FileDialog;
use serde::{Deserialize, Serialize};
use std::{
    collections::hash_map::DefaultHasher,
    collections::HashSet,
    fs,
    hash::Hasher,
    io::{Read, Write},
    net::TcpStream,
    path::{Path, PathBuf},
    process::{Command, Stdio},
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};
use sysinfo::{
    get_current_pid, CpuRefreshKind, MemoryRefreshKind, ProcessRefreshKind, ProcessesToUpdate,
    RefreshKind, System,
};

mod clipboard;
mod daemon;
mod git_changes;
mod icon;

#[derive(Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
enum UiTheme {
    #[default]
    Dark,
    Light,
    Cyberpunk,
}

#[derive(Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
enum UiStyle {
    #[default]
    Normal,
    Glass,
}

/// Where the workspace switcher lives — either the horizontal tab strip on top
/// (default) or a vertical sidebar on the left.
#[derive(Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
enum WorkspacePlacement {
    #[default]
    Tabbed,
    Sidebar,
}

const WORKSPACE_SIDEBAR_MIN_WIDTH: f32 = 150.0;
const WORKSPACE_SIDEBAR_MAX_WIDTH: f32 = 480.0;
const WORKSPACE_SIDEBAR_DEFAULT_WIDTH: f32 = 260.0;

fn default_sidebar_width() -> f32 {
    WORKSPACE_SIDEBAR_DEFAULT_WIDTH
}

fn default_true() -> bool {
    true
}
fn default_f32_1_5() -> f32 {
    1.5
}
fn default_f32_0_28() -> f32 {
    0.28
}
fn default_f32_1_0() -> f32 {
    1.0
}

#[derive(Clone, Copy, Serialize, Deserialize)]
struct CyberpunkSettings {
    /// Master toggle — hides all mouse-driven light effects when false.
    #[serde(default = "default_true")]
    show_light: bool,
    /// Whether the light tracks the mouse at all.
    #[serde(default = "default_true")]
    follows_mouse: bool,
    /// When true, every pane tracks the mouse; otherwise only the focused pane does.
    #[serde(default)]
    all_panes: bool,
    /// Lerp speed toward the mouse (higher = snappier, lower = lazier).
    #[serde(default = "default_f32_1_5")]
    speed: f32,
    /// Gaussian beam width as a fraction of the pane diagonal.
    #[serde(default = "default_f32_0_28")]
    sigma: f32,
    /// Peak brightness multiplier (1.0 = default).
    #[serde(default = "default_f32_1_0")]
    brightness: f32,
    /// Atmospheric shimmer — organic drift even when mouse is still.
    #[serde(default = "default_true")]
    shimmer: bool,
    /// Shimmer amplitude multiplier (1.0 = default ~10 px max drift).
    #[serde(default = "default_f32_1_0")]
    shimmer_strength: f32,
    /// Show the outer gradient glow rings around the active pane.
    #[serde(default = "default_true")]
    show_halos: bool,
    /// Show the radial gradient blob that orbits the pane edge.
    #[serde(default = "default_true")]
    show_radial: bool,
    /// Show the perimeter orbit dots that follow the light source.
    #[serde(default = "default_true")]
    show_dots: bool,
    /// When true the radial glow sits directly at the cursor; when false it orbits the pane edge.
    #[serde(default)]
    follow_cursor: bool,
}

impl Default for CyberpunkSettings {
    fn default() -> Self {
        Self {
            show_light: true,
            follows_mouse: true,
            all_panes: false,
            speed: 1.5,
            sigma: 0.28,
            brightness: 1.0,
            shimmer: true,
            shimmer_strength: 1.0,
            show_halos: true,
            show_radial: true,
            show_dots: true,
            follow_cursor: false,
        }
    }
}

/// Upper bound on the row count in a fixed grid (default height hint for new panes).
const MAX_PANEL_GRID_ROWS: u8 = 8;

const LINE_EDITOR_MAX_HISTORY: usize = 100;
const WORKSPACE_EDIT_HISTORY_MAX: usize = 64;
const WORKSPACE_HISTORY_PANEL_MAX_ITEMS: usize = 8;
const WORKSPACE_HISTORY_PANEL_HOLD: Duration = Duration::from_secs(10);
const WORKSPACE_HISTORY_PANEL_FADE: Duration = Duration::from_millis(900);

/// A single snapshot of the line buffer: text content + cursor position.
#[derive(Clone)]
struct LineState {
    text: String,
    /// Char index from the start of `text` (0 = before first char).
    cursor: usize,
}

impl LineState {
    fn new() -> Self {
        Self {
            text: String::new(),
            cursor: 0,
        }
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
        Self {
            current: LineState::new(),
            undo_stack: Vec::new(),
            redo_stack: Vec::new(),
        }
    }

    fn cursor_byte_pos(&self) -> usize {
        self.current
            .text
            .char_indices()
            .nth(self.current.cursor)
            .map(|(b, _)| b)
            .unwrap_or(self.current.text.len())
    }

    fn char_before_cursor(&self) -> Option<char> {
        if self.current.cursor == 0 {
            return None;
        }
        self.current
            .text
            .char_indices()
            .nth(self.current.cursor - 1)
            .map(|(_, c)| c)
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
            let last_is_word = self
                .char_before_cursor()
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

    /// Insert pasted text as one undo step (does not split on word boundaries like [`push_text`]).
    fn push_paste(&mut self, text: &str) {
        if text.is_empty() {
            return;
        }
        self.push_snapshot();
        for ch in text.chars() {
            let byte_pos = self.cursor_byte_pos();
            self.current.text.insert(byte_pos, ch);
            self.current.cursor += 1;
        }
    }

    /// Replace the whole shadow line with pasted text (one undo step). Used when pasting over
    /// a host selection so undo matches "replaced region → new text".
    fn replace_with_paste(&mut self, text: &str) {
        self.push_snapshot();
        self.current.text.clear();
        self.current.text.push_str(text);
        self.current.cursor = text.chars().count();
    }

    fn push_backspace(&mut self) {
        if self.current.cursor == 0 {
            return;
        }
        self.push_snapshot();
        let (byte_start, _) = self
            .current
            .text
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
        if self.current.cursor < max {
            self.current.cursor += 1;
        }
    }

    fn move_to_start(&mut self) {
        self.current.cursor = 0;
    }

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

/// Per-terminal "find in scrollback" (search host buffer, not the PTY).
#[derive(Clone)]
struct ScrollbackSearchPaneState {
    open: bool,
    query: String,
    /// Index into the match list for the current `query` (wrapped by match count).
    current_match: usize,
}

impl Default for ScrollbackSearchPaneState {
    fn default() -> Self {
        Self {
            open: false,
            query: String::new(),
            current_match: 0,
        }
    }
}

fn scrollback_search_text_id(pane_id: u64) -> egui::Id {
    egui::Id::new(("multerm_scrollback_search", pane_id))
}

#[inline]
fn scrollback_chars_match(a: char, b: char, ascii_case_insensitive: bool) -> bool {
    if ascii_case_insensitive && a.is_ascii() && b.is_ascii() {
        return a.eq_ignore_ascii_case(&b);
    }
    a == b
}

/// Flatten the grid to characters plus a `(row, col)` anchor for each character (for mapping matches).
fn scrollback_flat_haystack(grid: &TerminalGrid) -> (Vec<char>, Vec<(usize, usize)>) {
    let mut chars = Vec::new();
    let mut map = Vec::new();
    let total = grid.total_rows();
    for vrow in 0..total {
        if vrow > 0 {
            chars.push('\n');
            // Anchor the synthetic newline on the **previous** row's last rendered column so
            // haystack indices never share `(vrow, col)` with the first cell of the next row.
            // Duplicate anchors corrupted match ranges and painted highlights on the wrong rows.
            let pv = vrow - 1;
            let pe = row_render_end_virtual(grid, pv).min(grid.cols);
            let anchor = if pe == 0 {
                0usize
            } else {
                let lc = (pe - 1).min(grid.cols.saturating_sub(1));
                snap_to_leading_cell_v(grid, pv, lc)
            };
            map.push((pv, anchor));
        }
        let end = row_render_end_virtual(grid, vrow).min(grid.cols);
        let mut col = 0usize;
        while col < end {
            let cell = grid.virtual_cell(vrow, col);
            if cell.wide == WideKind::Trailing {
                col += 1;
                continue;
            }
            chars.push(cell.ch);
            map.push((vrow, col));
            col += if cell.wide == WideKind::Leading && col + 1 < end {
                2
            } else {
                1
            };
        }
    }
    (chars, map)
}

fn scrollback_find_match_start_indices(
    hay: &[char],
    needle: &[char],
    ascii_case_insensitive: bool,
) -> Vec<usize> {
    if needle.is_empty() || hay.len() < needle.len() {
        return Vec::new();
    }
    let nlen = needle.len();
    let max_start = hay.len() - nlen;
    let mut out = Vec::new();
    'try_start: for i in 0..=max_start {
        for j in 0..nlen {
            if !scrollback_chars_match(hay[i + j], needle[j], ascii_case_insensitive) {
                continue 'try_start;
            }
        }
        out.push(i);
    }
    out
}

fn wide_span_end_col_v(grid: &TerminalGrid, vrow: usize, start_col: usize) -> usize {
    if grid.virtual_cell(vrow, start_col).wide == WideKind::Leading && start_col + 1 < grid.cols {
        start_col + 1
    } else {
        start_col
    }
}

fn scrollback_match_to_range(
    grid: &TerminalGrid,
    map: &[(usize, usize)],
    hay_len: usize,
    start_ci: usize,
    needle_len: usize,
) -> Option<SelectionRange> {
    if needle_len == 0
        || start_ci + needle_len > hay_len
        || map.len() != hay_len
        || start_ci + needle_len > map.len()
    {
        return None;
    }
    let (sr, sc) = map[start_ci];
    let (lr, lc) = map[start_ci + needle_len - 1];
    let ec = wide_span_end_col_v(grid, lr, lc);
    Some(SelectionRange {
        start_row: sr,
        start_col: sc,
        end_row: lr,
        end_col: ec,
        active: true,
    })
}

fn scrollback_compute_match_ranges(grid: &TerminalGrid, query: &str) -> Vec<SelectionRange> {
    if query.is_empty() {
        return Vec::new();
    }
    let (hay_chars, map) = scrollback_flat_haystack(grid);
    let needle: Vec<char> = query.chars().collect();
    let starts = scrollback_find_match_start_indices(&hay_chars, &needle, true);
    let hay_len = hay_chars.len();
    starts
        .into_iter()
        .filter_map(|i| scrollback_match_to_range(grid, &map, hay_len, i, needle.len()))
        .collect()
}

fn scrollback_search_advance_pane(
    pane: &TerminalPane,
    search_state: &mut ScrollbackSearchPaneState,
    delta: isize,
) {
    let grid = pane.session.parser.grid();
    let ranges = scrollback_compute_match_ranges(grid, &search_state.query);
    if ranges.is_empty() {
        return;
    }
    let n = ranges.len();
    let cur = search_state.current_match % n;
    search_state.current_match = (cur as isize + delta).rem_euclid(n as isize) as usize;
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
        Some(
            h.max(TERMINAL_MIN_HEIGHT)
                .min(viewport_h.max(TERMINAL_MIN_HEIGHT)),
        )
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
    tab_label_active: Color32,
    resize_grip_hot: Color32,
    resize_grip_cold: Color32,
    terminal_border_active: Color32,
    spawn_flash_rgb: [u8; 3],
    /// Override color for the active-tab bottom indicator line. `None` = derive from fill.
    tab_active_indicator: Option<Color32>,
    /// Outer glow color painted around the active terminal border. `None` = no glow.
    terminal_glow: Option<Color32>,
}

impl UiTheme {
    fn palette(self) -> UiPalette {
        match self {
            // Dark — "Midnight Indigo": deep navy-black with a warm amber accent line
            // and more saturated borders/actives for visual depth.
            UiTheme::Dark => UiPalette {
                bg: Color32::from_rgb(6, 7, 15),
                panel_bg: Color32::from_rgb(10, 13, 26),
                border: Color32::from_rgb(44, 64, 108),
                text: Color32::from_rgb(210, 222, 248),
                muted: Color32::from_rgb(112, 128, 172),
                tab_active_bg: Color32::from_rgb(28, 72, 148),
                tab_inactive_bg: Color32::from_rgb(13, 18, 38),
                tab_close: Color32::from_rgb(158, 176, 215),
                tab_close_hover_bg: Color32::from_rgb(122, 42, 56),
                tab_close_active_bg: Color32::from_rgb(150, 56, 72),
                tab_close_hover_text: Color32::from_rgb(255, 241, 246),
                path_bar_bg: Color32::from_rgb(9, 12, 24),
                path_bar_border: Color32::from_rgb(36, 54, 92),
                term_bg: Color32::from_rgb(4, 5, 12),
                vt_default_fg: Color32::from_rgb(215, 220, 238),
                header_strip: Color32::from_rgb(7, 9, 20),
                popover_fill: Color32::from_rgb(9, 12, 26),
                tab_label_active: Color32::WHITE,
                resize_grip_hot: Color32::from_rgb(112, 170, 255),
                resize_grip_cold: Color32::from_rgb(72, 116, 196),
                terminal_border_active: Color32::from_rgb(92, 152, 255),
                spawn_flash_rgb: [120, 180, 255],
                tab_active_indicator: Some(Color32::from_rgb(255, 175, 80)),
                terminal_glow: None,
            },
            // Light — "Warm Canvas": cream-tinted backgrounds and a terracotta accent
            // line instead of cold flat grey.
            UiTheme::Light => UiPalette {
                bg: Color32::from_rgb(240, 238, 234),
                panel_bg: Color32::from_rgb(228, 226, 221),
                border: Color32::from_rgb(158, 162, 180),
                text: Color32::from_rgb(22, 26, 42),
                muted: Color32::from_rgb(90, 98, 124),
                tab_active_bg: Color32::from_rgb(50, 100, 196),
                tab_inactive_bg: Color32::from_rgb(210, 208, 203),
                tab_close: Color32::from_rgb(86, 96, 118),
                tab_close_hover_bg: Color32::from_rgb(196, 76, 88),
                tab_close_active_bg: Color32::from_rgb(176, 56, 70),
                tab_close_hover_text: Color32::from_rgb(255, 245, 247),
                path_bar_bg: Color32::from_rgb(248, 246, 241),
                path_bar_border: Color32::from_rgb(178, 178, 194),
                term_bg: Color32::from_rgb(250, 248, 244),
                vt_default_fg: Color32::from_rgb(28, 32, 48),
                header_strip: Color32::from_rgb(222, 220, 214),
                popover_fill: Color32::from_rgb(246, 244, 239),
                tab_label_active: Color32::WHITE,
                resize_grip_hot: Color32::from_rgb(58, 118, 212),
                resize_grip_cold: Color32::from_rgb(128, 150, 196),
                terminal_border_active: Color32::from_rgb(46, 106, 210),
                spawn_flash_rgb: [70, 130, 220],
                tab_active_indicator: Some(Color32::from_rgb(210, 90, 50)),
                terminal_glow: None,
            },
            UiTheme::Cyberpunk => UiPalette {
                bg: Color32::from_rgb(3, 6, 14),
                panel_bg: Color32::from_rgb(6, 10, 22),
                border: Color32::from_rgb(14, 48, 62),
                text: Color32::from_rgb(200, 238, 248),
                muted: Color32::from_rgb(78, 148, 170),
                tab_active_bg: Color32::from_rgb(8, 50, 66),
                tab_inactive_bg: Color32::from_rgb(4, 12, 22),
                tab_close: Color32::from_rgb(88, 168, 190),
                tab_close_hover_bg: Color32::from_rgb(110, 30, 50),
                tab_close_active_bg: Color32::from_rgb(140, 40, 62),
                tab_close_hover_text: Color32::from_rgb(255, 240, 248),
                path_bar_bg: Color32::from_rgb(4, 8, 18),
                path_bar_border: Color32::from_rgb(18, 58, 74),
                term_bg: Color32::from_rgb(6, 16, 28),
                vt_default_fg: Color32::from_rgb(208, 240, 248),
                header_strip: Color32::from_rgb(3, 6, 15),
                popover_fill: Color32::from_rgb(4, 9, 20),
                tab_label_active: Color32::WHITE,
                resize_grip_hot: Color32::from_rgb(0, 218, 240),
                resize_grip_cold: Color32::from_rgb(0, 148, 172),
                terminal_border_active: Color32::from_rgb(0, 224, 248),
                spawn_flash_rgb: [0, 210, 235],
                tab_active_indicator: Some(Color32::from_rgb(0, 224, 248)),
                terminal_glow: Some(Color32::from_rgb(0, 224, 248)),
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

/// Dimmed overlay behind the uploaded-images modal (theme-aware).
fn image_gallery_modal_scrim(theme: UiTheme, p: UiPalette) -> Color32 {
    match theme {
        UiTheme::Light => Color32::from_rgba_unmultiplied(p.text.r(), p.text.g(), p.text.b(), 96),
        UiTheme::Dark => Color32::from_rgba_unmultiplied(0, 0, 0, 178),
        UiTheme::Cyberpunk => Color32::from_rgba_unmultiplied(
            p.bg.r(),
            p.bg.g().saturating_add(6),
            (p.bg.b() as u16 + 22).min(255) as u8,
            188,
        ),
    }
}

fn image_content_fingerprint(path: &str) -> Option<(u64, u64)> {
    let bytes = fs::read(path).ok()?;
    let mut hasher = DefaultHasher::new();
    hasher.write(&bytes);
    Some((bytes.len() as u64, hasher.finish()))
}

fn uploaded_images_contains_same_content(uploaded_images: &[String], candidate: &str) -> bool {
    if uploaded_images.iter().any(|p| p == candidate) {
        return true;
    }
    let Some((candidate_len, candidate_hash)) = image_content_fingerprint(candidate) else {
        return false;
    };
    uploaded_images.iter().any(|path| {
        image_content_fingerprint(path)
            .is_some_and(|(len, hash)| len == candidate_len && hash == candidate_hash)
    })
}

fn lighten_toward_white(c: Color32, mix: f32, alpha: u8) -> Color32 {
    let t = mix.clamp(0.0, 1.0);
    let lerp = |v: u8| -> u8 {
        (v as f32 + (255.0 - v as f32) * t)
            .round()
            .clamp(0.0, 255.0) as u8
    };
    Color32::from_rgba_unmultiplied(lerp(c.r()), lerp(c.g()), lerp(c.b()), alpha)
}

fn tab_auto_text_color(bg: Color32) -> Color32 {
    let to_linear = |component: u8| -> f32 {
        let s = component as f32 / 255.0;
        if s <= 0.04045 {
            s / 12.92
        } else {
            ((s + 0.055) / 1.055).powf(2.4)
        }
    };
    let r = to_linear(bg.r());
    let g = to_linear(bg.g());
    let b = to_linear(bg.b());
    let luminance = 0.2126 * r + 0.7152 * g + 0.0722 * b;
    if luminance > 0.38 {
        Color32::from_rgb(20, 24, 31)
    } else {
        Color32::from_rgb(245, 247, 252)
    }
}

fn apply_egui_visuals(ctx: &egui::Context, theme: UiTheme, p: UiPalette) {
    let mut visuals = match theme {
        UiTheme::Dark | UiTheme::Cyberpunk => egui::Visuals::dark(),
        UiTheme::Light => egui::Visuals::light(),
    };

    // Global fills
    visuals.override_text_color = Some(p.text);
    visuals.panel_fill = p.bg;
    visuals.window_fill = p.popover_fill; // popups use a distinct layered fill
    visuals.window_stroke = Stroke::new(1.0, p.border);
    visuals.faint_bg_color = p.panel_bg;
    visuals.hyperlink_color = p.terminal_border_active;

    // Text selection
    visuals.selection.bg_fill = Color32::from_rgba_unmultiplied(
        p.terminal_border_active.r(),
        p.terminal_border_active.g(),
        p.terminal_border_active.b(),
        72,
    );

    // Noninteractive (labels, separators, read-only)
    visuals.widgets.noninteractive.bg_fill = p.panel_bg;
    visuals.widgets.noninteractive.weak_bg_fill = p.panel_bg;
    visuals.widgets.noninteractive.bg_stroke = Stroke::new(1.0, p.border);

    // Inactive — buttons / checkboxes at rest
    visuals.widgets.inactive.bg_fill = p.tab_inactive_bg;
    visuals.widgets.inactive.weak_bg_fill = p.panel_bg;
    visuals.widgets.inactive.bg_stroke = Stroke::NONE;

    // Hovered — use border color as a mid-tone highlight
    visuals.widgets.hovered.bg_fill = p.border;
    visuals.widgets.hovered.weak_bg_fill = p.panel_bg;
    visuals.widgets.hovered.bg_stroke = Stroke::new(1.0, p.border);

    // Active / pressed
    visuals.widgets.active.bg_fill = p.tab_active_bg;
    visuals.widgets.active.weak_bg_fill = p.tab_active_bg;
    visuals.widgets.active.bg_stroke = Stroke::new(1.0, p.terminal_border_active);

    ctx.set_visuals(visuals);
    ctx.style_mut(|style| {
        style.interaction.tooltip_delay = 0.0;
        style.spacing.item_spacing = Vec2::new(8.0, 5.0);
        style.spacing.button_padding = Vec2::new(8.0, 4.0);
    });
}
const CELL_W: f32 = 9.0;
const CELL_H: f32 = 18.0;
/// Shift caret / search overlay up (negative Y) so the block aligns with monospace glyphs in the cell.
const TERMINAL_CELL_OVERLAY_Y_NUDGE: f32 = -2.5;
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
const RESIZE_SNAP_DISTANCE: f32 = 1.0;
const RESIZE_SNAP_OVERLAP_MIN: f32 = 0.0;
/// Pixels past the pane outer edge where the BR diagonal grip lives (outside the border).
const CORNER_GRIP_OUTSET: f32 = 2.0;
/// Extra radius around BR corner to show resize cursor early.
const BR_CURSOR_HOVER_RADIUS: f32 = 14.0;

/// Local-space `(left, right, top, bottom)` of the spawn-preview dashed frame, matching
/// [`MultermUi::paint_spawn_flash`]. `area_w` is the visible workspace width used for column
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

    let _ = tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::from_default_env()
                // egui_winit logs an ERROR when clipboard has image-only data (no text).
                // We handle that case ourselves, so suppress the noise.
                .add_directive("egui_winit::clipboard=off".parse().unwrap()),
        )
        .try_init();

    // Kick the daemon off in a background thread immediately so it's up and
    // accepting connections by the time Default::default() runs during window
    // creation.  This overlaps daemon startup with GPU/window init instead of
    // serialising them.
    if std::env::var("MULTERM_DAEMON_DISABLED").ok().as_deref() != Some("1") {
        if let Ok(exe) = std::env::current_exe() {
            // Only spawn if the daemon isn't already running.
            let already_up = daemon::daemon_port_file_path()
                .ok()
                .and_then(|p| fs::read_to_string(p).ok())
                .and_then(|s| s.trim().parse::<u16>().ok())
                .and_then(|port| TcpStream::connect(("127.0.0.1", port)).ok())
                .is_some();
            if !already_up {
                let _ = Command::new(exe)
                    .arg("--daemon")
                    .stdin(Stdio::null())
                    .stdout(Stdio::null())
                    .stderr(Stdio::null())
                    .spawn();
            }
        }
    }

    let mut viewport = egui::ViewportBuilder::default()
        .with_inner_size([1440.0, 860.0])
        .with_min_inner_size([1100.0, 700.0])
        .with_title("Multerm");
    if let Some(icon) = icon::load_egui_icon_data() {
        viewport = viewport.with_icon(icon);
    }
    let options = eframe::NativeOptions {
        viewport,
        ..Default::default()
    };

    eframe::run_native(
        "Multerm",
        options,
        Box::new(|_cc| Ok(Box::<MultermUi>::default())),
    )
}

// Session daemon protocol frame types (mirrors `multerm-app/src/daemon.rs`).
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
    agent_kind: TerminalAgentKind,
    tmux_session: String,
    session: TerminalSession,
    backend: TerminalBackend,
    desired_size: Vec2,
    position: Option<Pos2>,
    /// Scroll restored/new panes to the latest output the first time they render.
    pending_initial_scroll_to_bottom: bool,
    /// Last caret `(virtual_row, col)` we auto-scrolled to; `None` until first scroll this session.
    last_autoscroll_caret_v: Option<(usize, usize)>,
    /// Per-pane animated border-light position; only updated while this pane is active.
    border_light_pos: Option<Pos2>,
    /// Git paths attributed to this terminal session.
    git: git_changes::TerminalGitSession,
}

#[derive(Serialize, Deserialize, Clone)]
struct TerminalPaneState {
    #[serde(default)]
    id: u64,
    title: String,
    #[serde(default)]
    tmux_session: Option<String>,
    #[serde(default)]
    agent_kind: TerminalAgentKind,
    width: f32,
    height: f32,
    x: Option<f32>,
    y: Option<f32>,
}

#[derive(Serialize, Deserialize, Clone, Copy, PartialEq, Eq, Default)]
#[serde(rename_all = "snake_case")]
enum TerminalAgentKind {
    #[default]
    Terminal,
    Claude,
    Codex,
    Cursor,
}

impl TerminalAgentKind {
    fn from_command(command: &str) -> Option<Self> {
        match command {
            "claude" => Some(Self::Claude),
            "codex" => Some(Self::Codex),
            "cursor" => Some(Self::Cursor),
            _ => None,
        }
    }

    fn badge_bg(self) -> Color32 {
        match self {
            Self::Terminal => Color32::from_rgb(44, 110, 86),
            Self::Claude => Color32::from_rgb(214, 140, 114),
            Self::Codex => Color32::from_rgb(20, 20, 24),
            Self::Cursor => Color32::from_rgb(56, 56, 56),
        }
    }
}

const CLAUDE_ICON_PNG: &[u8] = include_bytes!("../assets/icons/agents/claude.png");
const CODEX_ICON_PNG: &[u8] = include_bytes!("../assets/icons/agents/codex.png");
const CURSOR_ICON_PNG: &[u8] = include_bytes!("../assets/icons/agents/cursor.png");

fn load_embedded_icon_texture(
    ctx: &egui::Context,
    texture_key: &str,
    bytes: &[u8],
) -> Option<egui::TextureHandle> {
    let dyn_img = image::load_from_memory(bytes).ok()?;
    let rgba = dyn_img.to_rgba8();
    let (w, h) = rgba.dimensions();
    if w == 0 || h == 0 {
        return None;
    }
    let color_image =
        egui::ColorImage::from_rgba_unmultiplied([w as usize, h as usize], rgba.as_raw());
    Some(ctx.load_texture(
        format!("agent_icon_{texture_key}"),
        color_image,
        egui::TextureOptions::LINEAR,
    ))
}

fn paint_terminal_agent_icon(
    painter: &egui::Painter,
    icon_rect: egui::Rect,
    kind: TerminalAgentKind,
    fg: Color32,
) {
    let c = icon_rect.center();
    let r = icon_rect.width().min(icon_rect.height()) * 0.5;
    match kind {
        TerminalAgentKind::Terminal => {
            // Generic terminal glyph.
            let w = r * 1.25;
            let h = r * 0.95;
            let rect = egui::Rect::from_center_size(c, Vec2::new(w, h));
            painter.rect_stroke(rect, 2.0, Stroke::new(1.2, fg), egui::StrokeKind::Inside);
            let y = c.y;
            painter.line_segment(
                [
                    Pos2::new(c.x - w * 0.28, y),
                    Pos2::new(c.x - w * 0.12, y + h * 0.14),
                ],
                Stroke::new(1.2, fg),
            );
            painter.line_segment(
                [
                    Pos2::new(c.x - w * 0.28, y),
                    Pos2::new(c.x - w * 0.12, y - h * 0.14),
                ],
                Stroke::new(1.2, fg),
            );
            painter.line_segment(
                [
                    Pos2::new(c.x - w * 0.02, y + h * 0.20),
                    Pos2::new(c.x + w * 0.28, y + h * 0.20),
                ],
                Stroke::new(1.2, fg),
            );
        }
        TerminalAgentKind::Claude => {
            // Claude-like starburst.
            let rays = 8;
            for i in 0..rays {
                let a = (i as f32) * std::f32::consts::TAU / (rays as f32);
                let dir = Vec2::angled(a);
                painter.line_segment(
                    [c - dir * (r * 0.18), c + dir * (r * 0.72)],
                    Stroke::new(1.35, fg),
                );
            }
            painter.circle_filled(c, r * 0.12, fg);
        }
        TerminalAgentKind::Codex => {
            // OpenAI-like knot mark from six linked chords.
            let n = 6usize;
            let mut pts = Vec::with_capacity(n);
            for i in 0..n {
                let a =
                    std::f32::consts::TAU * (i as f32) / (n as f32) - std::f32::consts::FRAC_PI_2;
                pts.push(c + Vec2::angled(a) * (r * 0.72));
            }
            for i in 0..n {
                let j = (i + 2) % n;
                painter.line_segment([pts[i], pts[j]], Stroke::new(1.15, fg));
            }
            painter.circle_stroke(c, r * 0.76, Stroke::new(1.0, fg.linear_multiply(0.85)));
        }
        TerminalAgentKind::Cursor => {
            // Cursor-like paper-plane/triangle glyph.
            let top = Pos2::new(c.x, c.y - r * 0.62);
            let left = Pos2::new(c.x - r * 0.70, c.y + r * 0.20);
            let right = Pos2::new(c.x + r * 0.70, c.y + r * 0.20);
            let inner = Pos2::new(c.x, c.y + r * 0.58);
            painter.add(Shape::convex_polygon(
                vec![top, left, right],
                fg,
                Stroke::NONE,
            ));
            painter.add(Shape::line_segment(
                [top, inner],
                Stroke::new(1.15, kind.badge_bg()),
            ));
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum WorkspaceTerminalCwdBlock {
    Empty,
    NotADir,
    Missing,
}

#[derive(Clone)]
struct WorkspaceSpawnNotice {
    message: String,
    /// When `Some`, show **Create folder** for `create_dir_all` on this path (parent must exist).
    create_target: Option<PathBuf>,
}

struct TabDragState {
    source_idx: usize,
    /// Current left-edge x of the ghost tab (follows the pointer).
    ghost_x: f32,
    /// Index into the "others" array (tabs excluding source) where the tab would be inserted.
    insert_before: usize,
}

struct MultermUi {
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
    select_all_workspace_input_on_focus: bool,
    color_history: Vec<[u8; 4]>,
    color_hex_target_idx: Option<usize>,
    color_hex_input: String,
    color_picker_target_idx: Option<usize>,
    color_picker_draft: Color32,
    color_picker_original_rgba: Option<[u8; 4]>,
    color_picker_rendered_this_frame: bool,
    editing_working_dir: bool,
    /// After opening the path editor, focus the field once (avoid stealing focus every frame).
    working_dir_editor_focus_next_frame: bool,
    working_dir_input: String,
    pending_terminal_spawn_pos: Option<Pos2>,
    pending_context_terminal: Option<usize>,
    pending_spawn_flash_pos: Option<Pos2>,
    pending_spawn_flash_until: Option<Instant>,
    /// Shown when a new terminal cannot spawn (missing / invalid workspace folder).
    workspace_terminal_spawn_notice: Option<WorkspaceSpawnNotice>,
    workspace_terminal_spawn_notice_until: Option<Instant>,
    /// Live CPU / RAM / load readings (`sysinfo`).
    system: System,
    system_last_sample: Instant,
    /// Smoothed UI repaint rate from egui frame delta (shown in Multerm usage panel).
    ui_fps_smoothed: f32,
    /// Open usage panels, ordered from oldest click to newest click.
    /// `true` => multerm panel, `false` => system panel.
    usage_panel_open_order: Vec<bool>,
    show_multerm_only_status: bool,
    equal_size_picker_open: bool,
    equal_size_picker_selection: Option<u64>,
    equal_size_template_blink_terminal_id: Option<u64>,
    equal_size_template_blink_started_at: Option<Instant>,
    /// Flush workspace JSON periodically so abrupt quits still persist open terminals.
    workspace_autosave_deadline: Instant,
    /// Active tab drag state; `None` when no drag is in progress.
    tab_drag: Option<TabDragState>,
    /// When the focused terminal changes, scrollback find UI closes (see `update`).
    prev_terminal_focus_key: Option<(usize, Option<usize>)>,
    /// Terminals popped out into their own native fullscreen window (double-click pane title).
    fullscreen_terminal_ids: HashSet<u64>,
    workspace_edit_histories: Vec<WorkspaceEditHistory>,
    workspace_history_suspended: bool,
    cyberpunk_settings: CyberpunkSettings,
    /// Disables all continuous animations while keeping the visual style intact.
    performance_mode: bool,
    /// Cached egui textures for image gallery thumbnails, keyed by file path.
    image_gallery_textures: std::collections::HashMap<String, egui::TextureHandle>,
    agent_icon_claude: Option<egui::TextureHandle>,
    agent_icon_codex: Option<egui::TextureHandle>,
    agent_icon_cursor: Option<egui::TextureHandle>,
    /// In-app fullscreen preview from the gallery “View” action (`load_image_thumbnail` …, `view`).
    image_gallery_view: Option<(String, egui::TextureHandle)>,
    /// Paths of images currently selected in the gallery modal.
    image_gallery_selected: std::collections::HashSet<String>,
    /// Active rubber-band drag: (start_pos, current_pos) in screen coordinates.
    image_gallery_rubber_band: Option<(egui::Pos2, egui::Pos2)>,
    /// Thumbnail screen rects from the last gallery frame, used for rubber-band hit-testing.
    image_gallery_thumb_rects: Vec<(String, egui::Rect)>,
    /// Path of the last thumbnail that was clicked, for Shift+click range selection.
    image_gallery_last_clicked: Option<String>,
    /// Ephemeral “focus a terminal first” bubble above the footer Uploaded Images control.
    uploaded_images_no_terminal_hint_until: Option<Instant>,
    /// Transient Photoshop-like feed that shows recent undo/redo hits.
    workspace_history_overlay_entries: Vec<WorkspaceHistoryOverlayEntry>,
    /// Where the workspace switcher lives (top tab strip vs. left sidebar).
    workspace_placement: WorkspacePlacement,
    /// In sidebar mode, whether the sidebar is currently expanded.
    workspace_sidebar_visible: bool,
    /// Persisted sidebar width (clamped to [`WORKSPACE_SIDEBAR_MIN_WIDTH`,
    /// `WORKSPACE_SIDEBAR_MAX_WIDTH`]). Updated each frame from the actual
    /// rendered SidePanel width so user resizes survive restart.
    workspace_sidebar_width: f32,
    /// Sidebar search input (currently visual only — does not filter the list).
    workspace_sidebar_search: String,
    /// Filesystem watchers for git repos (debounced path batches).
    git_repo_watchers: git_changes::GitRepoWatcherHub,
    /// VS Code–style source control panel (per-terminal or all terminals).
    git_changes_panel: Option<git_changes::GitChangesPanelState>,
    /// Frame cache for the changes panel — refreshed on open, on selection
    /// change, on filesystem events, and after commits. Avoids re-shelling
    /// out to `git` every frame while the modal is up.
    git_changes_cache: Option<git_changes::GitChangesPanelCache>,
    /// When true, the cache will be rebuilt on the next render.
    git_changes_cache_dirty: bool,
}

struct WorkspaceRuntime {
    terminals: Vec<TerminalPane>,
    active_terminal: Option<usize>,
    equal_size_source_terminal_id: Option<u64>,
    selections: Vec<Option<SelectionRange>>,
    line_editors: Vec<LineEditor>,
    scrollback_searches: Vec<ScrollbackSearchPaneState>,
    /// After Ctrl/Cmd+F, focus the search field once (uses `Cell` for nested UI borrows).
    scrollback_search_focus_pane: std::cell::Cell<Option<u64>>,
    /// Image paths for the gallery (persisted per workspace in workspace state JSON).
    uploaded_images: Vec<String>,
    show_image_gallery: bool,
    /// Screen rect of the active terminal pane, captured each render frame.
    active_terminal_rect: Option<egui::Rect>,
}

impl Default for WorkspaceRuntime {
    fn default() -> Self {
        Self {
            terminals: Vec::new(),
            active_terminal: None,
            equal_size_source_terminal_id: None,
            selections: Vec::new(),
            line_editors: Vec::new(),
            scrollback_searches: Vec::new(),
            scrollback_search_focus_pane: std::cell::Cell::new(None),
            uploaded_images: Vec::new(),
            show_image_gallery: false,
            active_terminal_rect: None,
        }
    }
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
    show_multerm_only_status: bool,
    #[serde(default)]
    cyberpunk_settings: CyberpunkSettings,
    #[serde(default)]
    performance_mode: bool,
    #[serde(default)]
    workspace_placement: WorkspacePlacement,
    #[serde(default = "default_true")]
    workspace_sidebar_visible: bool,
    #[serde(default = "default_sidebar_width")]
    workspace_sidebar_width: f32,
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
    #[serde(default)]
    uploaded_images: Vec<String>,
}

#[derive(Clone)]
struct WorkspaceEditHistory {
    undo_stack: Vec<WorkspaceTabState>,
    redo_stack: Vec<WorkspaceTabState>,
    current: Option<WorkspaceTabState>,
}

#[derive(Clone, Copy)]
enum WorkspaceHistoryOverlayAction {
    Undo,
    Redo,
}

#[derive(Clone)]
struct WorkspaceHistoryOverlayEntry {
    label: String,
    prev_label: Option<String>,
    next_label: Option<String>,
    at: Instant,
}

impl Default for WorkspaceEditHistory {
    fn default() -> Self {
        Self {
            undo_stack: Vec::new(),
            redo_stack: Vec::new(),
            current: None,
        }
    }
}

impl Default for MultermUi {
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
                select_all_workspace_input_on_focus: false,
                color_history: state.color_history,
                color_hex_target_idx: None,
                color_hex_input: String::new(),
                color_picker_target_idx: None,
                color_picker_draft: theme_palette.tab_active_bg,
                color_picker_original_rgba: None,
                color_picker_rendered_this_frame: false,
                editing_working_dir: false,
                working_dir_editor_focus_next_frame: false,
                working_dir_input: String::new(),
                pending_terminal_spawn_pos: None,
                pending_context_terminal: None,
                pending_spawn_flash_pos: None,
                pending_spawn_flash_until: None,
                workspace_terminal_spawn_notice: None,
                workspace_terminal_spawn_notice_until: None,
                system: system_status_probe_new(),
                system_last_sample: Instant::now() - SYSTEM_STATUS_SAMPLE_INTERVAL,
                ui_fps_smoothed: 0.0,
                usage_panel_open_order: if state.usage_panel_open_order.is_empty() {
                    state.usage_panel_pinned_scope.into_iter().collect()
                } else {
                    state.usage_panel_open_order
                },
                show_multerm_only_status: state.show_multerm_only_status,
                equal_size_picker_open: false,
                equal_size_picker_selection: None,
                equal_size_template_blink_terminal_id: None,
                equal_size_template_blink_started_at: None,
                workspace_autosave_deadline: Instant::now(),
                tab_drag: None,
                prev_terminal_focus_key: None,
                fullscreen_terminal_ids: HashSet::new(),
                workspace_edit_histories: (0..runtime_count)
                    .map(|_| WorkspaceEditHistory::default())
                    .collect(),
                workspace_history_suspended: false,
                cyberpunk_settings: state.cyberpunk_settings,
                performance_mode: state.performance_mode,
                image_gallery_textures: std::collections::HashMap::new(),
                agent_icon_claude: None,
                agent_icon_codex: None,
                agent_icon_cursor: None,
                image_gallery_view: None,
                image_gallery_selected: std::collections::HashSet::new(),
                image_gallery_rubber_band: None,
                image_gallery_thumb_rects: Vec::new(),
                image_gallery_last_clicked: None,
                uploaded_images_no_terminal_hint_until: None,
                workspace_history_overlay_entries: Vec::new(),
                workspace_placement: state.workspace_placement,
                workspace_sidebar_visible: state.workspace_sidebar_visible,
                workspace_sidebar_width: state
                    .workspace_sidebar_width
                    .clamp(WORKSPACE_SIDEBAR_MIN_WIDTH, WORKSPACE_SIDEBAR_MAX_WIDTH),
                workspace_sidebar_search: String::new(),
                git_repo_watchers: git_changes::GitRepoWatcherHub::default(),
                git_changes_panel: None,
                git_changes_cache: None,
                git_changes_cache_dirty: false,
            };
            // Pre-warm the daemon once before the restore loop so each pane's
            // connect_daemon() call finds it already running instead of blocking
            // serially on the 100 ms × 30 retry loop per pane.
            warm_up_daemon();
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
                        pane.agent_kind = pane_state.agent_kind;
                        app.next_terminal_id = terminal_id + 1;
                        pane.desired_size = Vec2::new(
                            pane_state.width.max(TERMINAL_MIN_WIDTH),
                            pane_state.height.max(TERMINAL_MIN_HEIGHT),
                        );
                        pane.position = match (pane_state.x, pane_state.y) {
                            (Some(x), Some(y)) => Some(Pos2::new(x.max(0.0), y.max(0.0))),
                            _ => None,
                        };
                        if let Some(root) = &pane.git.repo_root {
                            app.git_repo_watchers.ensure_watching(root);
                        }
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
                        runtime.equal_size_source_terminal_id =
                            saved_tab.equal_size_source_terminal_id;
                        runtime.uploaded_images = saved_tab.uploaded_images.clone();
                    }
                }
            }
            app.sync_all_workspace_history_snapshots();
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
            select_all_workspace_input_on_focus: false,
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
            working_dir_editor_focus_next_frame: false,
            working_dir_input: String::new(),
            pending_terminal_spawn_pos: None,
            pending_context_terminal: None,
            pending_spawn_flash_pos: None,
            pending_spawn_flash_until: None,
            workspace_terminal_spawn_notice: None,
            workspace_terminal_spawn_notice_until: None,
            system: system_status_probe_new(),
            system_last_sample: Instant::now() - SYSTEM_STATUS_SAMPLE_INTERVAL,
            ui_fps_smoothed: 0.0,
            usage_panel_open_order: Vec::new(),
            show_multerm_only_status: false,
            equal_size_picker_open: false,
            equal_size_picker_selection: None,
            equal_size_template_blink_terminal_id: None,
            equal_size_template_blink_started_at: None,
            workspace_autosave_deadline: Instant::now(),
            tab_drag: None,
            prev_terminal_focus_key: None,
            fullscreen_terminal_ids: HashSet::new(),
            workspace_edit_histories: (0..5).map(|_| WorkspaceEditHistory::default()).collect(),
            workspace_history_suspended: false,
            cyberpunk_settings: CyberpunkSettings::default(),
            performance_mode: false,
            image_gallery_textures: std::collections::HashMap::new(),
            agent_icon_claude: None,
            agent_icon_codex: None,
            agent_icon_cursor: None,
            image_gallery_view: None,
            image_gallery_selected: std::collections::HashSet::new(),
            image_gallery_rubber_band: None,
            image_gallery_thumb_rects: Vec::new(),
            image_gallery_last_clicked: None,
            uploaded_images_no_terminal_hint_until: None,
            workspace_history_overlay_entries: Vec::new(),
            workspace_placement: WorkspacePlacement::default(),
            workspace_sidebar_visible: true,
            workspace_sidebar_width: WORKSPACE_SIDEBAR_DEFAULT_WIDTH,
            workspace_sidebar_search: String::new(),
            git_repo_watchers: git_changes::GitRepoWatcherHub::default(),
            git_changes_panel: None,
            git_changes_cache: None,
            git_changes_cache_dirty: false,
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

fn paint_workspace_terminal_spawn_notice_bar(ui: &mut egui::Ui, app: &mut MultermUi) {
    let (message, create_target) = match &app.workspace_terminal_spawn_notice {
        Some(n) => (n.message.clone(), n.create_target.clone()),
        None => return,
    };
    let (fill, stroke, text) = match app.ui_theme {
        UiTheme::Dark | UiTheme::Cyberpunk => (
            Color32::from_rgb(52, 28, 28),
            Color32::from_rgb(180, 90, 90),
            Color32::from_rgb(255, 200, 200),
        ),
        UiTheme::Light => (
            Color32::from_rgb(255, 235, 235),
            Color32::from_rgb(200, 100, 100),
            Color32::from_rgb(90, 35, 35),
        ),
    };
    let dismiss_notice = egui::Frame::default()
        .fill(fill)
        .stroke(Stroke::new(1.0, stroke))
        .corner_radius(4.0)
        .inner_margin(Margin::symmetric(10, 6))
        .show(ui, |ui| {
            let mut dismiss = false;
            ui.vertical(|ui| {
                ui.add(egui::Label::new(RichText::new(&message).size(12.0).color(text)).wrap());
                ui.add_space(6.0);
                ui.horizontal(|ui| {
                    if let Some(ref path) = create_target {
                        if ui
                            .button(RichText::new("Create folder").size(12.0).color(text))
                            .on_hover_text("Create this folder and any missing parents")
                            .clicked()
                        {
                            let _ = fs::create_dir_all(path);
                            if path.is_dir() {
                                let idx = app
                                    .selected_workspace
                                    .min(app.workspaces.len().saturating_sub(1));
                                if let Some(w) = app.workspaces.get_mut(idx) {
                                    w.working_dir = path.to_string_lossy().into_owned();
                                    save_workspace_state(app);
                                }
                                dismiss = true;
                            }
                        }
                        ui.add_space(8.0);
                    }
                    if ui
                        .button(RichText::new("Choose folder…").size(12.0).color(text))
                        .on_hover_text("Open the folder picker")
                        .clicked()
                    {
                        let idx = app
                            .selected_workspace
                            .min(app.workspaces.len().saturating_sub(1));
                        let displayed_dir = app
                            .workspaces
                            .get(idx)
                            .map(|w| w.working_dir.clone())
                            .unwrap_or_else(default_working_dir);
                        let mut dialog = FileDialog::new();
                        if PathBuf::from(&displayed_dir).is_dir() {
                            dialog = dialog.set_directory(&displayed_dir);
                        }
                        if let Some(folder) = dialog.pick_folder() {
                            if let Some(path_s) = folder.to_str() {
                                if let Some(w) = app.workspaces.get_mut(idx) {
                                    w.working_dir = path_s.to_string();
                                    save_workspace_state(app);
                                }
                                app.editing_working_dir = false;
                                app.working_dir_editor_focus_next_frame = false;
                                app.working_dir_input.clear();
                                dismiss = true;
                            }
                        }
                    }
                });
            });
            dismiss
        })
        .inner;
    if dismiss_notice {
        app.workspace_terminal_spawn_notice = None;
        app.workspace_terminal_spawn_notice_until = None;
    }
    ui.add_space(6.0);
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

/// Unified diff view (VS Code inline style) inside a scroll area.
fn paint_unified_diff(ui: &mut egui::Ui, lines: &[git_changes::DiffLine], p: UiPalette) {
    const LINE_H: f32 = 18.0;
    let removed_bg = Color32::from_rgba_unmultiplied(180, 60, 60, 55);
    let added_bg = Color32::from_rgba_unmultiplied(60, 160, 90, 55);
    let hunk_bg = Color32::from_rgba_unmultiplied(80, 120, 200, 40);
    let font = FontId::monospace(11.0);
    let row_w = ui.available_width().max(200.0);

    if lines.is_empty() {
        ui.label(RichText::new("(no diff)").color(p.muted).size(12.0));
        return;
    }

    for line in lines {
        let (bg, fg, prefix, gutter) = match line.kind {
            git_changes::DiffLineKind::Add => (
                added_bg,
                Color32::from_rgb(140, 230, 160),
                "+",
                line.new_line
                    .map(|n| format!("{n:>5}"))
                    .unwrap_or_else(|| "     ".to_string()),
            ),
            git_changes::DiffLineKind::Remove => (
                removed_bg,
                Color32::from_rgb(255, 150, 150),
                "-",
                line.old_line
                    .map(|n| format!("{n:>5}"))
                    .unwrap_or_else(|| "     ".to_string()),
            ),
            git_changes::DiffLineKind::Hunk => (
                hunk_bg,
                p.tab_active_indicator.unwrap_or(p.muted),
                "@",
                "     ".to_string(),
            ),
            git_changes::DiffLineKind::Context => (
                Color32::TRANSPARENT,
                p.text,
                " ",
                match (line.old_line, line.new_line) {
                    (Some(o), Some(n)) => format!("{o:>5} {n:>5}"),
                    (Some(o), None) => format!("{o:>5}"),
                    (None, Some(n)) => format!("     {n:>5}"),
                    _ => "          ".to_string(),
                },
            ),
        };

        let (rect, _) = ui.allocate_exact_size(egui::vec2(row_w, LINE_H), Sense::hover());
        if bg != Color32::TRANSPARENT {
            ui.painter().rect_filled(rect, 0.0, bg);
        }
        let display = if line.kind == git_changes::DiffLineKind::Hunk {
            line.text.clone()
        } else {
            format!("{gutter} {prefix} {}", line.text)
        };
        ui.painter().text(
            rect.left_top() + egui::vec2(6.0, 2.0),
            Align2::LEFT_TOP,
            display,
            font.clone(),
            fg,
        );
    }
}

fn new_terminal_context_menu(
    ui: &mut egui::Ui,
    app: &mut MultermUi,
    target_terminal: Option<usize>,
) {
    let mut changed = false;
    if ui.button("New Terminal").clicked() {
        let spawn_pos = app.pending_terminal_spawn_pos.take();
        let anchor_terminal = app.pending_context_terminal.take().or(target_terminal);
        let _ = app.add_terminal(ui.ctx(), spawn_pos, anchor_terminal);
        app.pending_context_terminal = None;
        ui.close();
    }
    if ui.button("New Claude Code").clicked() {
        let spawn_pos = app.pending_terminal_spawn_pos.take();
        let anchor_terminal = app.pending_context_terminal.take().or(target_terminal);
        if app.add_terminal(ui.ctx(), spawn_pos, anchor_terminal) {
            app.launch_cli_tool(ui.ctx(), None, "claude");
        }
        app.pending_context_terminal = None;
        ui.close();
    }
    if ui.button("New Codex").clicked() {
        let spawn_pos = app.pending_terminal_spawn_pos.take();
        let anchor_terminal = app.pending_context_terminal.take().or(target_terminal);
        if app.add_terminal(ui.ctx(), spawn_pos, anchor_terminal) {
            app.launch_cli_tool(ui.ctx(), None, "codex");
        }
        app.pending_context_terminal = None;
        ui.close();
    }
    if is_cli_command_available("gemini") && ui.button("New Gemini").clicked() {
        let spawn_pos = app.pending_terminal_spawn_pos.take();
        let anchor_terminal = app.pending_context_terminal.take().or(target_terminal);
        if app.add_terminal(ui.ctx(), spawn_pos, anchor_terminal) {
            app.launch_cli_tool(ui.ctx(), None, "gemini");
        }
        app.pending_context_terminal = None;
        ui.close();
    }
    ui.separator();
    if app.active_workspace_tab_mut().is_some() {
        let mut panel_layout = app.active_panel_layout();
        let ws_idx = app
            .selected_workspace
            .min(app.workspaces.len().saturating_sub(1));
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

impl eframe::App for MultermUi {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        // Keep refreshing so PTY output appears live without explicit wakeups.
        self.sync_all_workspace_runtime_buffers();
        let focus_key = self.active_workspace_runtime().map(|r| {
            let ws = self
                .selected_workspace
                .min(self.workspaces.len().saturating_sub(1));
            (ws, r.active_terminal)
        });
        if self.prev_terminal_focus_key != focus_key {
            for rt in &mut self.workspace_runtime {
                for st in &mut rt.scrollback_searches {
                    st.open = false;
                }
            }
            self.prev_terminal_focus_key = focus_key;
        }
        ctx.request_repaint_after(Duration::from_millis(16));

        let dt = ctx.input(|i| i.stable_dt);
        if dt > 0.0 {
            let instant_fps = 1.0 / dt;
            const FPS_SMOOTH: f32 = 0.12;
            self.ui_fps_smoothed = if self.ui_fps_smoothed <= 0.0 {
                instant_fps
            } else {
                self.ui_fps_smoothed + FPS_SMOOTH * (instant_fps - self.ui_fps_smoothed)
            };
        }

        // Per-pane light positions are updated inside the render loop (only for the active pane).
        // Atmospheric shimmer constants are computed once here and used during rendering.
        let light_dt = ctx.input(|i| i.stable_dt).min(0.1);
        let light_target = ctx.input(|i| i.pointer.hover_pos());
        let light_t_now = ctx.input(|i| i.time) as f32;
        let cs = self.cyberpunk_settings;
        let perf = self.performance_mode;
        let (light_atmos_dx, light_atmos_dy) = if !perf && cs.show_light && cs.shimmer {
            let s = cs.shimmer_strength;
            let dx = s
                * (5.0 * (light_t_now * 0.71).sin()
                    + 3.5 * (light_t_now * 1.47 + 1.1).sin()
                    + 2.0 * (light_t_now * 2.83 + 0.4).cos());
            let dy = s
                * (5.0 * (light_t_now * 0.89).cos()
                    + 3.5 * (light_t_now * 1.73 + 0.7).cos()
                    + 2.0 * (light_t_now * 3.07 + 1.9).sin());
            (dx, dy)
        } else {
            (0.0, 0.0)
        };

        let p = self.ui_theme.palette().with_style(self.ui_style);
        self.ensure_agent_icon_textures(ctx);
        let agent_icon_claude = self.agent_icon_claude.clone();
        let agent_icon_codex = self.agent_icon_codex.clone();
        let agent_icon_cursor = self.agent_icon_cursor.clone();

        self.drain_terminals();
        self.poll_git_file_events();
        self.tick_workspace_terminal_spawn_notice();
        self.tick_uploaded_images_no_terminal_hint();
        self.tick_workspace_history_overlay();
        self.handle_keyboard_input(ctx);
        self.render_image_gallery(ctx, p);
        self.render_git_changes_panel(ctx, p);
        self.color_picker_rendered_this_frame = false;
        self.refresh_system_status_if_due();

        let cyber = self.cyberpunk_settings;
        let perf_mode = self.performance_mode;
        apply_egui_visuals(ctx, self.ui_theme, p);

        // Height follows content (PanelState). Avoid `exact_height`: it pins `height_range`
        // and forces a tall inner `min_height`, leaving a large empty band when no alert row.
        egui::TopBottomPanel::top("workspace_tabs")
            .resizable(false)
            .default_height(96.0)
            .frame(
                egui::Frame::default()
                    .fill(p.header_strip)
                    .stroke(Stroke::NONE),
            )
            .show(ctx, |ui| {
                // Register the background sense FIRST so widgets added later take priority.
                let header_click_clear = ui.interact(
                    ui.max_rect(),
                    ui.id().with("workspace_header_focus_clear"),
                    Sense::click(),
                );
                header_tabs(ui, self, p);
                directory_path_bar(ui, self, p);
                if header_click_clear.clicked() {
                    clear_active_workspace_terminal_focus(self);
                }
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
                    // Left side: uploaded images gallery button
                    let ws = self
                        .selected_workspace
                        .min(self.workspaces.len().saturating_sub(1));
                    if let Some(runtime) = self.workspace_runtime.get(ws) {
                        if !runtime.uploaded_images.is_empty() {
                            let has_focused_terminal = runtime
                                .active_terminal
                                .is_some_and(|idx| idx < runtime.terminals.len());
                            let label = format!(
                                "\u{1f5bc} Uploaded Images ({})",
                                runtime.uploaded_images.len()
                            );
                            let is_open = runtime.show_image_gallery;
                            let accent = p.tab_active_indicator.unwrap_or(p.resize_grip_hot);
                            let btn_color = if is_open { accent } else { p.muted };
                            let btn = ui
                                .add(
                                    egui::Label::new(
                                        RichText::new(label).size(11.0).color(btn_color),
                                    )
                                    .sense(Sense::click()),
                                )
                                .on_hover_cursor(CursorIcon::PointingHand);
                            if btn.clicked() {
                                if has_focused_terminal {
                                    self.uploaded_images_no_terminal_hint_until = None;
                                    if let Some(rt) = self.workspace_runtime.get_mut(ws) {
                                        rt.show_image_gallery = !rt.show_image_gallery;
                                        if !rt.show_image_gallery {
                                            self.image_gallery_view = None;
                                        }
                                    }
                                } else {
                                    if let Some(rt) = self.workspace_runtime.get_mut(ws) {
                                        rt.show_image_gallery = false;
                                    }
                                    self.image_gallery_view = None;
                                    self.uploaded_images_no_terminal_hint_until =
                                        Some(Instant::now() + std::time::Duration::from_secs(5));
                                }
                            }
                            if self
                                .uploaded_images_no_terminal_hint_until
                                .is_some_and(|until| Instant::now() < until)
                            {
                                self.render_uploaded_images_no_terminal_hint(ctx, btn.rect, p);
                            }
                        }
                    }

                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        let term_resp = ui
                            .add(
                                egui::Label::new(RichText::new("Multerm").size(11.0).color(p.text))
                                    .sense(Sense::click()),
                            )
                            .on_hover_cursor(CursorIcon::PointingHand);
                        if term_resp.clicked() {
                            self.toggle_usage_panel(true);
                        }

                        let sep_resp = ui.label(RichText::new("|").size(11.0).color(p.muted));
                        let mut selector_resp = term_resp.union(sep_resp);

                        let sys_resp = ui
                            .add(
                                egui::Label::new(RichText::new("System").size(11.0).color(p.text))
                                    .sense(Sense::click()),
                            )
                            .on_hover_cursor(CursorIcon::PointingHand);
                        if sys_resp.clicked() {
                            self.toggle_usage_panel(false);
                        }
                        selector_resp = selector_resp.union(sys_resp);

                        ui.add_space(6.0);
                        let usage_resp =
                            ui.label(RichText::new("Usage:").size(11.0).strong().color(p.muted));
                        selector_resp = selector_resp.union(usage_resp);
                        let _ = selector_resp;
                    });
                });
            });

        // Sidebar workspace switcher (Sidebar mode only). Visibility is
        // animated inside `render_workspace_sidebar` via `show_animated`, so
        // we always call it while in Sidebar mode — even mid-hide, so the
        // collapse animation runs to completion.
        if self.workspace_placement == WorkspacePlacement::Sidebar {
            render_workspace_sidebar(ctx, self, p);
        }

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
                    paint_workspace_terminal_spawn_notice_bar(ui, self);
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
                        .id_salt(("multerm_ws_scroll", self.selected_workspace))
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

                            let fullscreen_ids_snapshot = self.fullscreen_terminal_ids.clone();
                            const FULLSCREEN_HEADER_HOVER: &str = "Double-click to fullscreen mode";
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
                            let git_changes_action = std::cell::Cell::new(None::<(u64, bool)>);
                            let fullscreen_title_open = std::cell::Cell::new(None::<u64>);
                            let fullscreen_title_close = std::cell::Cell::new(None::<u64>);
                            {
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
                                    let (left_group, right_group) =
                                        runtime.terminals.split_at_mut(idx);
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
                                        if let Some(rh) =
                                            layout.default_pane_height_hint(viewport.y)
                                        {
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
                                    let max_y =
                                        (content_height - pane.desired_size.y).max(0.0).max(pos.y);
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
                                        .on_hover_cursor(CursorIcon::Grab)
                                        .on_hover_text(FULLSCREEN_HEADER_HOVER);
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
                                                        if best_x_snap.is_none_or(|(best, _, _)| {
                                                            d_left < best
                                                        }) {
                                                            best_x_snap =
                                                                Some((d_left, nx, snap_x));
                                                        }
                                                    }
                                                    let d_right = ((pos.x + w) - snap_x).abs();
                                                    if d_right <= RESIZE_SNAP_DISTANCE {
                                                        let nx = (snap_x - w).max(0.0);
                                                        if best_x_snap.is_none_or(|(best, _, _)| {
                                                            d_right < best
                                                        }) {
                                                            best_x_snap =
                                                                Some((d_right, nx, snap_x));
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
                                                        if best_y_snap.is_none_or(|(best, _, _)| {
                                                            d_bottom < best
                                                        }) {
                                                            best_y_snap =
                                                                Some((d_bottom, ny, snap_y));
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

                                        let gap_snap = gap_rect_snapshot_three_way(
                                            left_group,
                                            pane,
                                            right_group,
                                        );
                                        let (used_h, used_v) =
                                            collect_pair_gaps_from_rect_snapshot(&gap_snap, idx);
                                        let dims_drag = pane_neighbor_dimensions(
                                            pos,
                                            Vec2::new(w, h),
                                            left_group.iter().chain(right_group.iter()),
                                            canvas_width,
                                            content_height,
                                        );
                                        snap_drag_pos_to_used_neighbor_gaps(
                                            &mut pos, w, h, max_y, &dims_drag, &used_h, &used_v,
                                        );

                                        pos.x = pos.x.round().max(0.0);
                                        pos.y = pos.y.round().clamp(0.0, max_y);

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

                                    let left_active =
                                        resize_left.hovered() || resize_left.dragged();
                                    let right_active =
                                        resize_right.hovered() || resize_right.dragged();
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
                                    let show_layout_metrics =
                                        drag_response.dragged() || any_resize_dragged;
                                    if any_resize_dragged {
                                        let init_right = pos.x + pane.desired_size.x;
                                        let init_bottom = pos.y + pane.desired_size.y;
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
                                            let max_left =
                                                (init_right - TERMINAL_MIN_WIDTH).max(0.0);
                                            let proposed_left =
                                                (pos.x + delta.x).clamp(0.0, max_left);
                                            new_x = proposed_left;
                                            new_w = init_right - proposed_left;
                                        }
                                        if right_dragged {
                                            new_w = (new_w + delta.x).max(TERMINAL_MIN_WIDTH);
                                        }
                                        if top_dragged {
                                            let max_top =
                                                (init_bottom - TERMINAL_MIN_HEIGHT).max(0.0);
                                            let proposed_top =
                                                (pos.y + delta.y).clamp(0.0, max_top);
                                            new_y = proposed_top;
                                            new_h = init_bottom - proposed_top;
                                        }
                                        if bottom_dragged {
                                            let max_h =
                                                (content_height - new_y).max(TERMINAL_MIN_HEIGHT);
                                            new_h =
                                                (new_h + delta.y).clamp(TERMINAL_MIN_HEIGHT, max_h);
                                        }

                                        let mut best_x_snap: Option<(f32, f32, bool, usize)> = None;
                                        let mut best_y_snap: Option<(f32, f32, bool, usize)> = None;
                                        let pane_y0 = new_y;
                                        let pane_y1 = new_y + new_h;
                                        let pane_x0 = new_x;
                                        let pane_x1 = new_x + new_w;

                                        let mut inspect_other =
                                            |other_idx: usize, other: &TerminalPane| {
                                                let other_pos =
                                                    other.position.unwrap_or(Pos2::ZERO);
                                                let other_left = other_pos.x;
                                                let other_right =
                                                    other_pos.x + other.desired_size.x;
                                                let other_top = other_pos.y;
                                                let other_bottom =
                                                    other_pos.y + other.desired_size.y;

                                                let y_overlap = (pane_y1.min(other_bottom)
                                                    - pane_y0.max(other_top))
                                                .max(0.0);
                                                let x_overlap = (pane_x1.min(other_right)
                                                    - pane_x0.max(other_left))
                                                .max(0.0);

                                                if right_dragged
                                                    && y_overlap >= RESIZE_SNAP_OVERLAP_MIN
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
                                                if left_dragged
                                                    && y_overlap >= RESIZE_SNAP_OVERLAP_MIN
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
                                                if top_dragged
                                                    && x_overlap >= RESIZE_SNAP_OVERLAP_MIN
                                                {
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
                                                new_x = (snap_x).clamp(
                                                    0.0,
                                                    (init_right - TERMINAL_MIN_WIDTH).max(0.0),
                                                );
                                                new_w = init_right - new_x;
                                                snap_guide_x = Some(snap_x);
                                            }
                                        }

                                        if let Some((_, snap_y, snapped_bottom_edge, _other_idx)) =
                                            best_y_snap
                                        {
                                            if snapped_bottom_edge && bottom_dragged {
                                                new_h = (snap_y - new_y).clamp(
                                                    TERMINAL_MIN_HEIGHT,
                                                    (content_height - new_y)
                                                        .max(TERMINAL_MIN_HEIGHT),
                                                );
                                                snap_guide_y = Some(snap_y);
                                            } else if !snapped_bottom_edge && top_dragged {
                                                new_y = (snap_y).clamp(
                                                    0.0,
                                                    (init_bottom - TERMINAL_MIN_HEIGHT).max(0.0),
                                                );
                                                new_h = init_bottom - new_y;
                                                snap_guide_y = Some(snap_y);
                                            }
                                        }

                                        let gap_snap = gap_rect_snapshot_three_way(
                                            left_group,
                                            pane,
                                            right_group,
                                        );
                                        let (used_h, used_v) =
                                            collect_pair_gaps_from_rect_snapshot(&gap_snap, idx);
                                        snap_resize_rect_to_used_neighbor_gaps(
                                            &mut new_x,
                                            &mut new_y,
                                            &mut new_w,
                                            &mut new_h,
                                            left_group,
                                            right_group,
                                            left_dragged,
                                            right_dragged,
                                            top_dragged,
                                            bottom_dragged,
                                            &used_h,
                                            &used_v,
                                            init_right,
                                            init_bottom,
                                            canvas_width,
                                            content_height,
                                            content_height,
                                        );

                                        new_x = new_x.round().max(0.0);
                                        new_y = new_y.round().max(0.0);
                                        if left_dragged && !right_dragged {
                                            new_w = (init_right - new_x).max(TERMINAL_MIN_WIDTH);
                                        } else {
                                            new_w = new_w.round().max(TERMINAL_MIN_WIDTH);
                                        }
                                        if top_dragged && !bottom_dragged {
                                            new_h = (init_bottom - new_y).max(TERMINAL_MIN_HEIGHT);
                                        } else {
                                            new_h = new_h.round().max(TERMINAL_MIN_HEIGHT);
                                        }
                                        if bottom_dragged && !top_dragged {
                                            let max_h =
                                                (content_height - new_y).max(TERMINAL_MIN_HEIGHT);
                                            new_h = new_h.min(max_h);
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

                                    let is_active = runtime.active_terminal == Some(idx);
                                    if is_active {
                                        runtime.active_terminal_rect = Some(pane_rect);
                                    }
                                    let mut border = if is_active {
                                        p.terminal_border_active
                                    } else {
                                        p.border
                                    };
                                    let mut stroke_w: f32 = 2.0;
                                    if let (Some(blink_id), Some(started_at)) = (
                                        equal_size_template_blink_terminal_id,
                                        equal_size_template_blink_started_at,
                                    ) {
                                        if pane.id == blink_id && equal_size_picker_open {
                                            let elapsed = equal_size_template_blink_now
                                                .duration_since(started_at);
                                            // Keep blinking continuously while picker is open.
                                            stroke_w = 5.0;
                                            const BLINK_PERIOD_MS: u128 = 550;
                                            let phase_ms =
                                                (elapsed.as_millis() % BLINK_PERIOD_MS) as f32;
                                            let phase = phase_ms / (BLINK_PERIOD_MS as f32);
                                            // Smooth sine-based fade for a less "jittery" look.
                                            let intensity =
                                                0.5 + 0.5 * (std::f32::consts::TAU * phase).sin(); // 0..1

                                            let base = p.border;
                                            let peak = p.terminal_border_active;
                                            let lerp_u8 = |a: u8, b: u8, t: f32| -> u8 {
                                                (a as f32 + (b as f32 - a as f32) * t)
                                                    .round()
                                                    .clamp(0.0, 255.0)
                                                    as u8
                                            };
                                            border = Color32::from_rgba_unmultiplied(
                                                lerp_u8(base.r(), peak.r(), intensity),
                                                lerp_u8(base.g(), peak.g(), intensity),
                                                lerp_u8(base.b(), peak.b(), intensity),
                                                lerp_u8(base.a(), peak.a(), intensity),
                                            );
                                        }
                                    }

                                    let drag_header_double_fullscreen = drag_response.double_clicked();

                                    // Keep the pane-wide click target *below* the header chrome. Otherwise it
                                    // sits above the header widgets in the interaction stack and swallows
                                    // double-clicks meant for fullscreen.
                                    const FRAME_INNER_TOP: f32 = 6.0;
                                    const HEADER_ROW_MIN: f32 = 24.0;
                                    const FIND_ROW_EST: f32 = 34.0;
                                    const SEP_BELOW_HEADER_EST: f32 = 10.0;
                                    let find_row_extra = if is_active
                                        && runtime
                                            .scrollback_searches
                                            .get(idx)
                                            .is_some_and(|s| s.open)
                                    {
                                        FIND_ROW_EST
                                    } else {
                                        0.0
                                    };
                                    let pane_header_chrome_y = FRAME_INNER_TOP
                                        + HEADER_ROW_MIN
                                        + find_row_extra
                                        + SEP_BELOW_HEADER_EST;
                                    let pane_body_rect = egui::Rect::from_min_max(
                                        pane_rect.min + Vec2::new(0.0, pane_header_chrome_y),
                                        pane_rect.max,
                                    );
                                    let pane_response =
                                        ui.allocate_rect(pane_body_rect, Sense::click());
                                    let mut clicked_cell_from_grid: Option<(usize, usize)> = None;

                                    // Update this pane's light position when settings permit.
                                    if cyber.show_light && cyber.follows_mouse && (is_active || cyber.all_panes) {
                                        pane.border_light_pos = match (pane.border_light_pos, light_target) {
                                            (Some(cur), Some(tgt)) => {
                                                let alpha = if perf_mode { 1.0 } else {
                                                    1.0 - (-cyber.speed * light_dt).exp()
                                                };
                                                Some(Pos2::new(cur.x + (tgt.x - cur.x) * alpha, cur.y + (tgt.y - cur.y) * alpha))
                                            }
                                            (None, Some(tgt)) => Some(tgt),
                                            (cur, None) => cur,
                                        };
                                    }
                                    // Apply atmospheric shimmer on top of this pane's stored position.
                                    let border_light_pos = if cyber.show_light {
                                        pane.border_light_pos
                                            .map(|p| Pos2::new(p.x + light_atmos_dx, p.y + light_atmos_dy))
                                    } else {
                                        None
                                    };

                                    // Shared light-source helpers used by both the outer halo rings
                                    // and the gradient border mesh below.
                                    const PANE_CORNER_R: f32 = 6.0;
                                    let light_sigma = pane_rect.size().length() * cyber.sigma;
                                    let light_brightness = |pos: Pos2| -> f32 {
                                        if let Some(mp) = border_light_pos {
                                            let dx = pos.x - mp.x;
                                            let dy = pos.y - mp.y;
                                            (-(dx * dx + dy * dy) / (2.0 * light_sigma * light_sigma)).exp()
                                        } else {
                                            0.15
                                        }
                                    };
                                    let light_n_arc = 8usize;
                                    let light_path_pts = |rect: egui::Rect, r: f32| -> Vec<Pos2> {
                                        let mut pts = Vec::with_capacity(4 * (light_n_arc + 1));
                                        let corners = [
                                            (rect.min.x + r, rect.min.y + r, 180.0_f32, 270.0_f32),
                                            (rect.max.x - r, rect.min.y + r, 270.0_f32, 360.0_f32),
                                            (rect.max.x - r, rect.max.y - r, 0.0_f32,   90.0_f32),
                                            (rect.min.x + r, rect.max.y - r, 90.0_f32, 180.0_f32),
                                        ];
                                        let edge_ends = [
                                            Pos2::new(rect.max.x - r, rect.min.y),
                                            Pos2::new(rect.max.x, rect.max.y - r),
                                            Pos2::new(rect.min.x + r, rect.max.y),
                                            Pos2::new(rect.min.x, rect.min.y + r),
                                        ];
                                        for (ci, &(cx, cy, a0, a1)) in corners.iter().enumerate() {
                                            for i in 0..light_n_arc {
                                                let t = i as f32 / light_n_arc as f32;
                                                let ang = (a0 + (a1 - a0) * t).to_radians();
                                                if r > 0.0 {
                                                    pts.push(Pos2::new(cx + r * ang.cos(), cy + r * ang.sin()));
                                                } else {
                                                    pts.push(Pos2::new(cx, cy));
                                                }
                                            }
                                            pts.push(edge_ends[ci]);
                                        }
                                        pts
                                    };

                                    // Cyberpunk active: gradient border + outer glow
                                    let use_gradient_border = is_active && p.terminal_glow.is_some() && stroke_w < 3.0 && cyber.show_light;
                                    if is_active && cyber.show_light && cyber.show_halos {
                                        if let Some(glow) = p.terminal_glow {
                                            // Outer soft glow halos — gradient-lit by the animated light source.
                                            let brt = cyber.brightness;
                                            let paint_halo = |outer_rect: egui::Rect, outer_cr: f32,
                                                               inner_rect: egui::Rect, inner_cr: f32,
                                                               peak_alpha: f32| {
                                                let outer_pts = light_path_pts(outer_rect, outer_cr);
                                                let inner_pts = light_path_pts(inner_rect, inner_cr);
                                                let n = outer_pts.len().min(inner_pts.len());
                                                let mut m = egui::epaint::Mesh::default();
                                                for i in 0..n {
                                                    let t_o = light_brightness(outer_pts[i]);
                                                    let t_i = light_brightness(inner_pts[i]);
                                                    m.colored_vertex(outer_pts[i], Color32::from_rgba_unmultiplied(
                                                        glow.r(), glow.g(), glow.b(), (peak_alpha * brt * t_o).min(255.0) as u8,
                                                    ));
                                                    m.colored_vertex(inner_pts[i], Color32::from_rgba_unmultiplied(
                                                        glow.r(), glow.g(), glow.b(), (peak_alpha * brt * t_i).min(255.0) as u8,
                                                    ));
                                                }
                                                for i in 0..n {
                                                    let j = (i + 1) % n;
                                                    let oi = (i * 2) as u32;
                                                    let ii = (i * 2 + 1) as u32;
                                                    let oj = (j * 2) as u32;
                                                    let ij = (j * 2 + 1) as u32;
                                                    m.add_triangle(oi, oj, ij);
                                                    m.add_triangle(oi, ij, ii);
                                                }
                                                Shape::mesh(m)
                                            };
                                            ui.painter().add(paint_halo(
                                                pane_rect.expand(10.0), PANE_CORNER_R + 10.0,
                                                pane_rect.expand(5.0),  PANE_CORNER_R + 5.0,
                                                90.0,
                                            ));
                                            ui.painter().add(paint_halo(
                                                pane_rect.expand(5.5), PANE_CORNER_R + 5.5,
                                                pane_rect.expand(2.5), PANE_CORNER_R + 2.5,
                                                180.0,
                                            ));
                                        }
                                    }
                                    let frame_stroke = if use_gradient_border {
                                        Stroke::NONE
                                    } else {
                                        Stroke::new(stroke_w, border)
                                    };
                                    ui.scope_builder(
                                        egui::UiBuilder::new().max_rect(pane_rect),
                                        |ui| {
                                            egui::Frame::default()
                                                .fill(p.term_bg)
                                                .stroke(frame_stroke)
                                                .corner_radius(if p.terminal_glow.is_some() { PANE_CORNER_R } else { 4.0 })
                                                .inner_margin(Margin::same(6))
                                                .show(ui, |ui| {
                                                    ui.horizontal(|ui| {
                                                        const HEADER_ROW_H: f32 = 32.0;
                                                        let row_h = HEADER_ROW_H
                                                            .max(ui.spacing().interact_size.y);
                                                        let row_w = ui.available_width();
                                                        let row_rect = egui::Rect::from_min_size(
                                                            ui.cursor().min,
                                                            Vec2::new(row_w, row_h),
                                                        );
                                                        // Cyberpunk: subtle tinted header background
                                                        if let Some(glow) = p.terminal_glow {
                                                            ui.painter().rect_filled(
                                                                row_rect,
                                                                0.0,
                                                                Color32::from_rgba_unmultiplied(
                                                                    glow.r(), glow.g(), glow.b(), 14,
                                                                ),
                                                            );
                                                        }
                                                        // Background interact first (below); title + close
                                                        // painted after (above). Hover-only label lets
                                                        // double-clicks fall through to this layer.
                                                        let header_fs = ui
                                                            .interact(
                                                                row_rect,
                                                                ui.id().with(("pane_header_fs", pane.id)),
                                                                Sense::click(),
                                                            )
                                                            .on_hover_text(FULLSCREEN_HEADER_HOVER);
                                                        ui.scope_builder(
                                                            egui::UiBuilder::new().max_rect(row_rect),
                                                            |ui| {
                                                                ui.horizontal(|ui| {
                                                                    let draw_agent_icon = |ui: &mut egui::Ui, icon_size: Vec2, icon_pad: f32, frame_alpha: u8| {
                                                                        let (icon_alloc, _) = ui.allocate_exact_size(
                                                                            icon_size,
                                                                            Sense::hover(),
                                                                        );
                                                                        let icon_rect = icon_alloc.shrink2(Vec2::splat(1.0));
                                                                        let icon_bg = pane.agent_kind.badge_bg();
                                                                        ui.painter().rect_filled(
                                                                            icon_rect,
                                                                            6.0,
                                                                            Color32::from_rgba_unmultiplied(
                                                                                p.terminal_border_active.r(),
                                                                                p.terminal_border_active.g(),
                                                                                p.terminal_border_active.b(),
                                                                                frame_alpha,
                                                                            ),
                                                                        );
                                                                        ui.painter().circle_filled(
                                                                            icon_rect.center(),
                                                                            icon_rect.height() * 0.42,
                                                                            icon_bg,
                                                                        );
                                                                        ui.painter().circle_stroke(
                                                                            icon_rect.center(),
                                                                            icon_rect.height() * 0.42,
                                                                            Stroke::new(1.0, Color32::from_rgba_unmultiplied(255, 255, 255, 40)),
                                                                        );
                                                                        let icon_tex = match pane.agent_kind {
                                                                            TerminalAgentKind::Terminal => None,
                                                                            TerminalAgentKind::Claude => agent_icon_claude.as_ref(),
                                                                            TerminalAgentKind::Codex => agent_icon_codex.as_ref(),
                                                                            TerminalAgentKind::Cursor => agent_icon_cursor.as_ref(),
                                                                        };
                                                                        if let Some(tex) = icon_tex {
                                                                            let uv = egui::Rect::from_min_max(
                                                                                Pos2::new(0.0, 0.0),
                                                                                Pos2::new(1.0, 1.0),
                                                                            );
                                                                            ui.painter().image(
                                                                                tex.id(),
                                                                                icon_rect.shrink2(Vec2::splat(icon_pad)),
                                                                                uv,
                                                                                Color32::WHITE,
                                                                            );
                                                                        } else {
                                                                            paint_terminal_agent_icon(
                                                                                ui.painter(),
                                                                                icon_rect.shrink2(Vec2::splat(icon_pad)),
                                                                                pane.agent_kind,
                                                                                Color32::WHITE,
                                                                            );
                                                                        }
                                                                    };
                                                                    if p.terminal_glow.is_some() {
                                                                        // Cyberpunk header: large agent icon at left + title
                                                                        draw_agent_icon(ui, Vec2::new(28.0, 28.0), 4.5, 32);
                                                                        ui.add_space(10.0);
                                                                        ui.add(
                                                                            egui::Label::new(
                                                                                RichText::new(&pane.title)
                                                                                    .family(FontFamily::Monospace)
                                                                                    .size(11.0)
                                                                                    .color(p.text),
                                                                            )
                                                                            .selectable(false)
                                                                            .sense(Sense::hover()),
                                                                        );
                                                                        ui.with_layout(
                                                                            egui::Layout::right_to_left(egui::Align::Center),
                                                                            |ui| {
                                                                                if ui.add(egui::Label::new(
                                                                                    RichText::new("×")
                                                                                        .size(13.0)
                                                                                        .color(p.muted),
                                                                                ).sense(Sense::click())).clicked() {
                                                                                    close_idx = Some(idx);
                                                                                }
                                                                            },
                                                                        );
                                                                    } else {
                                                                        // Standard header: agent icon + title + close
                                                                        draw_agent_icon(ui, Vec2::new(28.0, 28.0), 4.5, 56);
                                                                        ui.add_space(10.0);
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
                                                                            egui::Layout::right_to_left(egui::Align::Center),
                                                                            |ui| {
                                                                                if ui.small_button("x").clicked() {
                                                                                    close_idx = Some(idx);
                                                                                }
                                                                            },
                                                                        );
                                                                    }
                                                                });
                                                            },
                                                        );
                                                        ui.advance_cursor_after_rect(row_rect);
                                                        // Attach the right-click menu to the existing `header_fs`
                                                        // response (registered *before* the close button is painted),
                                                        // so the `×` button still receives primary clicks. A second
                                                        // `ui.interact()` over `row_rect` here would be the topmost
                                                        // widget in egui's hit-test order and swallow them.
                                                        let pane_id = pane.id;
                                                        egui::Popup::context_menu(&header_fs)
                                                            .close_behavior(
                                                                egui::PopupCloseBehavior::CloseOnClickOutside,
                                                            )
                                                            .show(|ui| {
                                                                if !git_changes::git_is_available() {
                                                                    ui.label(
                                                                        RichText::new("git not found on PATH")
                                                                            .small()
                                                                            .color(ui.visuals().warn_fg_color),
                                                                    );
                                                                    return;
                                                                }
                                                                if ui.button("View changes (this terminal)").clicked() {
                                                                    git_changes_action.set(Some((pane_id, false)));
                                                                    ui.close();
                                                                }
                                                                if ui.button("Commit changes (this terminal)…").clicked() {
                                                                    git_changes_action.set(Some((pane_id, true)));
                                                                    ui.close();
                                                                }
                                                            });
                                                        if header_fs.double_clicked()
                                                            || drag_header_double_fullscreen
                                                        {
                                                            if fullscreen_ids_snapshot
                                                                .contains(&pane.id)
                                                            {
                                                                fullscreen_title_close
                                                                    .set(Some(pane.id));
                                                            } else {
                                                                fullscreen_title_open
                                                                    .set(Some(pane.id));
                                                            }
                                                        }
                                                    });
                                                    if is_active
                                                        && runtime
                                                            .scrollback_searches
                                                            .get(idx)
                                                            .is_some_and(|s| s.open)
                                                    {
                                                        ui.horizontal(|ui| {
                                                            ui.label(
                                                                RichText::new("Find:")
                                                                    .size(11.0)
                                                                    .color(p.muted),
                                                            );
                                                            let sid = scrollback_search_text_id(pane.id);
                                                            let search_changed = {
                                                                let st =
                                                                    &mut runtime.scrollback_searches[idx];
                                                                let resp = ui.add(
                                                                    TextEdit::singleline(&mut st.query)
                                                                        .id(sid)
                                                                        .hint_text("Scrollback…")
                                                                        .desired_width(200.0),
                                                                );
                                                                if runtime
                                                                    .scrollback_search_focus_pane
                                                                    .get()
                                                                    == Some(pane.id)
                                                                {
                                                                    resp.request_focus();
                                                                    runtime
                                                                        .scrollback_search_focus_pane
                                                                        .set(None);
                                                                }
                                                                resp.changed()
                                                            };
                                                            if search_changed {
                                                                runtime.scrollback_searches[idx]
                                                                    .current_match = 0;
                                                            }
                                                            let grid_ct =
                                                                pane.session.parser.grid();
                                                            let n = scrollback_compute_match_ranges(
                                                                grid_ct,
                                                                &runtime.scrollback_searches[idx]
                                                                    .query,
                                                            )
                                                            .len();
                                                            let st = &runtime.scrollback_searches[idx];
                                                            let label = if n > 0 {
                                                                format!(
                                                                    "{} / {}",
                                                                    st.current_match % n + 1,
                                                                    n
                                                                )
                                                            } else {
                                                                "0 / 0".to_string()
                                                            };
                                                            ui.label(
                                                                RichText::new(label)
                                                                    .size(11.0)
                                                                    .color(p.muted),
                                                            );
                                                            if ui.small_button("Prev").clicked() {
                                                                scrollback_search_advance_pane(
                                                                    pane,
                                                                    &mut runtime.scrollback_searches
                                                                        [idx],
                                                                    -1,
                                                                );
                                                            }
                                                            if ui.small_button("Next").clicked() {
                                                                scrollback_search_advance_pane(
                                                                    pane,
                                                                    &mut runtime.scrollback_searches
                                                                        [idx],
                                                                    1,
                                                                );
                                                            }
                                                            if ui.small_button("Close").clicked() {
                                                                runtime.scrollback_searches[idx]
                                                                    .open = false;
                                                            }
                                                        });
                                                    }
                                                    // Cyberpunk: replace separator with a glowing cyan divider line
                                                    if let Some(glow) = p.terminal_glow {
                                                        let sep_h = 1.5_f32;
                                                        let (sep_rect, _) = ui.allocate_exact_size(
                                                            Vec2::new(ui.available_width(), sep_h + 4.0),
                                                            Sense::hover(),
                                                        );
                                                        let y = sep_rect.center().y;
                                                        ui.painter().line_segment(
                                                            [
                                                                Pos2::new(sep_rect.min.x, y),
                                                                Pos2::new(sep_rect.max.x, y),
                                                            ],
                                                            Stroke::new(sep_h, Color32::from_rgba_unmultiplied(
                                                                glow.r(), glow.g(), glow.b(), 80,
                                                            )),
                                                        );
                                                        ui.painter().line_segment(
                                                            [
                                                                Pos2::new(sep_rect.min.x + 2.0, y - 1.5),
                                                                Pos2::new(sep_rect.max.x - 2.0, y - 1.5),
                                                            ],
                                                            Stroke::new(1.0, Color32::from_rgba_unmultiplied(
                                                                glow.r(), glow.g(), glow.b(), 35,
                                                            )),
                                                        );
                                                    } else {
                                                        ui.separator();
                                                    }
                                                    // Cyberpunk radial glow + noise dots — orbit the pane edge with the light source
                                                    if let Some(glow) = p.terminal_glow {
                                                        let body = ui.available_rect_before_wrap();
                                                        let painter = ui.painter().with_clip_rect(pane_rect.expand(1.0));

                                                        let rc = pane_rect;
                                                        let center = rc.center();
                                                        // Radial glow origin: free cursor mode sits at the cursor;
                                                        // orbit mode projects the cursor onto the pane perimeter.
                                                        let origin = if let Some(mp) = border_light_pos {
                                                            if cyber.follow_cursor {
                                                                mp
                                                            } else {
                                                                let dx = mp.x - center.x;
                                                                let dy = mp.y - center.y;
                                                                if dx.abs() + dy.abs() > 0.5 {
                                                                    let sx = if dx != 0.0 { (rc.width() * 0.5) / dx.abs() } else { f32::INFINITY };
                                                                    let sy = if dy != 0.0 { (rc.height() * 0.5) / dy.abs() } else { f32::INFINITY };
                                                                    let s = sx.min(sy);
                                                                    Pos2::new(center.x + dx * s, center.y + dy * s)
                                                                } else {
                                                                    Pos2::new(rc.min.x, rc.max.y)
                                                                }
                                                            }
                                                        } else {
                                                            Pos2::new(rc.min.x, rc.max.y)
                                                        };

                                                        if cyber.show_radial {
                                                            const GLOW_RADIUS: f32 = 180.0;
                                                            const SEGMENTS: usize = 48;
                                                            let mut mesh = egui::epaint::Mesh::default();
                                                            let a = (65.0 * cyber.brightness).min(255.0) as u8;
                                                            let center_color = Color32::from_rgba_unmultiplied(glow.r(), glow.g(), glow.b(), a);
                                                            let edge_color = Color32::from_rgba_unmultiplied(glow.r(), glow.g(), glow.b(), 0);
                                                            mesh.colored_vertex(origin, center_color);
                                                            for i in 0..=SEGMENTS {
                                                                let angle = (i as f32 / SEGMENTS as f32) * std::f32::consts::TAU;
                                                                let pt = Pos2::new(
                                                                    origin.x + GLOW_RADIUS * angle.cos(),
                                                                    origin.y + GLOW_RADIUS * angle.sin(),
                                                                );
                                                                mesh.colored_vertex(pt, edge_color);
                                                            }
                                                            for i in 0..SEGMENTS {
                                                                mesh.add_triangle(0, i as u32 + 1, i as u32 + 2);
                                                            }
                                                            painter.add(Shape::mesh(mesh));
                                                        }

                                                        if cyber.show_dots {
                                                            // Perimeter orbit phase from mouse angle relative to pane center.
                                                            let orbit_t = if let Some(mp) = border_light_pos {
                                                                let angle = (mp.y - center.y).atan2(mp.x - center.x);
                                                                (angle / std::f32::consts::TAU + 0.5).rem_euclid(1.0)
                                                            } else { 0.0 };

                                                            let pw = rc.width();
                                                            let ph = rc.height();
                                                            let perim = 2.0 * (pw + ph);
                                                            let top_f = pw / perim;
                                                            let right_f = ph / perim;
                                                            let bot_f = pw / perim;
                                                            let left_f = 1.0 - top_f - right_f - bot_f;
                                                            let perim_pos = |t: f32| -> Pos2 {
                                                                let t = t.rem_euclid(1.0);
                                                                if t < top_f {
                                                                    Pos2::new(rc.min.x + (t / top_f) * pw, rc.min.y)
                                                                } else if t < top_f + right_f {
                                                                    Pos2::new(rc.max.x, rc.min.y + ((t - top_f) / right_f) * ph)
                                                                } else if t < top_f + right_f + bot_f {
                                                                    Pos2::new(rc.max.x - ((t - top_f - right_f) / bot_f) * pw, rc.max.y)
                                                                } else {
                                                                    Pos2::new(rc.min.x, rc.max.y - ((t - top_f - right_f - bot_f) / left_f) * ph)
                                                                }
                                                            };

                                                            let seed = pane.id;
                                                            let _ = body;
                                                            for i in 0u64..90 {
                                                                let h1 = seed.wrapping_mul(0x9e3779b97f4a7c15).wrapping_add(i.wrapping_mul(0x6c62272e07bb0142));
                                                                let dot_t = (h1 & 0xffff) as f32 / 65535.0;
                                                                let h2 = h1.wrapping_mul(0xbf58476d1ce4e5b9);
                                                                let alpha_base = ((h2 >> 32) & 0x1f) as u8;
                                                                if alpha_base > 8 {
                                                                    let pos = perim_pos(dot_t + orbit_t);
                                                                    let bt = light_brightness(pos);
                                                                    let alpha = (alpha_base as f32 * cyber.brightness * (0.25 + 0.75 * bt)) as u8;
                                                                    painter.circle_filled(
                                                                        pos, 0.9,
                                                                        Color32::from_rgba_unmultiplied(glow.r(), glow.g(), glow.b(), alpha),
                                                                    );
                                                                }
                                                            }
                                                        }
                                                    }
                                                    let terminal_height =
                                                        ui.available_height().max(120.0);
                                                    let terminal_size = Vec2::new(
                                                        pane.desired_size.x,
                                                        terminal_height,
                                                    );
                                                    let popped_out =
                                                        fullscreen_ids_snapshot.contains(&pane.id);
                                                    if popped_out {
                                                        ui.vertical_centered(|ui| {
                                                            ui.add_space(terminal_height * 0.3);
                                                            ui.label(
                                                                RichText::new(
                                                                    "This terminal is shown in a fullscreen window.\n\
                                                                     Double-click the title above to dock it here again.",
                                                                )
                                                                .size(13.0)
                                                                .color(p.muted),
                                                            );
                                                        });
                                                        clicked_cell_from_grid = None;
                                                    } else {
                                                        resize_terminal_for_size(
                                                            pane,
                                                            terminal_size,
                                                        );
                                                        let selection = runtime
                                                            .selections
                                                            .get_mut(idx)
                                                            .expect("selection slot should exist");
                                                        let grid = pane.session.parser.grid();
                                                        let search_highlight = if is_active
                                                            && runtime
                                                                .scrollback_searches
                                                                .get(idx)
                                                                .is_some_and(|s| s.open)
                                                        {
                                                            let ranges =
                                                                scrollback_compute_match_ranges(
                                                                    grid,
                                                                    &runtime.scrollback_searches
                                                                        [idx]
                                                                        .query,
                                                                );
                                                            if ranges.is_empty() {
                                                                None
                                                            } else {
                                                                let i = runtime.scrollback_searches
                                                                    [idx]
                                                                    .current_match
                                                                    % ranges.len();
                                                                Some(ranges[i])
                                                            }
                                                        } else {
                                                            None
                                                        };
                                                        let synthetic_cursor_overlay =
                                                            use_synthetic_cursor_overlay(
                                                                &pane.session.parser,
                                                                grid,
                                                            );
                                                        clicked_cell_from_grid =
                                                            render_terminal_grid(
                                                                ui,
                                                                pane.id,
                                                                grid,
                                                                p,
                                                                selection,
                                                                is_active,
                                                                synthetic_cursor_overlay,
                                                                search_highlight,
                                                                &mut pane.pending_initial_scroll_to_bottom,
                                                                &mut pane.last_autoscroll_caret_v,
                                                            );
                                                    }

                                                    if let Some((clicked_vrow, clicked_col)) =
                                                        clicked_cell_from_grid
                                                    {
                                                        runtime.active_terminal = Some(idx);
                                                        clicked_on_pane = true;
                                                        let grid = pane.session.parser.grid();
                                                        let sb = grid.scrollback_len();
                                                        if clicked_vrow >= sb && grid.cols > 0 {
                                                            let clicked_row = clicked_vrow - sb;
                                                            let target_row = clicked_row
                                                                .min(grid.rows.saturating_sub(1));
                                                            let row_end =
                                                                row_render_end(grid, target_row)
                                                                    .min(
                                                                        grid.cols.saturating_sub(1),
                                                                    );
                                                            let target_col = clicked_col
                                                                .min(row_end.saturating_add(1))
                                                                .min(grid.cols.saturating_sub(1));

                                                            let mut bytes = Vec::new();
                                                            // Horizontal targeting only (do not send Up/Down,
                                                            // which can trigger shell/TUI history navigation).
                                                            if clicked_col >= row_end {
                                                                // For readline-like prompts, this reliably lands at line end.
                                                                bytes.push(0x05); // Ctrl+E
                                                                if let Some(ed) = runtime
                                                                    .line_editors
                                                                    .get_mut(idx)
                                                                {
                                                                    ed.move_to_end();
                                                                }
                                                            } else if target_col > grid.cursor.col {
                                                                let steps =
                                                                    target_col - grid.cursor.col;
                                                                bytes.reserve(steps * 3);
                                                                for _ in 0..steps {
                                                                    bytes.extend_from_slice(
                                                                        b"\x1b[C",
                                                                    );
                                                                }
                                                                if let Some(ed) = runtime
                                                                    .line_editors
                                                                    .get_mut(idx)
                                                                {
                                                                    ed.move_cursor_delta(
                                                                        steps as isize,
                                                                    );
                                                                }
                                                            } else if target_col < grid.cursor.col {
                                                                let steps =
                                                                    grid.cursor.col - target_col;
                                                                bytes.reserve(steps * 3);
                                                                for _ in 0..steps {
                                                                    bytes.extend_from_slice(
                                                                        b"\x1b[D",
                                                                    );
                                                                }
                                                                if let Some(ed) = runtime
                                                                    .line_editors
                                                                    .get_mut(idx)
                                                                {
                                                                    ed.move_cursor_delta(
                                                                        -(steps as isize),
                                                                    );
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
                                    // Paint gradient border ON TOP of frame background (after scope_builder)
                                    if use_gradient_border {
                                        if let Some(glow) = p.terminal_glow {
                                            let sw = stroke_w;
                                            let cr = PANE_CORNER_R;
                                            let cr_i = (cr - sw).max(0.0);

                                            let color_at = |pos: Pos2| -> Color32 {
                                                let t = light_brightness(pos);
                                                let r_dim = glow.r() / 12;
                                                let g_dim = glow.g() / 12;
                                                let b_dim = glow.b() / 12;
                                                let brt = cyber.brightness;
                                                Color32::from_rgba_unmultiplied(
                                                    (r_dim as f32 + (glow.r() - r_dim) as f32 * t).round() as u8,
                                                    (g_dim as f32 + (glow.g() - g_dim) as f32 * t).round() as u8,
                                                    (b_dim as f32 + (glow.b() - b_dim) as f32 * t).round() as u8,
                                                    ((30.0 + 225.0 * t) * brt).min(255.0).round() as u8,
                                                )
                                            };

                                            let outer = light_path_pts(pane_rect, cr);
                                            let inner = light_path_pts(pane_rect.shrink(sw), cr_i);
                                            let n = outer.len().min(inner.len());

                                            let mut bm = egui::epaint::Mesh::default();
                                            for i in 0..n {
                                                bm.colored_vertex(outer[i], color_at(outer[i]));
                                                bm.colored_vertex(inner[i], color_at(inner[i]));
                                            }
                                            for i in 0..n {
                                                let j = (i + 1) % n;
                                                let oi = (i * 2) as u32;
                                                let ii = (i * 2 + 1) as u32;
                                                let oj = (j * 2) as u32;
                                                let ij = (j * 2 + 1) as u32;
                                                bm.add_triangle(oi, oj, ij);
                                                bm.add_triangle(oi, ij, ii);
                                            }
                                            ui.painter().add(Shape::mesh(bm));
                                        }
                                    }

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
                                    if near_br_corner || resize_br.hovered() || resize_br.dragged()
                                    {
                                        paint_br_resize_line(
                                            ui.painter(),
                                            br_grip_rect,
                                            resize_br.hovered() || resize_br.dragged(),
                                            p,
                                        );
                                    }

                                    if show_layout_metrics {
                                        let pos = pane.position.unwrap_or(Pos2::ZERO);
                                        let dims = pane_neighbor_dimensions(
                                            pos,
                                            pane.desired_size,
                                            left_group.iter().chain(right_group.iter()),
                                            canvas_width,
                                            content_height,
                                        );
                                        let gap_snap = gap_rect_snapshot_three_way(
                                            left_group,
                                            pane,
                                            right_group,
                                        );
                                        let (used_h, used_v) =
                                            collect_pair_gaps_from_rect_snapshot(&gap_snap, idx);
                                        paint_terminal_neighbor_gap_guides(
                                            ui.painter(),
                                            content_origin,
                                            &dims,
                                            &used_h,
                                            &used_v,
                                        );
                                        let grid = pane.session.parser.grid();
                                        let pty_w = pane.desired_size.x;
                                        let pty_h = (pane.desired_size.y - TERMINAL_GRID_CHROME_Y)
                                            .max(120.0);
                                        let (pad_x, pad_y) = terminal_cell_slack_px(
                                            pty_w, pty_h, grid.cols, grid.rows,
                                        );
                                        let fill = Color32::from_rgba_unmultiplied(0, 0, 0, 200);
                                        paint_pane_layout_metrics_overlay(
                                            ui.painter(),
                                            pane_rect,
                                            pad_x,
                                            pad_y,
                                            p.text,
                                            fill,
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
                                        let clicked_on_terminal =
                                            runtime.terminals.iter().any(|pane| {
                                                let pos = pane.position.unwrap_or(Pos2::ZERO);
                                                let rect = egui::Rect::from_min_size(
                                                    pos,
                                                    pane.desired_size,
                                                );
                                                rect.contains(local_pos)
                                            });
                                        if !clicked_on_terminal {
                                            runtime.active_terminal = None;
                                        }
                                    } else if !clicked_on_pane {
                                        runtime.active_terminal = None;
                                    }
                                }
                            }

                            if let Some(pid) = fullscreen_title_close.get() {
                                self.fullscreen_terminal_ids.remove(&pid);
                            }
                            if let Some(pid) = fullscreen_title_open.get() {
                                self.fullscreen_terminal_ids.insert(pid);
                            }
                            if let Some((term_id, show_commit)) = git_changes_action.get() {
                                let ws = self.selected_workspace;
                                self.open_git_changes_panel(
                                    ws,
                                    git_changes::GitChangesScope::Terminal(term_id),
                                    show_commit,
                                );
                            }

                            if let Some(idx) = close_idx {
                                if let Some(runtime) = self.active_workspace_runtime_mut() {
                                    let was_active = runtime.active_terminal == Some(idx);
                                    let removed_id = runtime.terminals.get(idx).map(|pane| pane.id);
                                    runtime.terminals.remove(idx);
                                    runtime.selections.remove(idx);
                                    runtime.scrollback_searches.remove(idx);
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
                                    if let Some(id) = removed_id {
                                        self.fullscreen_terminal_ids.remove(&id);
                                    }
                                }
                                save_workspace_state(self);
                            }
                        });
                });
        });

        let p_zoom = self.ui_theme.palette().with_style(self.ui_style);
        for &pane_id in self.fullscreen_terminal_ids.clone().iter() {
            let Some((ws, idx, title)) = self.terminal_location_by_pane_id(pane_id) else {
                self.fullscreen_terminal_ids.remove(&pane_id);
                continue;
            };
            let vp_id = ViewportId::from_hash_of(("multerm_terminal_zoom", pane_id));
            let window_title = format!("{title} — Multerm");
            let title_embed = title.clone();
            let builder = ViewportBuilder::default()
                .with_title(window_title)
                .with_fullscreen(true);
            ctx.show_viewport_immediate(vp_id, builder, |ctx, class| {
                ctx.request_repaint_after(Duration::from_millis(16));
                if ctx.input(|i| i.viewport().close_requested()) {
                    self.fullscreen_terminal_ids.remove(&pane_id);
                    return;
                }
                self.route_workspace_terminal_keyboard(ctx, ws, idx);
                let mut dock_fullscreen = false;
                match class {
                    ViewportClass::Embedded => {
                        egui::Window::new(format!("Multerm — {}", title_embed))
                            .id(egui::Id::new(("multerm_zoom_embed", pane_id)))
                            .default_rect(ctx.content_rect())
                            .collapsible(false)
                            .resizable(false)
                            .show(ctx, |ui| {
                                self.paint_fullscreen_zoom_terminal(
                                    ui,
                                    ws,
                                    idx,
                                    p_zoom,
                                    pane_id,
                                    &mut dock_fullscreen,
                                );
                            });
                    }
                    _ => {
                        egui::CentralPanel::default()
                            .frame(
                                egui::Frame::default()
                                    .fill(p_zoom.term_bg)
                                    .inner_margin(Margin::same(0)),
                            )
                            .show(ctx, |ui| {
                                self.paint_fullscreen_zoom_terminal(
                                    ui,
                                    ws,
                                    idx,
                                    p_zoom,
                                    pane_id,
                                    &mut dock_fullscreen,
                                );
                            });
                    }
                }
                if dock_fullscreen {
                    self.fullscreen_terminal_ids.remove(&pane_id);
                    ctx.send_viewport_cmd(egui::ViewportCommand::Close);
                }
            });
        }

        const WORKSPACE_AUTOSAVE_INTERVAL: Duration = Duration::from_secs(2);
        let now = Instant::now();
        if now >= self.workspace_autosave_deadline {
            save_workspace_state(self);
            self.workspace_autosave_deadline = now + WORKSPACE_AUTOSAVE_INTERVAL;
        }

        self.render_workspace_history_overlay(ctx, p);
        self.draw_equal_size_picker(ctx, p);
        self.cleanup_stale_color_picker();
    }

    fn on_exit(&mut self, _gl: Option<&eframe::glow::Context>) {
        save_workspace_state(self);
    }
}

impl MultermUi {
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

    fn toggle_usage_panel(&mut self, multerm_only: bool) {
        if let Some(existing_idx) = self
            .usage_panel_open_order
            .iter()
            .position(|scope| *scope == multerm_only)
        {
            self.usage_panel_open_order.remove(existing_idx);
            return;
        }
        self.show_multerm_only_status = multerm_only;
        self.usage_panel_open_order.push(multerm_only);
    }

    fn draw_usage_hover_panel(&mut self, ui: &mut egui::Ui, p: UiPalette, multerm_only: bool) {
        ui.set_min_width(300.0);
        ui.label(
            RichText::new(if multerm_only {
                "Multerm process usage"
            } else {
                "System usage"
            })
            .size(12.0)
            .strong()
            .color(p.text),
        );
        ui.add_space(4.0);

        if multerm_only {
            let Some(pid) = get_current_pid().ok() else {
                ui.label(
                    RichText::new("Multerm process unavailable")
                        .size(11.0)
                        .color(p.muted),
                );
                return;
            };
            let Some(proc_) = self.system.process(pid) else {
                ui.label(
                    RichText::new("Multerm process unavailable")
                        .size(11.0)
                        .color(p.muted),
                );
                return;
            };

            let total_mem = self.system.total_memory().max(1);
            // `Process::cpu_usage` is summed across cores (can exceed 100%); `global_cpu_usage` is
            // the average across CPUs (0–100%). Scale so Multerm matches the system panel.
            let n_cpus = self.system.cpus().len().max(1) as f32;
            let proc_cpu = (proc_.cpu_usage().max(0.0) / n_cpus).clamp(0.0, 100.0);
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
            ui.horizontal(|ui| {
                ui.label(
                    RichText::new("FPS")
                        .size(10.0)
                        .family(FontFamily::Monospace)
                        .color(p.muted),
                );
                // Match `usage_meter_row` progress bar width so the value lines up with CPU/RAM.
                ui.add_space(126.0);
                ui.label(
                    RichText::new(format!("{:.0}", self.ui_fps_smoothed.max(0.0)))
                        .size(10.0)
                        .family(FontFamily::Monospace)
                        .color(p.text),
                );
            });
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

    fn add_terminal(
        &mut self,
        ctx: &egui::Context,
        spawn_pos: Option<Pos2>,
        anchor_terminal: Option<usize>,
    ) -> bool {
        let working_dir = self
            .workspaces
            .get(self.selected_workspace)
            .map(|w| w.working_dir.as_str())
            .unwrap_or("");
        if let Some(block) = workspace_terminal_cwd_block(working_dir) {
            self.workspace_terminal_spawn_notice =
                Some(workspace_spawn_notice_from_block(working_dir, block));
            self.workspace_terminal_spawn_notice_until =
                Some(Instant::now() + Duration::from_secs(8));
            ctx.request_repaint_after(Duration::from_secs(8));
            return false;
        }

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

        let mut git_watch_root = None;
        {
            let Some(runtime) = self.active_workspace_runtime_mut() else {
                return false;
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
                let first_top =
                    min_y_topmost_in_column(&runtime.terminals, viewport_w, col, layout);
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
                    let col =
                        pick_column_at_x(pos.x + anchor.desired_size.x * 0.5, viewport_w, layout);
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
            git_watch_root = pane.git.repo_root.clone();
            runtime.terminals.push(pane);
            runtime.selections.push(None);
            runtime.line_editors.push(LineEditor::new());
            runtime
                .scrollback_searches
                .push(ScrollbackSearchPaneState::default());
            runtime.active_terminal = Some(runtime.terminals.len() - 1);
        }
        if let Some(root) = git_watch_root {
            self.git_repo_watchers.ensure_watching(&root);
        }
        self.next_terminal_id = next_terminal_id + 1;
        save_workspace_state(self);
        true
    }

    fn refresh_terminal_git_for_scope(&mut self, workspace_idx: usize, scope: git_changes::GitChangesScope) {
        let working_dir = self
            .workspaces
            .get(workspace_idx)
            .map(|w| w.working_dir.clone())
            .unwrap_or_else(default_working_dir);
        let Some(runtime) = self.workspace_runtime.get_mut(workspace_idx) else {
            return;
        };
        match scope {
            git_changes::GitChangesScope::Terminal(id) => {
                if let Some(t) = runtime.terminals.iter_mut().find(|t| t.id == id) {
                    t.git.refresh_repo(&working_dir);
                }
            }
            git_changes::GitChangesScope::AllTerminals => {
                for t in &mut runtime.terminals {
                    t.git.refresh_repo(&working_dir);
                }
            }
        }
    }

    fn open_git_changes_panel(
        &mut self,
        workspace_idx: usize,
        scope: git_changes::GitChangesScope,
        show_commit_section: bool,
    ) {
        self.refresh_terminal_git_for_scope(workspace_idx, scope);
        let selected = self
            .git_changes_panel
            .as_ref()
            .and_then(|p| p.selected_path.clone());
        self.git_changes_panel = Some(git_changes::GitChangesPanelState {
            workspace_idx,
            scope,
            selected_path: selected,
            commit_message: String::new(),
            show_commit_section,
            status_message: None,
        });
        self.git_changes_cache = None;
        self.git_changes_cache_dirty = true;
        let repo_root = self
            .git_repo_for_scope(workspace_idx, scope);
        if let Some(root) = repo_root {
            self.git_repo_watchers.ensure_watching(&root);
        }
    }

    fn git_repo_for_scope(
        &self,
        workspace_idx: usize,
        scope: git_changes::GitChangesScope,
    ) -> Option<PathBuf> {
        let ws_dir = self
            .workspaces
            .get(workspace_idx)
            .map(|w| w.working_dir.as_str())
            .unwrap_or("");
        let runtime = self.workspace_runtime.get(workspace_idx)?;
        match scope {
            git_changes::GitChangesScope::Terminal(id) => runtime
                .terminals
                .iter()
                .find(|t| t.id == id)
                .and_then(|t| t.git.repo_root.clone())
                .or_else(|| git_changes::git_repo_root(ws_dir)),
            git_changes::GitChangesScope::AllTerminals => runtime
                .terminals
                .iter()
                .find_map(|t| t.git.repo_root.clone())
                .or_else(|| git_changes::git_repo_root(ws_dir)),
        }
    }

    fn poll_git_file_events(&mut self) {
        let events = self.git_repo_watchers.drain_events();
        let panel_repo = self
            .git_changes_panel
            .as_ref()
            .and_then(|p| self.git_repo_for_scope(p.workspace_idx, p.scope));
        for (repo_root, paths) in events {
            if panel_repo.as_ref() == Some(&repo_root) {
                self.git_changes_cache_dirty = true;
            }
            for (ws_idx, ws) in self.workspaces.iter().enumerate() {
                let Some(ws_root) = git_changes::git_repo_root(&ws.working_dir) else {
                    continue;
                };
                if ws_root != repo_root {
                    continue;
                }
                let Some(runtime) = self.workspace_runtime.get_mut(ws_idx) else {
                    continue;
                };
                let term_idx = runtime
                    .active_terminal
                    .filter(|i| *i < runtime.terminals.len());
                let Some(term_idx) = term_idx else {
                    continue;
                };
                for path in &paths {
                    runtime.terminals[term_idx].git.note_path(path.clone());
                }
            }
        }
    }

    fn git_paths_for_scope(
        &self,
        workspace_idx: usize,
        scope: git_changes::GitChangesScope,
    ) -> (
        Option<PathBuf>,
        Option<String>,
        std::collections::HashSet<PathBuf>,
    ) {
        use std::collections::HashSet;
        let ws_dir = self
            .workspaces
            .get(workspace_idx)
            .map(|w| w.working_dir.as_str())
            .unwrap_or("");
        let Some(runtime) = self.workspace_runtime.get(workspace_idx) else {
            return (git_changes::git_repo_root(ws_dir), None, HashSet::new());
        };
        let mut paths = HashSet::new();
        let mut baseline = None;
        let mut repo_root = git_changes::git_repo_root(ws_dir);
        match scope {
            git_changes::GitChangesScope::Terminal(id) => {
                if let Some(t) = runtime.terminals.iter().find(|t| t.id == id) {
                    repo_root = t
                        .git
                        .repo_root
                        .clone()
                        .or_else(|| git_changes::git_repo_root(ws_dir));
                    baseline = t.git.baseline_head.clone();
                    paths = t.git.touched_paths.clone();
                }
            }
            git_changes::GitChangesScope::AllTerminals => {
                for t in &runtime.terminals {
                    paths.extend(t.git.touched_paths.iter().cloned());
                    if baseline.is_none() {
                        baseline = t.git.baseline_head.clone();
                    }
                    if repo_root.is_none() {
                        repo_root = t.git.repo_root.clone();
                    }
                }
                repo_root = repo_root.or_else(|| git_changes::git_repo_root(ws_dir));
                if paths.is_empty() {
                    if let Some(repo) = repo_root.as_ref() {
                        paths = git_changes::changed_paths_since_baseline(
                            repo,
                            baseline.as_deref(),
                        );
                    }
                }
            }
        }
        (repo_root, baseline, paths)
    }

    fn clear_git_paths_after_commit(
        &mut self,
        workspace_idx: usize,
        scope: git_changes::GitChangesScope,
        committed: &[PathBuf],
    ) {
        let Some(repo) = self.git_repo_for_scope(workspace_idx, scope) else {
            return;
        };
        let new_head = git_changes::git_head_at(&repo);
        let Some(runtime) = self.workspace_runtime.get_mut(workspace_idx) else {
            return;
        };
        match scope {
            git_changes::GitChangesScope::Terminal(id) => {
                for t in &mut runtime.terminals {
                    if t.id != id {
                        continue;
                    }
                    for p in committed {
                        t.git.touched_paths.remove(p);
                    }
                    t.git.baseline_head = new_head.clone();
                }
            }
            git_changes::GitChangesScope::AllTerminals => {
                for t in &mut runtime.terminals {
                    for p in committed {
                        t.git.touched_paths.remove(p);
                    }
                    t.git.baseline_head = new_head.clone();
                }
            }
        }
    }

    fn render_git_changes_panel(&mut self, ctx: &egui::Context, p: UiPalette) {
        let Some(panel) = self.git_changes_panel.clone() else {
            return;
        };

        let workspace_idx = panel.workspace_idx;
        let scope = panel.scope;

        // Invalidate cache if the panel switched workspace or scope.
        if let Some(cache) = self.git_changes_cache.as_ref() {
            if cache.workspace_idx != workspace_idx || cache.scope != Some(scope) {
                self.git_changes_cache_dirty = true;
            }
        } else {
            self.git_changes_cache_dirty = true;
        }

        if self.git_changes_cache_dirty {
            let (repo_root, baseline_head, paths) =
                self.git_paths_for_scope(workspace_idx, scope);
            let entries = repo_root
                .as_ref()
                .map(|repo| {
                    git_changes::collect_entries_for_paths(
                        repo,
                        baseline_head.as_deref(),
                        &paths,
                    )
                })
                .unwrap_or_default();
            self.git_changes_cache = Some(git_changes::GitChangesPanelCache {
                workspace_idx,
                scope: Some(scope),
                repo_root,
                baseline_head,
                paths,
                entries,
                diff_path: None,
                diff_lines: Vec::new(),
            });
            self.git_changes_cache_dirty = false;
        }

        let (repo_root, baseline_head, paths) = {
            let cache = self.git_changes_cache.as_ref().expect("cache built above");
            (
                cache.repo_root.clone(),
                cache.baseline_head.clone(),
                cache.paths.clone(),
            )
        };

        let viewport = ctx.viewport_rect();
        let margin = 36.0;
        let modal_size = (viewport.size() - egui::vec2(margin * 2.0, margin * 2.0))
            .max(egui::vec2(320.0, 240.0));
        let modal_size = egui::vec2(modal_size.x.min(980.0), modal_size.y.min(720.0));
        let modal_rect = egui::Rect::from_center_size(viewport.center(), modal_size);
        let backdrop_scrim = image_gallery_modal_scrim(self.ui_theme, p);
        let rim = p.terminal_border_active;
        let panel_bg = p.popover_fill;
        let mut close_panel = false;
        let mut do_commit = false;
        let mut refresh_selection: Option<Option<PathBuf>> = None;

        let backdrop_id = egui::Id::new("git_changes_backdrop");
        let backdrop_resp = egui::Area::new(backdrop_id)
            .order(egui::Order::Foreground)
            .fixed_pos(viewport.min)
            .show(ctx, |ui| {
                let (rect, resp) = ui.allocate_exact_size(viewport.size(), Sense::click());
                ui.painter().rect_filled(rect, 0.0, backdrop_scrim);
                resp
            });
        if backdrop_resp.inner.clicked() {
            close_panel = true;
        }

        let title = match scope {
            git_changes::GitChangesScope::Terminal(id) => {
                let name = self
                    .workspace_runtime
                    .get(workspace_idx)
                    .and_then(|r| r.terminals.iter().find(|t| t.id == id))
                    .map(|t| t.title.as_str())
                    .unwrap_or("Terminal");
                format!("Changes — {name}")
            }
            git_changes::GitChangesScope::AllTerminals => "Changes — all terminals".to_string(),
        };

        let Some(repo) = repo_root.clone() else {
            egui::Area::new(egui::Id::new("git_changes_panel_err"))
                .order(egui::Order::Tooltip)
                .fixed_pos(modal_rect.min)
                .show(ctx, |ui| {
                    egui::Frame::default()
                        .fill(panel_bg)
                        .stroke(Stroke::new(1.5, rim))
                        .inner_margin(Margin::same(12))
                        .show(ui, |ui| {
                            ui.label(RichText::new(&title).strong());
                            let ws_path = self
                                .workspaces
                                .get(workspace_idx)
                                .map(|w| w.working_dir.as_str())
                                .unwrap_or("(unknown)");
                            ui.label(format!(
                                "No git repository found for this terminal session.\n\nWorkspace folder: {ws_path}\n\nSet the workspace path (top bar) to your project root, or use a path inside a git repo."
                            ));
                            if ui.button("Close").clicked() {
                                close_panel = true;
                            }
                        });
                });
            if close_panel {
                self.git_changes_panel = None;
                self.git_changes_cache = None;
            }
            return;
        };

        let entries: Vec<git_changes::GitFileEntry> = self
            .git_changes_cache
            .as_ref()
            .map(|c| c.entries.clone())
            .unwrap_or_default();
        let selected_path = panel
            .selected_path
            .clone()
            .filter(|p| entries.iter().any(|e| &e.path == p))
            .or_else(|| entries.first().map(|e| e.path.clone()));

        // Only re-run `git diff` when the selected file actually changed.
        if let Some(cache) = self.git_changes_cache.as_mut() {
            if cache.diff_path != selected_path {
                cache.diff_lines = match selected_path.as_ref() {
                    Some(rel) => {
                        let text =
                            git_changes::git_diff_for_file(&repo, baseline_head.as_deref(), rel)
                                .unwrap_or_default();
                        git_changes::parse_unified_diff(&text)
                    }
                    None => Vec::new(),
                };
                cache.diff_path = selected_path.clone();
            }
        }
        let diff_lines: Vec<git_changes::DiffLine> = self
            .git_changes_cache
            .as_ref()
            .map(|c| c.diff_lines.clone())
            .unwrap_or_default();

        let panel_id = egui::Id::new("git_changes_panel");
        let mut commit_message = panel.commit_message.clone();
        let show_commit = panel.show_commit_section;
        let mut status_message = panel.status_message.clone();

        egui::Area::new(panel_id)
            .order(egui::Order::Tooltip)
            .fixed_pos(modal_rect.min)
            .show(ctx, |ui| {
                egui::Frame::default()
                    .fill(panel_bg)
                    .stroke(Stroke::new(1.5, rim))
                    .corner_radius(8.0)
                    .inner_margin(Margin::same(12))
                    .show(ui, |ui| {
                        let panel_inner = egui::vec2(modal_size.x - 24.0, modal_size.y - 24.0);
                        ui.set_min_size(panel_inner);
                        ui.set_max_size(panel_inner);

                        ui.horizontal(|ui| {
                            ui.label(RichText::new(&title).size(15.0).strong().color(p.text));
                            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                                if ui.button("✕").clicked() {
                                    close_panel = true;
                                }
                            });
                        });
                        ui.add_space(4.0);
                        if paths.is_empty() {
                            ui.label(
                                RichText::new(
                                    "No changes since this terminal session started, and no files were tracked yet. Make edits with this terminal focused, then open this view again.",
                                )
                                .color(p.muted)
                                .size(12.0),
                            );
                        } else if matches!(scope, git_changes::GitChangesScope::Terminal(_)) {
                            ui.label(
                                RichText::new(
                                    "Showing changes since this terminal session started (and files touched while it was focused).",
                                )
                                .color(p.muted)
                                .size(11.0),
                            );
                        }
                        if let Some(msg) = &status_message {
                            ui.label(RichText::new(msg).size(12.0).color(p.tab_active_indicator.unwrap_or(rim)));
                        }

                        let commit_block_h = if show_commit { 108.0 } else { 0.0 };
                        let body_h = (ui.available_height() - commit_block_h).max(160.0);
                        let list_w = 200.0;

                        ui.horizontal(|ui| {
                            ui.set_min_height(body_h);
                            ui.set_max_height(body_h);

                            ui.allocate_ui_with_layout(
                                egui::vec2(list_w, body_h),
                                egui::Layout::top_down(egui::Align::LEFT),
                                |ui| {
                                    ui.label(
                                        RichText::new("Changed files")
                                            .small()
                                            .strong()
                                            .color(p.muted),
                                    );
                                    egui::ScrollArea::vertical()
                                        .auto_shrink([false; 2])
                                        .show(ui, |ui| {
                                            ui.set_width(list_w - 8.0);
                                            if entries.is_empty() {
                                                ui.label(RichText::new("(none)").small().color(p.muted));
                                            }
                                            for entry in &entries {
                                                let selected = selected_path.as_deref()
                                                    == Some(entry.path.as_path());
                                                let label = format!(
                                                    "{}  {}",
                                                    entry.status.label(),
                                                    entry.path.display()
                                                );
                                                if ui
                                                    .selectable_label(
                                                        selected,
                                                        RichText::new(label)
                                                            .family(FontFamily::Monospace)
                                                            .size(11.0)
                                                            .color(entry.status.color()),
                                                    )
                                                    .clicked()
                                                {
                                                    refresh_selection =
                                                        Some(Some(entry.path.clone()));
                                                }
                                            }
                                        });
                                },
                            );

                            ui.separator();

                            let diff_w = (panel_inner.x - list_w - 24.0).max(200.0);
                            ui.allocate_ui_with_layout(
                                egui::vec2(diff_w, body_h),
                                egui::Layout::top_down(egui::Align::LEFT),
                                |ui| {
                                    if let Some(rel) = &selected_path {
                                        ui.label(
                                            RichText::new(rel.display().to_string())
                                                .family(FontFamily::Monospace)
                                                .strong()
                                                .color(p.text),
                                        );
                                    }
                                    let diff_scroll_h = ui.available_height().max(80.0);
                                    egui::ScrollArea::both()
                                        .id_salt("git_diff_body")
                                        .auto_shrink([false; 2])
                                        .max_height(diff_scroll_h)
                                        .show(ui, |ui| {
                                            ui.set_min_width(diff_w - 16.0);
                                            paint_unified_diff(ui, &diff_lines, p);
                                        });
                                },
                            );
                        });

                        if show_commit {
                            ui.add_space(6.0);
                            ui.separator();
                            ui.label(RichText::new("Commit message").small().strong());
                            ui.add(
                                TextEdit::multiline(&mut commit_message)
                                    .desired_width(f32::INFINITY)
                                    .desired_rows(2)
                                    .hint_text("Describe changes made in this terminal…"),
                            );
                            ui.horizontal(|ui| {
                                let can_commit = !entries.is_empty();
                                if ui
                                    .add_enabled(can_commit, egui::Button::new("Commit"))
                                    .clicked()
                                {
                                    do_commit = true;
                                }
                            });
                        }
                    });
            });

        if let Some(sel) = refresh_selection {
            if let Some(p) = self.git_changes_panel.as_mut() {
                p.selected_path = sel;
            }
        }
        if let Some(p) = self.git_changes_panel.as_mut() {
            p.commit_message = commit_message;
            p.status_message = status_message;
        }
        if do_commit {
            let paths: Vec<PathBuf> = entries.iter().map(|e| e.path.clone()).collect();
            let msg = self
                .git_changes_panel
                .as_ref()
                .map(|p| p.commit_message.clone())
                .unwrap_or_default();
            match git_changes::git_commit_paths(&repo, &paths, &msg) {
                Ok(()) => {
                    self.clear_git_paths_after_commit(workspace_idx, scope, &paths);
                    self.git_changes_cache_dirty = true;
                    status_message = Some(format!("Committed {} file(s).", paths.len()));
                    if let Some(p) = self.git_changes_panel.as_mut() {
                        p.status_message = status_message.clone();
                        p.commit_message.clear();
                    }
                }
                Err(e) => {
                    if let Some(p) = self.git_changes_panel.as_mut() {
                        p.status_message = Some(e);
                    }
                }
            }
        }
        if close_panel {
            self.git_changes_panel = None;
            self.git_changes_cache = None;
        }
    }

    fn tick_workspace_terminal_spawn_notice(&mut self) {
        if let Some(until) = self.workspace_terminal_spawn_notice_until {
            if Instant::now() >= until {
                self.workspace_terminal_spawn_notice = None;
                self.workspace_terminal_spawn_notice_until = None;
            }
        }
    }

    fn tick_uploaded_images_no_terminal_hint(&mut self) {
        if self
            .uploaded_images_no_terminal_hint_until
            .is_some_and(|until| Instant::now() >= until)
        {
            self.uploaded_images_no_terminal_hint_until = None;
        }
    }

    fn tick_workspace_history_overlay(&mut self) {
        let ttl = WORKSPACE_HISTORY_PANEL_HOLD + WORKSPACE_HISTORY_PANEL_FADE;
        self.workspace_history_overlay_entries
            .retain(|entry| entry.at.elapsed() <= ttl);
    }

    fn push_workspace_history_overlay_entry(
        &mut self,
        action: WorkspaceHistoryOverlayAction,
        detail: impl Into<String>,
    ) {
        let ws = self
            .selected_workspace
            .min(self.workspaces.len().saturating_sub(1));
        let workspace_name = self
            .workspaces
            .get(ws)
            .map(|w| w.title.as_str())
            .unwrap_or("Workspace");
        let prefix = match action {
            WorkspaceHistoryOverlayAction::Undo => "Undo",
            WorkspaceHistoryOverlayAction::Redo => "Redo",
        };
        let detail = detail.into();
        let (prev_label, next_label) = self.workspace_history_neighbor_labels(ws);
        self.workspace_history_overlay_entries
            .push(WorkspaceHistoryOverlayEntry {
                label: format!("{prefix}: {detail} ({workspace_name})"),
                prev_label,
                next_label,
                at: Instant::now(),
            });
        if self.workspace_history_overlay_entries.len() > WORKSPACE_HISTORY_PANEL_MAX_ITEMS {
            let overflow =
                self.workspace_history_overlay_entries.len() - WORKSPACE_HISTORY_PANEL_MAX_ITEMS;
            self.workspace_history_overlay_entries.drain(0..overflow);
        }
    }

    fn render_workspace_history_overlay(&self, ctx: &egui::Context, p: UiPalette) {
        let Some(last_at) = self.workspace_history_overlay_entries.last().map(|e| e.at) else {
            return;
        };
        let age = last_at.elapsed();
        let panel_alpha = if age <= WORKSPACE_HISTORY_PANEL_HOLD {
            1.0
        } else {
            let fade_t = ((age - WORKSPACE_HISTORY_PANEL_HOLD).as_secs_f32()
                / WORKSPACE_HISTORY_PANEL_FADE.as_secs_f32())
            .clamp(0.0, 1.0);
            1.0 - fade_t
        };
        if panel_alpha <= 0.0 {
            return;
        }

        let alpha_color = |c: Color32, factor: f32| {
            Color32::from_rgba_unmultiplied(
                c.r(),
                c.g(),
                c.b(),
                (255.0 * panel_alpha * factor).clamp(0.0, 255.0) as u8,
            )
        };

        egui::Area::new(egui::Id::new("workspace_history_overlay"))
            .order(egui::Order::Foreground)
            .anchor(Align2::RIGHT_TOP, egui::vec2(-12.0, 104.0))
            .show(ctx, |ui| {
                egui::Frame::default()
                    .fill(alpha_color(p.popover_fill, 0.96))
                    .stroke(Stroke::new(1.0, alpha_color(p.border, 0.95)))
                    .corner_radius(6.0)
                    .inner_margin(Margin::symmetric(10, 8))
                    .show(ui, |ui| {
                        ui.set_min_width(220.0);
                        ui.label(
                            RichText::new("History")
                                .size(11.0)
                                .family(FontFamily::Monospace)
                                .color(alpha_color(p.muted, 0.95)),
                        );
                        ui.add_space(2.0);
                        if let Some(entry) = self.workspace_history_overlay_entries.last() {
                            let item_age = entry.at.elapsed().as_secs_f32();
                            let item_fade = (1.0
                                - item_age
                                    / (WORKSPACE_HISTORY_PANEL_HOLD
                                        + WORKSPACE_HISTORY_PANEL_FADE)
                                        .as_secs_f32())
                            .clamp(0.35, 1.0);
                            let prev = entry.prev_label.as_deref().unwrap_or("—");
                            let next = entry.next_label.as_deref().unwrap_or("—");
                            ui.label(
                                RichText::new("Previous:")
                                    .size(11.0)
                                    .family(FontFamily::Monospace)
                                    .color(alpha_color(p.muted, item_fade * 0.95)),
                            );
                            ui.label(
                                RichText::new(prev)
                                    .size(12.0)
                                    .color(alpha_color(p.text, item_fade)),
                            );
                            ui.add_space(1.0);
                            ui.label(
                                RichText::new(&entry.label)
                                    .size(12.0)
                                    .strong()
                                    .color(alpha_color(p.text, item_fade)),
                            );
                            ui.add_space(1.0);
                            ui.label(
                                RichText::new("Next:")
                                    .size(11.0)
                                    .family(FontFamily::Monospace)
                                    .color(alpha_color(p.muted, item_fade * 0.95)),
                            );
                            ui.label(
                                RichText::new(next)
                                    .size(12.0)
                                    .color(alpha_color(p.text, item_fade)),
                            );
                        }
                    });
            });
    }

    /// Small popover above the footer “Uploaded Images” control (not a full-window modal).
    fn render_uploaded_images_no_terminal_hint(
        &self,
        ctx: &egui::Context,
        anchor: egui::Rect,
        p: UiPalette,
    ) {
        let Some(until) = self.uploaded_images_no_terminal_hint_until else {
            return;
        };
        if Instant::now() >= until {
            return;
        }

        let cyber_shell = p.terminal_glow.is_some();
        let fill = if cyber_shell {
            p.panel_bg
        } else {
            p.popover_fill
        };
        let border = if cyber_shell {
            p.terminal_border_active
        } else {
            p.border
        };
        let corner_r = if cyber_shell { 6.0 } else { 6.0 };
        let stroke_w = if cyber_shell { 1.25 } else { 1.0 };

        const GAP: f32 = 6.0;
        const MAX_W: f32 = 300.0;
        let viewport = ctx.viewport_rect();
        let hint_id = egui::Id::new("uploaded_images_no_terminal_hint_popover");
        // Anchor the bubble’s bottom edge just above the button (`pivot` + `fixed_pos`).
        let pivot_pos = egui::pos2(anchor.left(), anchor.top() - GAP);

        egui::Area::new(hint_id)
            .order(egui::Order::Tooltip)
            .pivot(egui::Align2::LEFT_BOTTOM)
            .fixed_pos(pivot_pos)
            .constrain_to(viewport)
            .show(ctx, |ui| {
                egui::Frame::default()
                    .fill(fill)
                    .stroke(Stroke::new(stroke_w, border))
                    .corner_radius(corner_r)
                    .inner_margin(Margin::symmetric(10, 8))
                    .show(ui, |ui| {
                        ui.set_max_width(MAX_W);
                        let hint_rt = RichText::new(
                            "Focus a terminal pane first, then open Uploaded Images to paste a path.",
                        )
                        .size(11.0)
                        .color(p.text);
                        let hint_rt = if cyber_shell {
                            hint_rt.family(FontFamily::Monospace)
                        } else {
                            hint_rt
                        };
                        ui.label(hint_rt);
                    });
            });
    }

    fn render_image_gallery(&mut self, ctx: &egui::Context, p: UiPalette) {
        let ws = self
            .selected_workspace
            .min(self.workspaces.len().saturating_sub(1));
        let Some(runtime) = self.workspace_runtime.get(ws) else {
            self.image_gallery_view = None;
            return;
        };
        if !runtime.show_image_gallery || runtime.uploaded_images.is_empty() {
            self.image_gallery_view = None;
            return;
        }

        let has_focused_terminal = runtime
            .active_terminal
            .is_some_and(|idx| idx < runtime.terminals.len());
        if !has_focused_terminal {
            self.image_gallery_view = None;
            if let Some(rt) = self.workspace_runtime.get_mut(ws) {
                rt.show_image_gallery = false;
            }
            return;
        }

        let images: Vec<String> = runtime.uploaded_images.iter().rev().cloned().collect();
        let active_idx = runtime.active_terminal;

        // Lazily load textures for images not yet in the cache.
        for path in &images {
            if !self.image_gallery_textures.contains_key(path) {
                if let Some(tex) = load_image_thumbnail(path, ctx, 256, "thumb") {
                    self.image_gallery_textures.insert(path.clone(), tex);
                }
            }
        }

        let cyber_shell = p.terminal_glow.is_some();
        let rim = p.terminal_border_active;
        let accent = p.tab_active_indicator.unwrap_or(rim);
        let panel_bg = if cyber_shell {
            p.panel_bg
        } else {
            p.popover_fill
        };
        let thumb_bg = p.term_bg;
        let thumb_border = p.border;
        let muted_color = p.muted;
        let backdrop_scrim = image_gallery_modal_scrim(self.ui_theme, p);
        let corner_r = if cyber_shell { 6.0 } else { 8.0 };
        let outer_stroke = Stroke::new(if cyber_shell { 1.25 } else { 1.5 }, rim);

        // Centered modal over the full viewport so the gallery stays usable regardless of
        // which terminal pane is active (e.g. Claude Code, Codex, or a shell).
        let viewport = ctx.viewport_rect();
        let margin = 40.0;
        let avail = (viewport.size() - egui::vec2(margin * 2.0, margin * 2.0))
            .max(egui::vec2(280.0, 220.0));
        let modal_size = egui::vec2(avail.x.min(720.0), avail.y.min(560.0));
        let modal_rect = egui::Rect::from_center_size(viewport.center(), modal_size);

        // Full-viewport dimmed backdrop — click outside the panel to close.
        let backdrop_id = egui::Id::new("gallery_backdrop");
        let backdrop_resp = egui::Area::new(backdrop_id)
            .order(egui::Order::Foreground)
            .fixed_pos(viewport.min)
            .show(ctx, |ui| {
                let (rect, resp) = ui.allocate_exact_size(viewport.size(), Sense::click());
                ui.painter().rect_filled(rect, 0.0, backdrop_scrim);
                resp
            });
        if backdrop_resp.inner.clicked() {
            self.image_gallery_view = None;
            self.image_gallery_rubber_band = None;
            self.image_gallery_selected.clear();
            if let Some(rt) = self.workspace_runtime.get_mut(ws) {
                rt.show_image_gallery = false;
            }
            return;
        }

        let gallery_id = egui::Id::new("image_gallery_panel");
        let mut close_gallery = false;
        let mut reuse_path: Option<String> = None;
        let mut copy_image_path: Option<String> = None;
        let mut delete_path: Option<String> = None;
        let mut open_view_path: Option<String> = None;
        let mut delete_selected_action = false;
        // Deferred selection mutations collected inside the closure.
        let mut click_select: Option<(String, bool, bool)> = None; // (path, ctrl, shift)
        let mut this_frame_thumb_rects: Vec<(String, egui::Rect)> = Vec::new();

        // Snapshots for immutable reads inside the closure.
        let selected_snapshot = self.image_gallery_selected.clone();
        let images_order = images.clone(); // for shift-range selection
        let thumb_rects_last = self.image_gallery_thumb_rects.clone();
        let rubber_band_snapshot = self.image_gallery_rubber_band;

        egui::Area::new(gallery_id)
            .order(egui::Order::Tooltip)
            .fixed_pos(modal_rect.min)
            .show(ctx, |ui| {
                egui::Frame::default()
                    .fill(panel_bg)
                    .stroke(outer_stroke)
                    .shadow(egui::epaint::Shadow {
                        offset: [0, 10],
                        blur: 22,
                        spread: 0,
                        color: if cyber_shell {
                            Color32::from_rgba_unmultiplied(rim.r(), rim.g(), rim.b(), 64)
                        } else {
                            Color32::from_rgba_unmultiplied(0, 0, 0, 88)
                        },
                    })
                    .corner_radius(corner_r)
                    .inner_margin(Margin {
                        left: 14,
                        right: 14,
                        top: 12,
                        bottom: 18,
                    })
                    .show(ui, |ui| {
                        ui.set_min_size(modal_size);
                        ui.set_max_size(modal_size);
                        let outer = ui.max_rect();

                        /// Space above the cyber double-line footer (scroll ends here).
                        const CYBER_BOTTOM_STRIP: f32 = 9.0;
                        /// Inset for header chrome only (does not change the outer modal `Frame` margin).
                        const CYBER_HEADER_PAD_X: f32 = 10.0;
                        const CYBER_HEADER_PAD_TOP: f32 = 8.0;
                        const CYBER_HEADER_PAD_BOTTOM: f32 = 6.0;
                        /// Row for title + close; must be ≥ close diameter so the pill is not clipped.
                        const CYBER_HEADER_ROW_H: f32 = 22.0;

                        if cyber_shell {
                            let glow = p.terminal_glow.unwrap();
                            let inner_w = outer.width();
                            let header_h =
                                CYBER_HEADER_ROW_H + CYBER_HEADER_PAD_TOP + CYBER_HEADER_PAD_BOTTOM;
                            let (_, hdr_resp) = ui
                                .allocate_exact_size(Vec2::new(inner_w, header_h), Sense::hover());
                            let header_rect = hdr_resp.rect;
                            ui.painter().rect_filled(
                                header_rect,
                                0.0,
                                Color32::from_rgba_unmultiplied(glow.r(), glow.g(), glow.b(), 14),
                            );
                            let header_content = egui::Rect::from_min_max(
                                Pos2::new(
                                    header_rect.left() + CYBER_HEADER_PAD_X,
                                    header_rect.top() + CYBER_HEADER_PAD_TOP,
                                ),
                                Pos2::new(
                                    header_rect.right() - CYBER_HEADER_PAD_X,
                                    header_rect.bottom() - CYBER_HEADER_PAD_BOTTOM,
                                ),
                            );
                            let mut cyber_close = false;
                            ui.scope_builder(
                                egui::UiBuilder::new().max_rect(header_content),
                                |ui| {
                                    // Title centered in the bar; close pinned to the far right.
                                    const CLOSE_DIAM: f32 = 18.0;
                                    const CLOSE_RIGHT_PAD: f32 = 2.0;
                                    const CLOSE_TITLE_GAP: f32 = 4.0;

                                    let full = ui.max_rect();
                                    let close_left = full.right()
                                        - CLOSE_RIGHT_PAD
                                        - CLOSE_DIAM
                                        - CLOSE_TITLE_GAP;
                                    let title_rect = egui::Rect::from_min_max(
                                        full.min,
                                        Pos2::new(close_left.max(full.min.x), full.max.y),
                                    );
                                    ui.scope_builder(
                                        egui::UiBuilder::new().max_rect(title_rect),
                                        |ui| {
                                            ui.horizontal_centered(|ui| {
                                                ui.label(
                                                    RichText::new(format!(
                                                        "Uploaded images - {}",
                                                        images.len()
                                                    ))
                                                    .family(FontFamily::Monospace)
                                                    .size(14.0)
                                                    .color(p.text),
                                                );
                                            });
                                        },
                                    );

                                    let close_center = Pos2::new(
                                        full.right() - CLOSE_RIGHT_PAD - CLOSE_DIAM * 0.5,
                                        full.center().y,
                                    );
                                    let close_rect = egui::Rect::from_center_size(
                                        close_center,
                                        Vec2::splat(CLOSE_DIAM),
                                    );
                                    let close_resp = ui.allocate_rect(close_rect, Sense::click());
                                    let cr = CLOSE_DIAM * 0.5;
                                    ui.painter().rect_filled(close_rect, cr, rim);
                                    ui.painter().text(
                                        close_rect.center(),
                                        Align2::CENTER_CENTER,
                                        "×",
                                        FontId::monospace(10.0),
                                        p.term_bg,
                                    );
                                    if close_resp.clicked() {
                                        cyber_close = true;
                                    }
                                    close_resp.on_hover_cursor(CursorIcon::PointingHand);
                                },
                            );
                            if cyber_close {
                                close_gallery = true;
                            }
                            let sep_y = header_rect.bottom() - 0.75;
                            ui.painter().line_segment(
                                [
                                    Pos2::new(outer.left(), sep_y),
                                    Pos2::new(outer.right(), sep_y),
                                ],
                                Stroke::new(
                                    1.5,
                                    Color32::from_rgba_unmultiplied(
                                        glow.r(),
                                        glow.g(),
                                        glow.b(),
                                        80,
                                    ),
                                ),
                            );
                        } else {
                            ui.horizontal(|ui| {
                                ui.add_space(14.0);
                                ui.label(
                                    RichText::new(format!(
                                        "Uploaded images  \u{2022}  {}",
                                        images.len()
                                    ))
                                    .size(12.5)
                                    .color(accent)
                                    .strong(),
                                );
                                ui.with_layout(
                                    egui::Layout::right_to_left(egui::Align::Center),
                                    |ui| {
                                        ui.add_space(10.0);
                                        let x_resp = ui
                                            .add(
                                                egui::Label::new(
                                                    RichText::new(" \u{2715} ")
                                                        .size(13.0)
                                                        .color(p.tab_close),
                                                )
                                                .sense(Sense::click()),
                                            )
                                            .on_hover_cursor(CursorIcon::PointingHand);
                                        if x_resp.clicked() {
                                            close_gallery = true;
                                        }
                                    },
                                );
                            });
                            ui.painter().hline(
                                outer.left()..=outer.right(),
                                ui.cursor().min.y,
                                Stroke::new(1.0, p.border),
                            );
                            ui.add_space(1.0);
                        }

                        // Selection action bar (both themes).
                        if !selected_snapshot.is_empty() {
                            ui.horizontal(|ui| {
                                ui.add_space(4.0);
                                ui.label(
                                    RichText::new(format!(
                                        "{} selected",
                                        selected_snapshot.len()
                                    ))
                                    .size(11.0)
                                    .color(accent),
                                );
                                ui.with_layout(
                                    egui::Layout::right_to_left(egui::Align::Center),
                                    |ui| {
                                        ui.add_space(4.0);
                                        let del_sel_resp = ui
                                            .add(
                                                egui::Button::new(
                                                    RichText::new(format!(
                                                        "Delete {} selected",
                                                        selected_snapshot.len()
                                                    ))
                                                    .size(11.0)
                                                    .color(p.text),
                                                )
                                                .fill(color_with_alpha(p.tab_close, 200))
                                                .stroke(Stroke::new(1.0, p.tab_close))
                                                .corner_radius(6.0),
                                            )
                                            .on_hover_cursor(CursorIcon::PointingHand);
                                        if del_sel_resp.clicked() {
                                            delete_selected_action = true;
                                        }
                                    },
                                );
                            });
                            ui.add_space(2.0);
                        }

                        ui.add_space(6.0);

                        // Use remaining content height so the grid doesn't get clipped by
                        // stale cursor/min-rect math after header/layout changes.
                        let scroll_h = (ui.available_height()
                            - if cyber_shell { CYBER_BOTTOM_STRIP } else { 0.0 })
                        .max(120.0);

                        let thumb_corner = 4.0_f32;
                        let ph_font = if cyber_shell {
                            FontId::monospace(18.0)
                        } else {
                            FontId::proportional(24.0)
                        };
                        let ph_glyph = if cyber_shell { "▢" } else { "\u{1f5bc}" };

                        // ── Image grid ───────────────────────────────────────────
                        let cols = 3usize;
                        let padding = 10.0;

                        egui::ScrollArea::vertical()
                            .max_height(scroll_h)
                            .show(ui, |ui| {
                                ui.add_space(4.0);
                                let content_w = ui.available_width().max(1.0);
                                let cell_w = ((content_w - padding * (cols as f32 - 1.0))
                                    / cols as f32)
                                    .max(80.0);
                                // Tall enough for four stacked hover actions (View / Select / Copy / Delete).
                                const HOVER_ACTION_STACK_H: f32 = 26.0 * 4.0 + 7.0 * 3.0 + 10.0;
                                let cell_h = cell_w.max(HOVER_ACTION_STACK_H);
                                ui.spacing_mut().item_spacing = egui::vec2(padding, padding);
                                for row in images.chunks(cols) {
                                    ui.horizontal(|ui| {
                                        for path in row {
                                            ui.vertical(|ui| {
                                                ui.set_width(cell_w);
                                                let (thumb_rect, thumb_resp) = ui
                                                    .allocate_exact_size(
                                                        egui::vec2(cell_w, cell_h),
                                                        Sense::click(),
                                                    );

                                                // Record for rubber-band hit-testing next frame.
                                                this_frame_thumb_rects.push((path.clone(), thumb_rect));

                                                // Selection state.
                                                let is_selected = selected_snapshot.contains(path);
                                                let is_rubber_band = rubber_band_snapshot
                                                    .map(|(start, cur)| {
                                                        let rb = egui::Rect::from_two_pos(start, cur);
                                                        (rb.width() > 5.0 || rb.height() > 5.0)
                                                            && thumb_rects_last
                                                                .iter()
                                                                .find(|(p, _)| p == path)
                                                                .map(|(_, r)| rb.intersects(*r))
                                                                .unwrap_or(false)
                                                    })
                                                    .unwrap_or(false);
                                                let effective_selected = is_selected || is_rubber_band;

                                                // Keep hover overlay stable while interacting with
                                                // overlay buttons inside the thumbnail rect.
                                                let is_hovered =
                                                    ui.rect_contains_pointer(thumb_rect);
                                                let border_col = if effective_selected {
                                                    accent
                                                } else if is_hovered {
                                                    rim
                                                } else {
                                                    thumb_border
                                                };
                                                let border_w = if effective_selected {
                                                    2.0
                                                } else if is_hovered {
                                                    1.5
                                                } else {
                                                    1.0
                                                };
                                                ui.painter().rect_filled(
                                                    thumb_rect,
                                                    thumb_corner,
                                                    thumb_bg,
                                                );
                                                ui.painter().rect_stroke(
                                                    thumb_rect,
                                                    thumb_corner,
                                                    Stroke::new(border_w, border_col),
                                                    egui::StrokeKind::Middle,
                                                );

                                                if let Some(tex) =
                                                    self.image_gallery_textures.get(path)
                                                {
                                                    let img_corner = (thumb_corner - 1.0).max(1.0);
                                                    let img = egui::Image::from_texture(
                                                        egui::load::SizedTexture::new(
                                                            tex.id(),
                                                            egui::vec2(
                                                                tex.size()[0] as f32,
                                                                tex.size()[1] as f32,
                                                            ),
                                                        ),
                                                    )
                                                    .fit_to_exact_size(egui::vec2(
                                                        cell_w - 4.0,
                                                        cell_h - 4.0,
                                                    ))
                                                    .corner_radius(img_corner);
                                                    img.paint_at(ui, thumb_rect.shrink(2.0));
                                                } else {
                                                    ui.painter().text(
                                                        thumb_rect.center(),
                                                        Align2::CENTER_CENTER,
                                                        ph_glyph,
                                                        ph_font.clone(),
                                                        muted_color,
                                                    );
                                                }

                                                // Selection overlay + checkmark badge.
                                                if effective_selected {
                                                    ui.painter().rect_filled(
                                                        thumb_rect,
                                                        thumb_corner,
                                                        color_with_alpha(accent, if is_hovered { 55 } else { 38 }),
                                                    );
                                                    let badge = egui::Rect::from_min_size(
                                                        thumb_rect.min + egui::vec2(6.0, 6.0),
                                                        Vec2::splat(16.0),
                                                    );
                                                    ui.painter().rect_filled(badge, 8.0, accent);
                                                    ui.painter().text(
                                                        badge.center(),
                                                        Align2::CENTER_CENTER,
                                                        "\u{2713}",
                                                        FontId::monospace(10.0),
                                                        p.term_bg,
                                                    );
                                                }

                                                if is_hovered {
                                                    if !effective_selected {
                                                        ui.painter().rect_filled(
                                                            thumb_rect,
                                                            thumb_corner,
                                                            color_with_alpha(rim, 38),
                                                        );
                                                    }

                                                    let btn_h = 26.0;
                                                    let btn_gap = 7.0;
                                                    let btn_w = (thumb_rect.width() - 20.0)
                                                        .clamp(86.0, 140.0);
                                                    let stack_h = btn_h * 4.0 + btn_gap * 3.0;
                                                    let cx = thumb_rect.center().x;
                                                    let btn_y =
                                                        thumb_rect.center().y - stack_h * 0.5;
                                                    let view_rect = egui::Rect::from_min_size(
                                                        Pos2::new(cx - btn_w * 0.5, btn_y),
                                                        Vec2::new(btn_w, btn_h),
                                                    );
                                                    let select_rect = egui::Rect::from_min_size(
                                                        Pos2::new(
                                                            cx - btn_w * 0.5,
                                                            view_rect.bottom() + btn_gap,
                                                        ),
                                                        Vec2::new(btn_w, btn_h),
                                                    );
                                                    let copy_rect = egui::Rect::from_min_size(
                                                        Pos2::new(
                                                            cx - btn_w * 0.5,
                                                            select_rect.bottom() + btn_gap,
                                                        ),
                                                        Vec2::new(btn_w, btn_h),
                                                    );
                                                    let delete_rect = egui::Rect::from_min_size(
                                                        Pos2::new(
                                                            cx - btn_w * 0.5,
                                                            copy_rect.bottom() + btn_gap,
                                                        ),
                                                        Vec2::new(btn_w, btn_h),
                                                    );

                                                    let view_resp = ui.put(
                                                        view_rect,
                                                        egui::Button::new(
                                                            RichText::new("View")
                                                                .italics()
                                                                .family(FontFamily::Monospace)
                                                                .size(11.0)
                                                                .color(p.term_bg),
                                                        )
                                                        .fill(color_with_alpha(rim, 230))
                                                        .stroke(Stroke::new(1.0, rim))
                                                        .corner_radius(8.0),
                                                    );
                                                    let select_resp = ui.put(
                                                        select_rect,
                                                        egui::Button::new(
                                                            RichText::new("Select")
                                                                .italics()
                                                                .family(FontFamily::Monospace)
                                                                .size(11.0)
                                                                .color(p.term_bg),
                                                        )
                                                        .fill(color_with_alpha(rim, 230))
                                                        .stroke(Stroke::new(1.0, rim))
                                                        .corner_radius(8.0),
                                                    );
                                                    let copy_resp = ui.put(
                                                        copy_rect,
                                                        egui::Button::new(
                                                            RichText::new("Copy")
                                                                .italics()
                                                                .family(FontFamily::Monospace)
                                                                .size(11.0)
                                                                .color(p.term_bg),
                                                        )
                                                        .fill(color_with_alpha(rim, 230))
                                                        .stroke(Stroke::new(1.0, rim))
                                                        .corner_radius(8.0),
                                                    );
                                                    let delete_resp = ui.put(
                                                        delete_rect,
                                                        egui::Button::new(
                                                            RichText::new("Delete")
                                                                .italics()
                                                                .family(FontFamily::Monospace)
                                                                .size(11.0)
                                                                .color(p.text),
                                                        )
                                                        .fill(color_with_alpha(p.tab_close, 212))
                                                        .stroke(Stroke::new(1.0, p.tab_close))
                                                        .corner_radius(8.0),
                                                    );
                                                    if view_resp.hovered()
                                                        || select_resp.hovered()
                                                        || copy_resp.hovered()
                                                        || delete_resp.hovered()
                                                    {
                                                        ui.ctx().set_cursor_icon(
                                                            CursorIcon::PointingHand,
                                                        );
                                                    }
                                                    if view_resp.clicked() {
                                                        open_view_path = Some(path.clone());
                                                    }
                                                    if select_resp.clicked() {
                                                        reuse_path = Some(path.clone());
                                                    }
                                                    if copy_resp.clicked() {
                                                        copy_image_path = Some(path.clone());
                                                    }
                                                    if delete_resp.clicked() {
                                                        delete_path = Some(path.clone());
                                                    }

                                                    // Click on thumb background (not on overlay buttons) → selection.
                                                    if thumb_resp.clicked() {
                                                        let click_pos = thumb_resp
                                                            .interact_pointer_pos()
                                                            .unwrap_or_default();
                                                        let on_btn = view_rect.contains(click_pos)
                                                            || select_rect.contains(click_pos)
                                                            || copy_rect.contains(click_pos)
                                                            || delete_rect.contains(click_pos);
                                                        if !on_btn {
                                                            let mods =
                                                                ui.input(|i| i.modifiers);
                                                            click_select = Some((
                                                                path.clone(),
                                                                mods.ctrl || mods.mac_cmd,
                                                                mods.shift,
                                                            ));
                                                        }
                                                    }
                                                } else {
                                                    // No overlay — direct click selects.
                                                    if thumb_resp.clicked() {
                                                        let mods = ui.input(|i| i.modifiers);
                                                        click_select = Some((
                                                            path.clone(),
                                                            mods.ctrl || mods.mac_cmd,
                                                            mods.shift,
                                                        ));
                                                    } else if thumb_resp.hovered() {
                                                        ui.ctx().set_cursor_icon(
                                                            CursorIcon::PointingHand,
                                                        );
                                                    }
                                                }
                                            });
                                        }
                                    });
                                }
                                ui.add_space(8.0);
                            });

                        if cyber_shell {
                            let _ = ui.allocate_space(Vec2::new(outer.width(), CYBER_BOTTOM_STRIP));
                        }
                    });
            });

        // Apply deferred mutations after borrowing ctx.

        // Update cached thumb rects for rubber-band hit-testing.
        self.image_gallery_thumb_rects = this_frame_thumb_rects;

        // Rubber-band drag tracking.
        let pointer_pos = ctx.input(|i| i.pointer.latest_pos());
        let pointer_pressed = ctx.input(|i| i.pointer.primary_pressed());
        let pointer_down = ctx.input(|i| i.pointer.primary_down());
        let pointer_released = ctx.input(|i| i.pointer.primary_released());
        if pointer_pressed {
            if let Some(pos) = pointer_pos {
                if modal_rect.contains(pos) {
                    self.image_gallery_rubber_band = Some((pos, pos));
                }
            }
        } else if pointer_down {
            if let Some((start, _)) = self.image_gallery_rubber_band {
                if let Some(pos) = pointer_pos {
                    self.image_gallery_rubber_band = Some((start, pos));
                }
            }
        } else if pointer_released {
            if let Some((start, cur)) = self.image_gallery_rubber_band.take() {
                let rb = egui::Rect::from_two_pos(start, cur);
                if rb.width() > 5.0 || rb.height() > 5.0 {
                    let ctrl = ctx.input(|i| i.modifiers.ctrl || i.modifiers.mac_cmd);
                    if !ctrl {
                        self.image_gallery_selected.clear();
                    }
                    for (path, rect) in &self.image_gallery_thumb_rects {
                        if rb.intersects(*rect) {
                            self.image_gallery_selected.insert(path.clone());
                        }
                    }
                }
            }
        }

        // Paint rubber-band selection rectangle.
        if let Some((start, cur)) = self.image_gallery_rubber_band {
            let rb = egui::Rect::from_two_pos(start, cur);
            if rb.width() > 5.0 || rb.height() > 5.0 {
                let rb_fill = color_with_alpha(accent, 35);
                let rb_stroke = Stroke::new(1.0, color_with_alpha(accent, 180));
                let painter = ctx.layer_painter(egui::LayerId::new(
                    egui::Order::Tooltip,
                    egui::Id::new("gallery_rubber_band"),
                ));
                painter.rect(rb, 2.0, rb_fill, rb_stroke, egui::StrokeKind::Middle);
            }
        }

        // Handle click selection (ctrl/shift/plain).
        if let Some((path, ctrl, shift)) = click_select {
            if shift {
                if let Some(last) = &self.image_gallery_last_clicked.clone() {
                    let pos_a = images_order.iter().position(|p| p == &path);
                    let pos_b = images_order.iter().position(|p| p == last.as_str());
                    if let (Some(a), Some(b)) = (pos_a, pos_b) {
                        let (lo, hi) = if a <= b { (a, b) } else { (b, a) };
                        for p in &images_order[lo..=hi] {
                            self.image_gallery_selected.insert(p.clone());
                        }
                    }
                } else {
                    self.image_gallery_selected.insert(path.clone());
                }
            } else if ctrl {
                if self.image_gallery_selected.contains(&path) {
                    self.image_gallery_selected.remove(&path);
                } else {
                    self.image_gallery_selected.insert(path.clone());
                }
            } else {
                self.image_gallery_selected.clear();
                self.image_gallery_selected.insert(path.clone());
            }
            self.image_gallery_last_clicked = Some(path);
        }

        // Delete all selected images.
        if delete_selected_action && !self.image_gallery_selected.is_empty() {
            let to_delete: Vec<String> = self.image_gallery_selected.drain().collect();
            self.image_gallery_last_clicked = None;
            if let Some(rt) = self.workspace_runtime.get_mut(ws) {
                for path in &to_delete {
                    rt.uploaded_images.retain(|p| p != path);
                    self.image_gallery_textures.remove(path);
                    if self
                        .image_gallery_view
                        .as_ref()
                        .is_some_and(|(vp, _)| vp == path)
                    {
                        self.image_gallery_view = None;
                    }
                }
                if rt.uploaded_images.is_empty() {
                    rt.show_image_gallery = false;
                    self.image_gallery_view = None;
                }
            }
        }

        if close_gallery {
            self.image_gallery_view = None;
            self.image_gallery_selected.clear();
            self.image_gallery_rubber_band = None;
            self.image_gallery_last_clicked = None;
            if let Some(rt) = self.workspace_runtime.get_mut(ws) {
                rt.show_image_gallery = false;
            }
        }
        if let Some(path) = reuse_path {
            self.image_gallery_view = None;
            if let Some(idx) = active_idx {
                if let Some(rt) = self.workspace_runtime.get_mut(ws) {
                    if idx < rt.terminals.len() {
                        let bracketed = rt.terminals[idx].session.parser.bracketed_paste();
                        rt.line_editors[idx].push_paste(&path);
                        let bytes =
                            clipboard::clipboard_text_to_pty_bytes_with_mode(&path, bracketed);
                        let _ = rt.terminals[idx].backend.write_all(&bytes);
                    }
                }
            }
            if let Some(rt) = self.workspace_runtime.get_mut(ws) {
                rt.show_image_gallery = false;
            }
        }
        if let Some(path) = copy_image_path {
            let _ = clipboard::set_clipboard_image_from_path(&path);
        }
        if let Some(path) = delete_path {
            if self
                .image_gallery_view
                .as_ref()
                .is_some_and(|(vp, _)| vp == &path)
            {
                self.image_gallery_view = None;
            }
            self.image_gallery_textures.remove(&path);
            if let Some(rt) = self.workspace_runtime.get_mut(ws) {
                rt.uploaded_images.retain(|p| p != &path);
                if rt.uploaded_images.is_empty() {
                    rt.show_image_gallery = false;
                    self.image_gallery_view = None;
                }
            }
        }
        if let Some(path) = open_view_path {
            self.image_gallery_view = None;
            if let Some(tex) = load_image_thumbnail(&path, ctx, 2048, "view") {
                self.image_gallery_view = Some((path, tex));
            }
        }

        self.paint_image_gallery_viewer(ctx, p);
    }

    /// Full-window preview on top of the gallery (backdrop click, ×, or Escape closes).
    fn paint_image_gallery_viewer(&mut self, ctx: &egui::Context, p: UiPalette) {
        let (path, tex) = match self.image_gallery_view.clone() {
            Some(v) => v,
            None => return,
        };
        let cyber_shell = p.terminal_glow.is_some();
        let rim = p.terminal_border_active;
        let viewport = ctx.viewport_rect();
        let mut close_view = false;

        // Use `Debug` so this UI is always above the gallery panel (`Tooltip`). Otherwise the
        // gallery and viewer layers compete and the full-screen scrim can paint over the image
        // after reopening a different preview.
        let backdrop_id = egui::Id::new("image_gallery_view_backdrop");
        egui::Area::new(backdrop_id)
            .order(egui::Order::Debug)
            .fixed_pos(viewport.min)
            .show(ctx, |ui| {
                let (_, resp) = ui.allocate_exact_size(viewport.size(), Sense::click());
                // No extra dimming: the gallery modal already has a viewport scrim. A second
                // opaque layer here stacked too dark; clicks still close the preview.
                if resp.clicked() {
                    close_view = true;
                }
            });

        let tex_sz = tex.size_vec2().max(Vec2::splat(1.0));
        let max_img = (viewport.size() - Vec2::splat(48.0)).max(Vec2::splat(80.0)) * 0.92;
        let scale = (max_img.x / tex_sz.x)
            .min(max_img.y / tex_sz.y)
            .min(1.0)
            .max(0.0);
        let display_sz = (tex_sz * scale).max(Vec2::splat(1.0));
        // Frame uses `inner_margin(16)` on all sides: outer size must include title row + gap +
        // image plus vertical margins (otherwise the image is clipped against dark panel fill).
        let frame_pad = 16.0_f32;
        let title_row_h = 28.0_f32;
        let title_to_img_gap = 6.0_f32;
        let mut panel_size = egui::vec2(
            display_sz.x + frame_pad * 2.0,
            title_row_h + title_to_img_gap + display_sz.y + frame_pad * 2.0,
        );
        panel_size = panel_size.min(viewport.size() - Vec2::splat(24.0));
        let panel_rect = egui::Rect::from_center_size(viewport.center(), panel_size);
        let panel_bg = if cyber_shell {
            p.panel_bg
        } else {
            p.popover_fill
        };

        egui::Area::new(egui::Id::new("image_gallery_view_panel"))
            .order(egui::Order::Debug)
            .fixed_pos(panel_rect.min)
            .show(ctx, |ui| {
                ui.set_min_size(panel_size);
                ui.set_max_size(panel_size);
                egui::Frame::default()
                    .fill(panel_bg)
                    .stroke(Stroke::new(if cyber_shell { 1.25 } else { 1.5 }, rim))
                    .shadow(egui::epaint::Shadow {
                        offset: [0, 10],
                        blur: 22,
                        spread: 0,
                        color: if cyber_shell {
                            Color32::from_rgba_unmultiplied(rim.r(), rim.g(), rim.b(), 72)
                        } else {
                            Color32::from_rgba_unmultiplied(0, 0, 0, 96)
                        },
                    })
                    .corner_radius(if cyber_shell { 6.0 } else { 8.0 })
                    .inner_margin(Margin::same(frame_pad as i8))
                    .show(ui, |ui| {
                        // Reserve full body height; image is painted manually and won't
                        // contribute to egui layout size by itself.
                        ui.set_min_size(egui::vec2(
                            (panel_size.x - frame_pad * 2.0).max(1.0),
                            (panel_size.y - frame_pad * 2.0).max(1.0),
                        ));
                        let full = ui.max_rect();
                        let close_d = 22.0_f32;
                        let close_pad = 6.0_f32;
                        let close_rect = egui::Rect::from_min_size(
                            Pos2::new(full.right() - close_pad - close_d, full.top() + close_pad),
                            Vec2::splat(close_d),
                        );
                        let title_rect = egui::Rect::from_min_max(
                            Pos2::new(full.left() + 8.0, full.top() + close_pad),
                            Pos2::new(close_rect.left() - 4.0, close_rect.bottom()),
                        );
                        let close_resp = ui.allocate_rect(close_rect, Sense::click());
                        let cr = close_d * 0.5;
                        ui.painter().rect_filled(close_rect, cr, rim);
                        ui.painter().text(
                            close_rect.center(),
                            Align2::CENTER_CENTER,
                            "×",
                            FontId::monospace(12.0),
                            p.term_bg,
                        );
                        if close_resp.clicked() {
                            close_view = true;
                        }
                        close_resp.on_hover_cursor(CursorIcon::PointingHand);

                        let stem = std::path::Path::new(&path)
                            .file_name()
                            .and_then(|s| s.to_str())
                            .unwrap_or(path.as_str());
                        ui.scope_builder(egui::UiBuilder::new().max_rect(title_rect), |ui| {
                            ui.set_width(title_rect.width());
                            ui.horizontal(|ui| {
                                ui.add(
                                    egui::Label::new(
                                        RichText::new(stem)
                                            .family(FontFamily::Monospace)
                                            .size(11.0)
                                            .color(p.text),
                                    )
                                    .truncate(),
                                );
                            });
                        });

                        let img_top = title_rect.bottom() + title_to_img_gap;
                        let avail = Vec2::new(full.width(), (full.bottom() - img_top).max(1.0))
                            .max(Vec2::splat(1.0));
                        let fit = (avail.x / display_sz.x)
                            .min(avail.y / display_sz.y)
                            .min(1.0)
                            .max(0.0);
                        let img_wh = (display_sz * fit).max(Vec2::splat(1.0));
                        let img_rect = egui::Rect::from_center_size(
                            Pos2::new(full.center().x, img_top + img_wh.y * 0.5),
                            img_wh,
                        );
                        let st = egui::load::SizedTexture::new(tex.id(), tex_sz);
                        egui::widgets::paint_texture_at(
                            ui.painter(),
                            img_rect,
                            &egui::widgets::ImageOptions::default(),
                            &st,
                        );
                    });
            });

        if close_view {
            self.image_gallery_view = None;
        }
    }

    fn drain_terminals(&mut self) {
        for runtime in &mut self.workspace_runtime {
            for pane in &mut runtime.terminals {
                let _ = pane.session.drain_and_parse();
                // Flush any terminal responses (DA, DSR, etc.) back to the PTY.
                for response in pane.session.parser.drain_responses() {
                    let _ = pane.backend.write_all(&response);
                }
            }
        }
    }

    fn handle_keyboard_input(&mut self, ctx: &egui::Context) {
        if self.handle_workspace_close_history_shortcuts(ctx) {
            return;
        }
        self.ensure_workspace_runtime_slots();
        let ws = self
            .selected_workspace
            .min(self.workspaces.len().saturating_sub(1));
        if self
            .uploaded_images_no_terminal_hint_until
            .is_some_and(|until| Instant::now() < until)
            && ctx.input(|i| i.key_pressed(egui::Key::Escape))
        {
            self.uploaded_images_no_terminal_hint_until = None;
            return;
        }
        if self.workspace_runtime.get(ws).is_some_and(|rt| {
            rt.show_image_gallery && ctx.input(|i| i.key_pressed(egui::Key::Escape))
        }) {
            if self.image_gallery_view.is_some() {
                self.image_gallery_view = None;
            } else if let Some(rt) = self.workspace_runtime.get_mut(ws) {
                rt.show_image_gallery = false;
            }
            return;
        }
        let Some(runtime) = self.workspace_runtime.get(ws) else {
            return;
        };
        let Some(active_idx) = runtime.active_terminal else {
            return;
        };
        if active_idx >= runtime.terminals.len() {
            return;
        }
        self.route_workspace_terminal_keyboard(ctx, ws, active_idx);
    }

    fn handle_workspace_close_history_shortcuts(&mut self, ctx: &egui::Context) -> bool {
        let mut changed = false;
        let events = ctx.input(|i| i.events.clone());
        for event in events {
            let egui::Event::Key {
                key,
                pressed,
                modifiers,
                ..
            } = event
            else {
                continue;
            };
            if !pressed || key != egui::Key::Z || !modifiers.alt {
                continue;
            }
            let cmd_or_ctrl = modifiers.command || modifiers.ctrl;
            if !cmd_or_ctrl {
                continue;
            }
            if modifiers.shift {
                if self.can_redo_workspace_edit() {
                    changed |= self.redo_workspace_edit();
                }
            } else {
                if self.can_undo_workspace_edit() {
                    changed |= self.undo_workspace_edit();
                }
            }
        }
        changed
    }

    fn route_workspace_terminal_keyboard(
        &mut self,
        ctx: &egui::Context,
        workspace_idx: usize,
        active_idx: usize,
    ) {
        self.ensure_workspace_runtime_slots();
        let Some(runtime) = self.workspace_runtime.get_mut(workspace_idx) else {
            return;
        };
        if active_idx >= runtime.terminals.len() {
            return;
        }
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
        if runtime.scrollback_searches.len() < runtime.terminals.len() {
            runtime
                .scrollback_searches
                .resize_with(runtime.terminals.len(), ScrollbackSearchPaneState::default);
        } else if runtime.scrollback_searches.len() > runtime.terminals.len() {
            runtime
                .scrollback_searches
                .truncate(runtime.terminals.len());
        }

        let pane_id = runtime.terminals[active_idx].id;
        let search_id = scrollback_search_text_id(pane_id);
        let search_focused = ctx.memory(|m| m.focused() == Some(search_id));
        let search_open = runtime
            .scrollback_searches
            .get(active_idx)
            .is_some_and(|s| s.open);

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
                    if search_focused {
                        continue;
                    }
                    if !text.is_empty() {
                        runtime.line_editors[active_idx].push_text(&text);
                        runtime.terminals[active_idx]
                            .backend
                            .write_all(text.as_bytes());
                    }
                }
                egui::Event::Paste(text) => {
                    if search_focused {
                        continue;
                    }
                    let clean = clipboard::sanitize_pasted_terminal_text(&text);
                    let had_sel = runtime.selections[active_idx].is_some_and(|r| r.active);
                    let bracketed = runtime.terminals[active_idx]
                        .session
                        .parser
                        .bracketed_paste();
                    let delete_bytes = runtime.selections[active_idx]
                        .filter(|r| r.active)
                        .map(|range| {
                            let grid = runtime.terminals[active_idx].session.parser.grid();
                            selection_delete_bytes(grid, range, egui::Key::Backspace)
                        })
                        .unwrap_or_default();
                    runtime.selections[active_idx] = None;
                    if !clean.is_empty() {
                        let ed = &mut runtime.line_editors[active_idx];
                        if had_sel && !delete_bytes.is_empty() {
                            ed.replace_with_paste(&clean);
                        } else {
                            ed.push_paste(&clean);
                        }
                        let mut buf = delete_bytes;
                        buf.extend_from_slice(&clipboard::clipboard_text_to_pty_bytes_with_mode(
                            &clean, bracketed,
                        ));
                        let _ = runtime.terminals[active_idx].backend.write_all(&buf);
                    } else {
                        #[cfg(target_os = "macos")]
                        {
                            if let Some(img_path) = clipboard::save_clipboard_image() {
                                if !uploaded_images_contains_same_content(
                                    &runtime.uploaded_images,
                                    &img_path,
                                ) {
                                    runtime.uploaded_images.push(img_path.clone());
                                }
                                runtime.line_editors[active_idx].push_paste(&img_path);
                                let bytes = clipboard::clipboard_text_to_pty_bytes_with_mode(
                                    &img_path, bracketed,
                                );
                                let _ = runtime.terminals[active_idx].backend.write_all(&bytes);
                            }
                        }
                    }
                }
                // egui-winit turns Cmd+C / Cmd+X into these and does not emit `Key::C` / `Key::X`.
                egui::Event::Copy => {
                    if search_focused {
                        continue;
                    }
                    let shift = ctx.input(|i| i.modifiers.shift);
                    if let Some(range) = runtime.selections[active_idx].filter(|r| r.active) {
                        let grid = runtime.terminals[active_idx].session.parser.grid();
                        let text = if shift {
                            clipboard::selection_to_ansi_sgr_text(grid, range)
                        } else {
                            clipboard::sanitize_pasted_terminal_text(
                                &clipboard::selection_to_plain_text(grid, range),
                            )
                        };
                        let _ = clipboard::set_clipboard_text(&text);
                    }
                }
                egui::Event::Cut => {
                    if search_focused {
                        continue;
                    }
                    if let Some(range) = runtime.selections[active_idx].filter(|r| r.active) {
                        let grid = runtime.terminals[active_idx].session.parser.grid();
                        let text = clipboard::sanitize_pasted_terminal_text(
                            &clipboard::selection_to_plain_text(grid, range),
                        );
                        let _ = clipboard::set_clipboard_text(&text);
                        let bytes = selection_delete_bytes(grid, range, egui::Key::Backspace);
                        if !bytes.is_empty() {
                            let _ = runtime.terminals[active_idx].backend.write_all(&bytes);
                        }
                        runtime.selections[active_idx] = None;
                        ctx.with_plugin(
                            |label_sel: &mut egui::text_selection::LabelSelectionState| {
                                label_sel.clear_selection();
                            },
                        );
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

                    let cmd = modifiers.command;
                    let ctrl = modifiers.ctrl;
                    let shift = modifiers.shift;

                    let is_open_find = key == egui::Key::F && (cmd || ctrl) && !shift;
                    if is_open_find {
                        runtime.scrollback_searches[active_idx].open = true;
                        runtime.scrollback_search_focus_pane.set(Some(pane_id));
                        continue;
                    }

                    if search_open && key == egui::Key::Escape {
                        runtime.scrollback_searches[active_idx].open = false;
                        continue;
                    }

                    if search_open && key == egui::Key::F3 {
                        scrollback_search_advance_pane(
                            &runtime.terminals[active_idx],
                            &mut runtime.scrollback_searches[active_idx],
                            if shift { -1 } else { 1 },
                        );
                        continue;
                    }

                    if search_focused && key == egui::Key::Enter {
                        scrollback_search_advance_pane(
                            &runtime.terminals[active_idx],
                            &mut runtime.scrollback_searches[active_idx],
                            if shift { -1 } else { 1 },
                        );
                        continue;
                    }

                    // ── Undo / redo typed text ────────────────────────────────
                    if search_focused
                        && ((cmd && !shift && key == egui::Key::Z)
                            || (cmd && shift && key == egui::Key::Z))
                    {
                        continue;
                    }
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

                    // Select all: input block (cursor row + contiguous non-empty live rows above),
                    // not scrollback / full buffer. Not plain Ctrl+A (readline beginning-of-line).
                    let is_select_all = (cmd && key == egui::Key::A)
                        || (cmd && shift && key == egui::Key::A)
                        || (ctrl && shift && key == egui::Key::A);
                    if is_select_all && search_focused {
                        continue;
                    }
                    if is_select_all {
                        let grid = runtime.terminals[active_idx].session.parser.grid();
                        if let Some(sel) = clipboard::selection_range_select_input_block(grid) {
                            runtime.selections[active_idx] = Some(sel);
                        }
                        ctx.with_plugin(
                            |label_sel: &mut egui::text_selection::LabelSelectionState| {
                                label_sel.clear_selection();
                            },
                        );
                        continue;
                    }

                    // Copy: plain (Cmd+C, Ctrl+Shift+C) for pasting back into the shell; rich (Cmd+Shift+C) with ANSI SGR.
                    let is_copy_rich = cmd && shift && key == egui::Key::C;
                    let is_copy_plain = (cmd && !shift && key == egui::Key::C)
                        || (ctrl && shift && key == egui::Key::C);
                    if (is_copy_plain || is_copy_rich) && search_focused {
                        continue;
                    }
                    if is_copy_rich {
                        if let Some(range) = runtime.selections[active_idx].filter(|r| r.active) {
                            let grid = runtime.terminals[active_idx].session.parser.grid();
                            let text = clipboard::selection_to_ansi_sgr_text(grid, range);
                            let _ = clipboard::set_clipboard_text(&text);
                        }
                        continue;
                    }
                    if is_copy_plain {
                        if let Some(range) = runtime.selections[active_idx].filter(|r| r.active) {
                            let grid = runtime.terminals[active_idx].session.parser.grid();
                            let text = clipboard::sanitize_pasted_terminal_text(
                                &clipboard::selection_to_plain_text(grid, range),
                            );
                            let _ = clipboard::set_clipboard_text(&text);
                        }
                        continue;
                    }

                    // Cmd+Shift+I — open file picker and paste the selected image path.
                    let is_insert_image = cmd && shift && key == egui::Key::I;
                    if is_insert_image && !search_focused {
                        if let Some(file) = FileDialog::new()
                            .add_filter(
                                "Images",
                                &["png", "jpg", "jpeg", "gif", "webp", "bmp", "tiff", "tif"],
                            )
                            .pick_file()
                        {
                            let path = file.to_string_lossy().into_owned();
                            if !uploaded_images_contains_same_content(
                                &runtime.uploaded_images,
                                &path,
                            ) {
                                runtime.uploaded_images.push(path.clone());
                            }
                            let bracketed = runtime.terminals[active_idx]
                                .session
                                .parser
                                .bracketed_paste();
                            let ed = &mut runtime.line_editors[active_idx];
                            ed.push_paste(&path);
                            let bytes =
                                clipboard::clipboard_text_to_pty_bytes_with_mode(&path, bracketed);
                            let _ = runtime.terminals[active_idx].backend.write_all(&bytes);
                        }
                        continue;
                    }

                    // Paste from system clipboard (keyboard); `Event::Paste` also handles OS paste.
                    let is_paste = (cmd && !shift && key == egui::Key::V)
                        || (ctrl && shift && key == egui::Key::V)
                        || (ctrl && !shift && key == egui::Key::V)
                        || (shift && key == egui::Key::Insert);
                    if is_paste && search_focused {
                        continue;
                    }
                    if is_paste {
                        let clip_text = clipboard::get_clipboard_text().ok();
                        let clean = clip_text
                            .as_deref()
                            .map(clipboard::sanitize_pasted_terminal_text)
                            .unwrap_or_default();

                        if !clean.is_empty() {
                            let had_sel = runtime.selections[active_idx].is_some_and(|r| r.active);
                            let bracketed = runtime.terminals[active_idx]
                                .session
                                .parser
                                .bracketed_paste();
                            let delete_bytes = runtime.selections[active_idx]
                                .filter(|r| r.active)
                                .map(|range| {
                                    let grid = runtime.terminals[active_idx].session.parser.grid();
                                    selection_delete_bytes(grid, range, egui::Key::Backspace)
                                })
                                .unwrap_or_default();
                            runtime.selections[active_idx] = None;
                            let ed = &mut runtime.line_editors[active_idx];
                            if had_sel && !delete_bytes.is_empty() {
                                ed.replace_with_paste(&clean);
                            } else {
                                ed.push_paste(&clean);
                            }
                            let mut buf = delete_bytes;
                            buf.extend_from_slice(
                                &clipboard::clipboard_text_to_pty_bytes_with_mode(
                                    &clean, bracketed,
                                ),
                            );
                            let _ = runtime.terminals[active_idx].backend.write_all(&buf);
                        } else {
                            runtime.selections[active_idx] = None;
                            #[cfg(target_os = "macos")]
                            {
                                if let Some(img_path) = clipboard::save_clipboard_image() {
                                    if !uploaded_images_contains_same_content(
                                        &runtime.uploaded_images,
                                        &img_path,
                                    ) {
                                        runtime.uploaded_images.push(img_path.clone());
                                    }
                                    let bracketed = runtime.terminals[active_idx]
                                        .session
                                        .parser
                                        .bracketed_paste();
                                    runtime.line_editors[active_idx].push_paste(&img_path);
                                    let bytes = clipboard::clipboard_text_to_pty_bytes_with_mode(
                                        &img_path, bracketed,
                                    );
                                    let _ = runtime.terminals[active_idx].backend.write_all(&bytes);
                                }
                            }
                        }
                        continue;
                    }

                    if matches!(key, egui::Key::Backspace | egui::Key::Delete) {
                        if search_focused {
                            continue;
                        }
                        if let Some(range) = runtime.selections[active_idx].filter(|r| r.active) {
                            let grid = runtime.terminals[active_idx].session.parser.grid();
                            let bytes = selection_delete_bytes(grid, range, key);
                            if !bytes.is_empty() {
                                runtime.terminals[active_idx].backend.write_all(&bytes);
                            }
                            runtime.selections[active_idx] = None;
                            // Clear egui's label galley selection too; otherwise the blue
                            // overlay persists after grid-based deletion.
                            ctx.with_plugin(
                                |label_sel: &mut egui::text_selection::LabelSelectionState| {
                                    label_sel.clear_selection();
                                },
                            );
                            continue;
                        }
                        // Track single backspace in line editor.
                        if key == egui::Key::Backspace {
                            runtime.line_editors[active_idx].push_backspace();
                        }
                    }

                    // Update cursor offset for navigation keys so mid-line insertions
                    // are tracked accurately; reset only when the line context is lost.
                    if search_focused {
                        continue;
                    }
                    let submitted_text = if key == egui::Key::Enter {
                        Some(
                            runtime.line_editors[active_idx]
                                .current
                                .text
                                .trim()
                                .to_string(),
                        )
                    } else {
                        None
                    };
                    let ed = &mut runtime.line_editors[active_idx];
                    match key {
                        egui::Key::ArrowLeft => ed.move_left(),
                        egui::Key::ArrowRight => ed.move_right(),
                        egui::Key::Home => ed.move_to_start(),
                        egui::Key::End => ed.move_to_end(),
                        egui::Key::Enter
                        | egui::Key::ArrowUp
                        | egui::Key::ArrowDown
                        | egui::Key::PageUp
                        | egui::Key::PageDown
                        | egui::Key::Escape => ed.reset(),
                        _ if ctrl && key == egui::Key::C => ed.reset(),
                        _ if ctrl && key == egui::Key::U => ed.reset(),
                        _ if ctrl && key == egui::Key::W => ed.reset(),
                        _ if ctrl && key == egui::Key::A => ed.move_to_start(),
                        _ if ctrl && key == egui::Key::E => ed.move_to_end(),
                        _ => {}
                    }
                    if ctrl && key == egui::Key::C {
                        runtime.terminals[active_idx].agent_kind = TerminalAgentKind::Terminal;
                    }
                    if let Some(text) = submitted_text {
                        if matches!(text.as_str(), "exit" | "quit" | "logout") {
                            runtime.terminals[active_idx].agent_kind = TerminalAgentKind::Terminal;
                        }
                    }

                    if let Some(bytes) = key_to_ansi_bytes(key, shift, modifiers.ctrl) {
                        runtime.terminals[active_idx].backend.write_all(&bytes);
                    }
                }
                _ => {}
            }
        }

        if shortcut_new_terminal {
            let _ = self.add_terminal(ctx, None, None);
        }
    }

    fn terminal_location_by_pane_id(&self, pane_id: u64) -> Option<(usize, usize, String)> {
        for (ws, runtime) in self.workspace_runtime.iter().enumerate() {
            for (idx, pane) in runtime.terminals.iter().enumerate() {
                if pane.id == pane_id {
                    return Some((ws, idx, pane.title.clone()));
                }
            }
        }
        None
    }

    /// Fills the zoom viewport with a single terminal grid (native fullscreen window).
    fn paint_fullscreen_zoom_terminal(
        &mut self,
        ui: &mut egui::Ui,
        workspace_idx: usize,
        pane_idx: usize,
        p: UiPalette,
        pane_id: u64,
        dock_fullscreen: &mut bool,
    ) {
        self.ensure_workspace_runtime_slots();
        let Some(runtime) = self.workspace_runtime.get_mut(workspace_idx) else {
            return;
        };
        if pane_idx >= runtime.terminals.len() {
            return;
        }
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
        if runtime.scrollback_searches.len() < runtime.terminals.len() {
            runtime
                .scrollback_searches
                .resize_with(runtime.terminals.len(), ScrollbackSearchPaneState::default);
        } else if runtime.scrollback_searches.len() > runtime.terminals.len() {
            runtime
                .scrollback_searches
                .truncate(runtime.terminals.len());
        }

        let selected_ws = self
            .selected_workspace
            .min(self.workspaces.len().saturating_sub(1));
        if workspace_idx == selected_ws {
            runtime.active_terminal = Some(pane_idx);
        }

        const TOOLBAR_H: f32 = 32.0;
        egui::Frame::NONE
            .inner_margin(Margin::symmetric(12, 6))
            .show(ui, |ui| {
                ui.allocate_ui_with_layout(
                    Vec2::new(ui.available_width(), TOOLBAR_H),
                    egui::Layout::right_to_left(egui::Align::Center),
                    |ui| {
                        if ui.button("Skip").clicked() {
                            *dock_fullscreen = true;
                        }
                    },
                );
            });

        let content_rect = ui.available_rect_before_wrap();
        let terminal_size = content_rect.size();

        let search_highlight = if runtime
            .scrollback_searches
            .get(pane_idx)
            .is_some_and(|s| s.open)
        {
            let st = &runtime.scrollback_searches[pane_idx];
            let grid = runtime.terminals[pane_idx].session.parser.grid();
            let ranges = scrollback_compute_match_ranges(grid, &st.query);
            if ranges.is_empty() {
                None
            } else {
                let i = st.current_match % ranges.len();
                Some(ranges[i])
            }
        } else {
            None
        };

        let clicked_cell_from_grid = {
            let pane = &mut runtime.terminals[pane_idx];
            resize_terminal_for_size(pane, terminal_size);
            let grid = pane.session.parser.grid();
            let synthetic_cursor_overlay = use_synthetic_cursor_overlay(&pane.session.parser, grid);
            let selection = &mut runtime.selections[pane_idx];
            ui.scope_builder(egui::UiBuilder::new().max_rect(content_rect), |ui| {
                render_terminal_grid(
                    ui,
                    pane.id,
                    grid,
                    p,
                    selection,
                    true,
                    synthetic_cursor_overlay,
                    search_highlight,
                    &mut pane.pending_initial_scroll_to_bottom,
                    &mut pane.last_autoscroll_caret_v,
                )
            })
            .inner
        };

        let pane_response = ui.interact(
            content_rect,
            ui.id().with(("fs_term_click", pane_id)),
            Sense::click(),
        );

        if let Some((clicked_vrow, clicked_col)) = clicked_cell_from_grid {
            let pane = &mut runtime.terminals[pane_idx];
            let grid = pane.session.parser.grid();
            let sb = grid.scrollback_len();
            if clicked_vrow >= sb && grid.cols > 0 {
                let clicked_row = clicked_vrow - sb;
                let target_row = clicked_row.min(grid.rows.saturating_sub(1));
                let row_end = row_render_end(grid, target_row).min(grid.cols.saturating_sub(1));
                let target_col = clicked_col
                    .min(row_end.saturating_add(1))
                    .min(grid.cols.saturating_sub(1));

                let mut bytes = Vec::new();
                if clicked_col >= row_end {
                    bytes.push(0x05);
                    if let Some(ed) = runtime.line_editors.get_mut(pane_idx) {
                        ed.move_to_end();
                    }
                } else if target_col > grid.cursor.col {
                    let steps = target_col - grid.cursor.col;
                    bytes.reserve(steps * 3);
                    for _ in 0..steps {
                        bytes.extend_from_slice(b"\x1b[C");
                    }
                    if let Some(ed) = runtime.line_editors.get_mut(pane_idx) {
                        ed.move_cursor_delta(steps as isize);
                    }
                } else if target_col < grid.cursor.col {
                    let steps = grid.cursor.col - target_col;
                    bytes.reserve(steps * 3);
                    for _ in 0..steps {
                        bytes.extend_from_slice(b"\x1b[D");
                    }
                    if let Some(ed) = runtime.line_editors.get_mut(pane_idx) {
                        ed.move_cursor_delta(-(steps as isize));
                    }
                }

                if !bytes.is_empty() {
                    pane.backend.write_all(&bytes);
                }
            }
        }

        if pane_response.clicked() && clicked_cell_from_grid.is_none() {
            let pane = &mut runtime.terminals[pane_idx];
            pane.backend.write_all(&[0x05]);
        }
    }

    fn ensure_agent_icon_textures(&mut self, ctx: &egui::Context) {
        if self.agent_icon_claude.is_none() {
            self.agent_icon_claude = load_embedded_icon_texture(ctx, "claude", CLAUDE_ICON_PNG);
        }
        if self.agent_icon_codex.is_none() {
            self.agent_icon_codex = load_embedded_icon_texture(ctx, "codex", CODEX_ICON_PNG);
        }
        if self.agent_icon_cursor.is_none() {
            self.agent_icon_cursor = load_embedded_icon_texture(ctx, "cursor", CURSOR_ICON_PNG);
        }
    }

    fn launch_cli_tool(
        &mut self,
        ctx: &egui::Context,
        target_terminal: Option<usize>,
        command: &str,
    ) {
        let pending_target = self.pending_context_terminal.take();
        self.ensure_workspace_runtime_slots();
        if self
            .active_workspace_runtime()
            .is_some_and(|runtime| runtime.terminals.is_empty())
        {
            if !self.add_terminal(ctx, None, None) {
                return;
            }
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
        if let Some(agent_kind) = TerminalAgentKind::from_command(command) {
            runtime.terminals[idx].agent_kind = agent_kind;
        }
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

/// Ensure the daemon is running and reachable before the startup restore loop.
/// Called once at init so that every subsequent `connect_daemon()` call during
/// pane restoration finds the daemon already up and connects immediately,
/// instead of each pane paying the full 100 ms × 30 retry wait independently.
fn warm_up_daemon() {
    if std::env::var("MULTERM_DAEMON_DISABLED").ok().as_deref() == Some("1") {
        return;
    }
    // Spawn the daemon if needed, then wait until it's reachable (same budget
    // as connect_daemon uses).
    let mut spawned = false;
    for _attempt in 0..30 {
        if let Ok(port_file) = daemon::daemon_port_file_path() {
            if let Ok(port_s) = fs::read_to_string(&port_file) {
                if let Ok(port) = port_s.trim().parse::<u16>() {
                    if TcpStream::connect(("127.0.0.1", port)).is_ok() {
                        return;
                    }
                }
            }
        }
        if !spawned {
            if let Ok(exe) = std::env::current_exe() {
                let _ = Command::new(exe)
                    .arg("--daemon")
                    .stdin(Stdio::null())
                    .stdout(Stdio::null())
                    .stderr(Stdio::null())
                    .spawn();
            }
            spawned = true;
        }
        std::thread::sleep(Duration::from_millis(100));
    }
}

fn connect_daemon() -> Option<TcpStream> {
    if std::env::var("MULTERM_DAEMON_DISABLED").ok().as_deref() == Some("1") {
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
    let cwd_resolved = ensure_working_dir_for_spawn(working_dir);

    // Prefer the built-in daemon so terminal state (e.g. a running Claude session)
    // survives closing/reopening this UI.
    if let Some(mut stream) = connect_daemon() {
        if let Ok(mut reader) = stream.try_clone() {
            // Attach payload is built in `daemon::attach_request_payload` (key, rows/cols, optional cwd).
            let cwd = (!cwd_resolved.is_empty()).then_some(cwd_resolved.as_str());
            if let Some(payload) = crate::daemon::attach_request_payload(tmux_session, 24, 80, cwd)
            {
                if write_frame_tcp(&mut stream, FRAME_ATTACH, &payload).is_ok() {
                    if let Ok((ft, first_payload)) = read_frame_tcp(&mut reader) {
                        if ft == FRAME_OUTPUT {
                            let _ = tx.send(first_payload);

                            let writer = Arc::new(Mutex::new(stream));
                            let tx_thread = tx;

                            std::thread::spawn(move || loop {
                                let Ok((ft, payload)) = read_frame_tcp(&mut reader) else {
                                    break;
                                };
                                if ft == FRAME_OUTPUT {
                                    let _ = tx_thread.send(payload);
                                } else if ft == FRAME_ATTACH_ERROR {
                                    break;
                                }
                            });

                            return TerminalPane {
                                id: next_terminal_id,
                                title,
                                agent_kind: TerminalAgentKind::Terminal,
                                tmux_session: tmux_session.to_string(),
                                session: TerminalSession::new(PaneId::new(), 24, 80, rx),
                                backend: TerminalBackend::DaemonPty { writer },
                                desired_size: Vec2::new(520.0, 280.0),
                                position: None,
                                pending_initial_scroll_to_bottom: true,
                                last_autoscroll_caret_v: None,
                                border_light_pos: None,
                                git: git_changes::TerminalGitSession::begin(working_dir, None),
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
        Some(cwd_resolved.as_str()),
        None,
    )
    .expect("spawn terminal pty");
    let shell_pid = pty.shell_pid;

    TerminalPane {
        id: next_terminal_id,
        title,
        agent_kind: TerminalAgentKind::Terminal,
        tmux_session: tmux_session.to_string(),
        session: TerminalSession::new(PaneId::new(), 24, 80, rx),
        backend: TerminalBackend::LocalPty { pty },
        desired_size: Vec2::new(520.0, 280.0),
        position: None,
        pending_initial_scroll_to_bottom: true,
        last_autoscroll_caret_v: None,
        border_light_pos: None,
        git: git_changes::TerminalGitSession::begin(working_dir, shell_pid),
    }
}

fn tmux_session_name(workspace_idx: usize, terminal_id: u64) -> String {
    format!("multerm-w{}-t{}", workspace_idx + 1, terminal_id)
}

fn resize_terminal_for_size(pane: &mut TerminalPane, size: Vec2) {
    let cols = (size.x / CELL_W).max(1.0) as usize;
    let rows = (size.y / CELL_H).max(1.0) as usize;
    pane.session.parser.resize(rows, cols);
    pane.backend.resize(rows as u16, cols as u16);
}

/// Vertical space above the PTY grid inside a pane frame (title row, separator, margins).
const TERMINAL_GRID_CHROME_Y: f32 = 44.0;

/// How closely a live gap must match a gap measured elsewhere to count as the same (px).
/// Integer-rounded gaps use strict pixel equality for highlight (`gap_matches_used`).
const USED_GAP_MATCH_EPS: f32 = 0.51;
/// Magnet distance (px): snap a live neighbor gap to a gap already used between other terminals.
const USED_GAP_SNAP_DISTANCE: f32 = 2.0;

#[derive(Clone, Copy)]
struct PaneGapDim {
    px: f32,
    /// Workspace-local segment endpoints (add `content_origin` to paint).
    a: Pos2,
    b: Pos2,
    /// `true` when this measures horizontal distance (compare to other terminals' side gaps).
    is_horizontal: bool,
}

#[derive(Clone, Copy)]
struct PaneNeighborDimensions {
    left: PaneGapDim,
    right: PaneGapDim,
    top: PaneGapDim,
    bottom: PaneGapDim,
}

/// Gaps from this pane to the nearest neighbor on each axis, with segment geometry for guides.
/// `others` must not include the pane being measured.
fn pane_neighbor_dimensions<'a>(
    pos: Pos2,
    size: Vec2,
    others: impl Iterator<Item = &'a TerminalPane>,
    workspace_right: f32,
    workspace_bottom: f32,
) -> PaneNeighborDimensions {
    let x0 = pos.x;
    let y0 = pos.y;
    let x1 = pos.x + size.x;
    let y1 = pos.y + size.y;

    let horiz_overlap = |oy0: f32, oy1: f32| -> bool { (y1.min(oy1) - y0.max(oy0)).max(0.0) > 0.0 };
    let vert_overlap = |ox0: f32, ox1: f32| -> bool { (x1.min(ox1) - x0.max(ox0)).max(0.0) > 0.0 };

    let mut best_left: Option<(f32, f32)> = None; // (neighbor_right_x, y_mid)
    let mut best_right: Option<(f32, f32)> = None; // (neighbor_left_x, y_mid)
    let mut best_top: Option<(f32, f32)> = None; // (neighbor_bottom_y, x_mid)
    let mut best_bottom: Option<(f32, f32)> = None; // (neighbor_top_y, x_mid)

    for other in others {
        let op = other.position.unwrap_or(Pos2::ZERO);
        let ox0 = op.x;
        let oy0 = op.y;
        let ox1 = op.x + other.desired_size.x;
        let oy1 = op.y + other.desired_size.y;

        if horiz_overlap(oy0, oy1) && ox1 <= x0 {
            let y_lo = y0.max(oy0);
            let y_hi = y1.min(oy1);
            let y_mid = (y_lo + y_hi) * 0.5;
            best_left = Some(match best_left {
                None => (ox1, y_mid),
                Some((xr, _ym)) if ox1 > xr => (ox1, y_mid),
                Some(keep) => keep,
            });
        }
        if horiz_overlap(oy0, oy1) && ox0 >= x1 {
            let y_lo = y0.max(oy0);
            let y_hi = y1.min(oy1);
            let y_mid = (y_lo + y_hi) * 0.5;
            best_right = Some(match best_right {
                None => (ox0, y_mid),
                Some((xl, _ym)) if ox0 < xl => (ox0, y_mid),
                Some(keep) => keep,
            });
        }
        if vert_overlap(ox0, ox1) && oy1 <= y0 {
            let x_lo = x0.max(ox0);
            let x_hi = x1.min(ox1);
            let x_mid = (x_lo + x_hi) * 0.5;
            best_top = Some(match best_top {
                None => (oy1, x_mid),
                Some((yb, _xm)) if oy1 > yb => (oy1, x_mid),
                Some(keep) => keep,
            });
        }
        if vert_overlap(ox0, ox1) && oy0 >= y1 {
            let x_lo = x0.max(ox0);
            let x_hi = x1.min(ox1);
            let x_mid = (x_lo + x_hi) * 0.5;
            best_bottom = Some(match best_bottom {
                None => (oy0, x_mid),
                Some((yt, _xm)) if oy0 < yt => (oy0, x_mid),
                Some(keep) => keep,
            });
        }
    }

    let y_center = (y0 + y1) * 0.5;
    let x_center = (x0 + x1) * 0.5;

    let left = if let Some((xr, y_mid)) = best_left {
        let px = x0 - xr;
        PaneGapDim {
            px,
            a: Pos2::new(xr, y_mid),
            b: Pos2::new(x0, y_mid),
            is_horizontal: true,
        }
    } else {
        let px = x0;
        PaneGapDim {
            px,
            a: Pos2::new(0.0, y_center),
            b: Pos2::new(x0, y_center),
            is_horizontal: true,
        }
    };

    let right = if let Some((xl, y_mid)) = best_right {
        let px = xl - x1;
        PaneGapDim {
            px,
            a: Pos2::new(x1, y_mid),
            b: Pos2::new(xl, y_mid),
            is_horizontal: true,
        }
    } else {
        let px = workspace_right - x1;
        PaneGapDim {
            px,
            a: Pos2::new(x1, y_center),
            b: Pos2::new(workspace_right, y_center),
            is_horizontal: true,
        }
    };

    let top = if let Some((yb, x_mid)) = best_top {
        let px = y0 - yb;
        PaneGapDim {
            px,
            a: Pos2::new(x_mid, yb),
            b: Pos2::new(x_mid, y0),
            is_horizontal: false,
        }
    } else {
        let px = y0;
        PaneGapDim {
            px,
            a: Pos2::new(x_center, 0.0),
            b: Pos2::new(x_center, y0),
            is_horizontal: false,
        }
    };

    let bottom = if let Some((yt, x_mid)) = best_bottom {
        let px = yt - y1;
        PaneGapDim {
            px,
            a: Pos2::new(x_mid, y1),
            b: Pos2::new(x_mid, yt),
            is_horizontal: false,
        }
    } else {
        let px = workspace_bottom - y1;
        PaneGapDim {
            px,
            a: Pos2::new(x_center, y1),
            b: Pos2::new(x_center, workspace_bottom),
            is_horizontal: false,
        }
    };

    PaneNeighborDimensions {
        left,
        right,
        top,
        bottom,
    }
}

fn gap_matches_used(px: f32, used: &[f32]) -> bool {
    let p = px.round();
    used.iter()
        .any(|&u| (p - u.round()).abs() <= USED_GAP_MATCH_EPS)
}

fn gap_rect_snapshot_three_way(
    left_group: &[TerminalPane],
    mid: &TerminalPane,
    right_group: &[TerminalPane],
) -> Vec<Option<(f32, f32, f32, f32)>> {
    let mut gap_snap: Vec<Option<(f32, f32, f32, f32)>> =
        Vec::with_capacity(left_group.len() + 1 + right_group.len());
    for p in left_group.iter() {
        gap_snap.push(
            p.position
                .map(|op| (op.x, op.y, op.x + p.desired_size.x, op.y + p.desired_size.y)),
        );
    }
    gap_snap.push(mid.position.map(|op| {
        (
            op.x,
            op.y,
            op.x + mid.desired_size.x,
            op.y + mid.desired_size.y,
        )
    }));
    for p in right_group.iter() {
        gap_snap.push(
            p.position
                .map(|op| (op.x, op.y, op.x + p.desired_size.x, op.y + p.desired_size.y)),
        );
    }
    gap_snap
}

/// If `current` is within `snap_dist` of some entry in `used`, returns the closest such target.
/// Compares in **integer pixel** space so a horizontal 20 and vertical measurement ~20.4
/// both agree on 20.
fn closest_used_gap_target(current: f32, used: &[f32], snap_dist: f32) -> Option<f32> {
    let c = current.round();
    let mut best: Option<(f32, f32)> = None; // (abs_diff, target_u_rounded)
    for &u in used {
        let ur = u.round();
        let d = (c - ur).abs();
        if d <= snap_dist && best.is_none_or(|(bd, _)| d < bd - 1e-5) {
            best = Some((d, ur));
        }
    }
    best.map(|(_, ur)| ur)
}

/// All distinct gap sizes seen between other terminals (horizontal *and* vertical pairs),
/// so snapping and highlights can match either orientation.
fn merged_used_gap_targets(used_h: &[f32], used_v: &[f32]) -> Vec<f32> {
    let mut out: Vec<f32> = Vec::with_capacity(used_h.len() + used_v.len());
    for &g in used_h.iter().chain(used_v.iter()) {
        let gr = g.round();
        if gr < 1.0 {
            continue;
        }
        if !out.iter().any(|&x| (x - gr).abs() <= USED_GAP_MATCH_EPS) {
            out.push(gr);
        }
    }
    out
}

fn snap_drag_pos_to_used_neighbor_gaps(
    pos: &mut Pos2,
    _w: f32,
    _h: f32,
    max_y: f32,
    dims: &PaneNeighborDimensions,
    used_h: &[f32],
    used_v: &[f32],
) {
    let used_all = merged_used_gap_targets(used_h, used_v);
    if used_all.is_empty() {
        return;
    }

    let l = dims.left.px.round();
    let r = dims.right.px.round();
    let mut best_dx: Option<(f32, f32)> = None;
    for &u in &used_all {
        let ur = u.round();
        let dx_l = ur - l;
        if dx_l.abs() <= USED_GAP_SNAP_DISTANCE
            && best_dx.is_none_or(|(ab, _)| dx_l.abs() < ab - 1e-5)
        {
            best_dx = Some((dx_l.abs(), dx_l));
        }
        let dx_r = r - ur;
        if dx_r.abs() <= USED_GAP_SNAP_DISTANCE
            && best_dx.is_none_or(|(ab, _)| dx_r.abs() < ab - 1e-5)
        {
            best_dx = Some((dx_r.abs(), dx_r));
        }
    }
    if let Some((_, dx)) = best_dx {
        pos.x = (pos.x + dx).max(0.0);
    }

    let t = dims.top.px.round();
    let b = dims.bottom.px.round();
    let mut best_dy: Option<(f32, f32)> = None;
    for &u in &used_all {
        let ur = u.round();
        let dy_t = ur - t;
        if dy_t.abs() <= USED_GAP_SNAP_DISTANCE
            && best_dy.is_none_or(|(ab, _)| dy_t.abs() < ab - 1e-5)
        {
            best_dy = Some((dy_t.abs(), dy_t));
        }
        let dy_b = b - ur;
        if dy_b.abs() <= USED_GAP_SNAP_DISTANCE
            && best_dy.is_none_or(|(ab, _)| dy_b.abs() < ab - 1e-5)
        {
            best_dy = Some((dy_b.abs(), dy_b));
        }
    }
    if let Some((_, dy)) = best_dy {
        pos.y = (pos.y + dy).clamp(0.0, max_y);
    }
}

fn snap_resize_rect_to_used_neighbor_gaps<'a>(
    new_x: &mut f32,
    new_y: &mut f32,
    new_w: &mut f32,
    new_h: &mut f32,
    left_group: &'a [TerminalPane],
    right_group: &'a [TerminalPane],
    left_dragged: bool,
    right_dragged: bool,
    top_dragged: bool,
    bottom_dragged: bool,
    used_h: &[f32],
    used_v: &[f32],
    init_right: f32,
    init_bottom: f32,
    workspace_right: f32,
    workspace_bottom: f32,
    content_height: f32,
) {
    let others = || left_group.iter().chain(right_group.iter());
    let used_all = merged_used_gap_targets(used_h, used_v);

    let snap_left = left_dragged && !right_dragged;
    let snap_right = right_dragged && !left_dragged;
    let snap_top = top_dragged && !bottom_dragged;
    let snap_bottom = bottom_dragged && !top_dragged;

    if !used_all.is_empty() && (snap_left || snap_right) {
        let dims = pane_neighbor_dimensions(
            Pos2::new(*new_x, *new_y),
            Vec2::new(*new_w, *new_h),
            others(),
            workspace_right,
            workspace_bottom,
        );
        if snap_right {
            if let Some(u) =
                closest_used_gap_target(dims.right.px, &used_all, USED_GAP_SNAP_DISTANCE)
            {
                let xl = dims.right.b.x;
                *new_w = (xl - *new_x - u).max(TERMINAL_MIN_WIDTH);
            }
        }
        if snap_left {
            if let Some(u) =
                closest_used_gap_target(dims.left.px, &used_all, USED_GAP_SNAP_DISTANCE)
            {
                let xr = dims.left.a.x;
                *new_x = (xr + u).clamp(0.0, (init_right - TERMINAL_MIN_WIDTH).max(0.0));
                *new_w = (init_right - *new_x).max(TERMINAL_MIN_WIDTH);
            }
        }
    }

    if !used_all.is_empty() && (snap_top || snap_bottom) {
        let dims = pane_neighbor_dimensions(
            Pos2::new(*new_x, *new_y),
            Vec2::new(*new_w, *new_h),
            others(),
            workspace_right,
            workspace_bottom,
        );
        if snap_bottom {
            if let Some(u) =
                closest_used_gap_target(dims.bottom.px, &used_all, USED_GAP_SNAP_DISTANCE)
            {
                let yt = dims.bottom.b.y;
                let max_h = (content_height - *new_y).max(TERMINAL_MIN_HEIGHT);
                *new_h = (yt - *new_y - u).clamp(TERMINAL_MIN_HEIGHT, max_h);
            }
        }
        if snap_top {
            if let Some(u) = closest_used_gap_target(dims.top.px, &used_all, USED_GAP_SNAP_DISTANCE)
            {
                let yb = dims.top.a.y;
                *new_y = (yb + u).clamp(0.0, (init_bottom - TERMINAL_MIN_HEIGHT).max(0.0));
                *new_h = (init_bottom - *new_y).max(TERMINAL_MIN_HEIGHT);
            }
        }
    }
}

fn push_gap_unique(list: &mut Vec<f32>, g: f32) {
    let g = g.round();
    if g < 1.0 {
        return;
    }
    if !list.iter().any(|&x| (x - g).abs() <= USED_GAP_MATCH_EPS) {
        list.push(g);
    }
}

/// Horizontal and vertical clearances already present between *other* terminals (excludes
/// `exclude_idx` so the dragged pane does not define the reference set).
/// Uses a precomputed snapshot so we can read gaps while the pane loop holds `split_at_mut`.
fn collect_pair_gaps_from_rect_snapshot(
    rects: &[Option<(f32, f32, f32, f32)>],
    exclude_idx: usize,
) -> (Vec<f32>, Vec<f32>) {
    let mut h = Vec::new();
    let mut v = Vec::new();
    let n = rects.len();
    for i in 0..n {
        if i == exclude_idx {
            continue;
        }
        let Some((ax0, ay0, ax1, ay1)) = rects[i] else {
            continue;
        };
        for j in (i + 1)..n {
            if j == exclude_idx {
                continue;
            }
            let Some((bx0, by0, bx1, by1)) = rects[j] else {
                continue;
            };

            let y_overlap = (ay1.min(by1) - ay0.max(by0)).max(0.0);
            if y_overlap > 0.0 {
                if ax1 <= bx0 - 0.05 {
                    push_gap_unique(&mut h, bx0 - ax1);
                } else if bx1 <= ax0 - 0.05 {
                    push_gap_unique(&mut h, ax0 - bx1);
                }
            }

            let x_overlap = (ax1.min(bx1) - ax0.max(bx0)).max(0.0);
            if x_overlap > 0.0 {
                if ay1 <= by0 - 0.05 {
                    push_gap_unique(&mut v, by0 - ay1);
                } else if by1 <= ay0 - 0.05 {
                    push_gap_unique(&mut v, ay0 - by1);
                }
            }
        }
    }
    (h, v)
}

fn format_gap_label_px(px: f32) -> String {
    format!("{}", px.abs().round() as i32)
}

fn paint_gap_dimension_guide(
    painter: &egui::Painter,
    content_origin: Pos2,
    dim: PaneGapDim,
    used_any_orientation: &[f32],
) {
    if dim.px < 0.25 {
        return;
    }
    let matches_used = gap_matches_used(dim.px, used_any_orientation);

    let stroke = if matches_used {
        Stroke::new(1.8, Color32::from_rgb(235, 70, 210))
    } else {
        Stroke::new(1.15, Color32::from_rgb(175, 130, 215))
    };

    let a = content_origin + dim.a.to_vec2();
    let b = content_origin + dim.b.to_vec2();
    painter.line_segment([a, b], stroke);

    let mid = Pos2::new((a.x + b.x) * 0.5, (a.y + b.y) * 0.5);
    let (label_anchor, align) = if dim.is_horizontal {
        (mid + Vec2::new(0.0, -11.0), Align2::CENTER_BOTTOM)
    } else {
        (mid + Vec2::new(12.0, 0.0), Align2::LEFT_CENTER)
    };

    let label = format_gap_label_px(dim.px);
    let font = FontId::monospace(10.0);
    let text_color = Color32::WHITE;
    let galley = painter.layout_no_wrap(label, font.clone(), text_color);
    let pad = 3.0_f32;
    let bubble_size = galley.size() + 2.0 * Vec2::splat(pad);
    let bubble_rect = align.anchor_size(label_anchor, bubble_size);
    let fill = if matches_used {
        Color32::from_rgba_unmultiplied(220, 55, 195, 238)
    } else {
        Color32::from_rgba_unmultiplied(95, 65, 125, 228)
    };
    painter.rect_filled(bubble_rect, 4.0, fill);
    painter.galley(bubble_rect.min + Vec2::splat(pad), galley, text_color);
}

fn paint_terminal_neighbor_gap_guides(
    painter: &egui::Painter,
    content_origin: Pos2,
    dims: &PaneNeighborDimensions,
    used_h: &[f32],
    used_v: &[f32],
) {
    let used_all = merged_used_gap_targets(used_h, used_v);
    const MAX_SPAN: f32 = 900.0;
    for dim in [dims.left, dims.right, dims.top, dims.bottom] {
        if dim.px > MAX_SPAN {
            continue;
        }
        paint_gap_dimension_guide(painter, content_origin, dim, &used_all);
    }
}

/// Leftover pixels after fitting an integer cell grid at [`CELL_W`] × [`CELL_H`].
fn terminal_cell_slack_px(
    pty_w: f32,
    pty_h: f32,
    grid_cols: usize,
    grid_rows: usize,
) -> (f32, f32) {
    let pad_x = pty_w - grid_cols as f32 * CELL_W;
    let pad_y = pty_h - grid_rows as f32 * CELL_H;
    (pad_x.max(0.0), pad_y.max(0.0))
}

fn paint_pane_layout_metrics_overlay(
    painter: &egui::Painter,
    pane_screen_rect: egui::Rect,
    pad_x: f32,
    pad_y: f32,
    text_color: Color32,
    fill: Color32,
) {
    let font = FontId::monospace(10.0);
    let line = format!(
        "grid +{} +{} px\nref gutter {}×{}",
        pad_x.round() as i32,
        pad_y.round() as i32,
        GRID_SPACING.round() as i32,
        STACK_GAP_Y.round() as i32,
    );
    let galley = painter.layout(line, font, text_color, f32::INFINITY);
    let anchor = pane_screen_rect.right_bottom() + Vec2::new(-6.0, -6.0);
    let text_rect = Align2::RIGHT_BOTTOM.anchor_size(anchor, galley.size());
    painter.rect_filled(text_rect.expand(3.0), 3.0, fill);
    painter.galley(text_rect.min, galley, text_color);
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
        line_height: Some(CELL_H),
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

fn row_render_end(grid: &TerminalGrid, row: usize) -> usize {
    if row >= grid.rows {
        return 0;
    }
    grid.virtual_row_render_end(grid.scrollback_len() + row)
}

fn row_render_end_virtual(grid: &TerminalGrid, vrow: usize) -> usize {
    grid.virtual_row_render_end(vrow)
}

#[inline]
fn terminal_char_category(ch: char) -> u8 {
    if ch.is_whitespace() {
        0
    } else if ch.is_alphanumeric() || ch == '_' {
        1
    } else {
        2
    }
}

fn snap_to_leading_cell_v(grid: &TerminalGrid, vrow: usize, mut col: usize) -> usize {
    if col < grid.cols && grid.virtual_cell(vrow, col).wide == WideKind::Trailing && col > 0 {
        col -= 1;
    }
    col
}

fn prev_char_start_col_v(grid: &TerminalGrid, vrow: usize, col: usize) -> Option<usize> {
    if col == 0 {
        return None;
    }
    let mut pc = col - 1;
    if grid.virtual_cell(vrow, pc).wide == WideKind::Trailing {
        pc = pc.checked_sub(1)?;
    }
    Some(pc)
}

fn next_char_start_after_end_col_v(
    grid: &TerminalGrid,
    vrow: usize,
    end_col: usize,
) -> Option<usize> {
    let next = end_col + 1;
    if next >= grid.cols {
        return None;
    }
    Some(snap_to_leading_cell_v(grid, vrow, next))
}

/// Virtual row (`0..total_rows`) for combined scrollback + live buffer.
/// Draw Multerm's VT-cursor follower (baked block + white overlay).
///
/// On the **alternate screen**, many TUIs hide the DEC cursor (`ESC [ ? 25 l`) and paint
/// the caret in-grid (reverse video). The PTY cursor is then often left on a junk row
/// (e.g. bottom), so following it misaligns with the real input. In that case we only
/// render the grid and rely on the app's in-buffer caret.
#[inline]
fn use_synthetic_cursor_overlay(parser: &TerminalParser, grid: &TerminalGrid) -> bool {
    !grid.in_alt || parser.cursor_visible() || parser.app_cursor_keys()
}

fn terminal_word_selection_span_v(
    grid: &TerminalGrid,
    vrow: usize,
    col: usize,
) -> Option<(usize, usize)> {
    if vrow >= grid.total_rows() || grid.cols == 0 {
        return None;
    }
    let col = col.min(grid.cols - 1);
    let anchor = snap_to_leading_cell_v(grid, vrow, col);
    let cat = terminal_char_category(grid.virtual_cell(vrow, anchor).ch);
    let mut start = anchor;
    let mut end = wide_span_end_col_v(grid, vrow, anchor);
    while let Some(ps) = prev_char_start_col_v(grid, vrow, start) {
        if terminal_char_category(grid.virtual_cell(vrow, ps).ch) != cat {
            break;
        }
        start = ps;
    }
    while let Some(ns) = next_char_start_after_end_col_v(grid, vrow, end) {
        if terminal_char_category(grid.virtual_cell(vrow, ns).ch) != cat {
            break;
        }
        end = wide_span_end_col_v(grid, vrow, ns);
    }
    Some((start, end))
}

fn render_terminal_grid(
    ui: &mut egui::Ui,
    pane_id: u64,
    grid: &TerminalGrid,
    p: UiPalette,
    selection: &mut Option<SelectionRange>,
    is_focused_terminal: bool,
    synthetic_cursor_overlay: bool,
    search_highlight: Option<SelectionRange>,
    initial_scroll_to_bottom: &mut bool,
    caret_autoscroll: &mut Option<(usize, usize)>,
) -> Option<(usize, usize)> {
    if !is_focused_terminal {
        *caret_autoscroll = None;
    }
    let font_id = FontId::monospace(12.0);
    let newline_fmt = TextFormat {
        font_id: font_id.clone(),
        line_height: Some(CELL_H),
        color: p.vt_default_fg,
        background: Color32::TRANSPARENT,
        ..Default::default()
    };

    // Bake the block cursor directly into the LayoutJob so it is pixel-perfectly
    // aligned with the glyph grid (avoids painter-overlay Y drift).
    let blink_visible = ui
        .ctx()
        .input(|i| ((i.time / 0.5).floor() as i64).rem_euclid(2) == 0);
    // Blink is tied to focus; whether we draw a VT-cursor follower at all is gated
    // separately (`synthetic_cursor_overlay`) so alternate-screen TUIs that hide the
    // DEC cursor and paint the caret in-grid are not overlaid at a stale PTY position.
    let show_block_cursor = is_focused_terminal && blink_visible && synthetic_cursor_overlay;
    let cursor_row = grid.cursor.row.min(grid.rows.saturating_sub(1));
    let cursor_col = grid.cursor.col.min(grid.cols.saturating_sub(1));
    let cursor_row_v = grid.scrollback_len() + cursor_row;
    let total_rows = grid.total_rows();

    let mut clicked_cell: Option<(usize, usize)> = None;
    // Virtualized: only build a `LayoutJob` for rows in the visible scroll window,
    // while pinning the full scroll extent up-front so resize / undo-redo / auto-fit
    // / layout never see the content geometry jitter as the visible window shifts.
    //
    // We use `show_viewport` (not `show_rows`) so we can explicitly:
    //   * reserve the total content rect via `ui.set_min_size`,
    //   * place the LayoutJob label at the correct Y offset for the visible window,
    //   * keep a single full-extent `ui.interact` rect for click/drag hit-testing
    //     (so clicking blank rows below the visible text still registers, matching
    //     the previous behaviour where the Label itself spanned the full grid).
    egui::ScrollArea::both()
        .id_salt(("term-scroll", pane_id))
        .auto_shrink([false, false])
        .show_viewport(ui, |ui, viewport| {
            let row_h = CELL_H;
            let glyph_w = ui.fonts_mut(|f| f.glyph_width(&font_id, 'W')).max(1.0);
            let content_w = grid.cols as f32 * glyph_w;
            let content_h = (total_rows as f32 * row_h).max(row_h);

            // Pin the full content extent so the scroll area's total scrollable
            // region is stable regardless of which rows we actually render.
            ui.set_min_size(Vec2::new(content_w, content_h));

            // Virtual origin = top-left of virtual content in screen coords.
            // Shifts with the scroll offset; using it lets us address any vrow
            // (including ones outside the visible window) in a scroll-stable way.
            let virtual_origin = ui.max_rect().min;

            // Visible vrow window. +1 padding on `max` to cover a partial row;
            // clamp to `total_rows` to avoid reading past the grid.
            let vrow_start = if total_rows == 0 {
                0
            } else {
                ((viewport.min.y / row_h).floor().max(0.0) as usize)
                    .min(total_rows.saturating_sub(1))
            };
            let vrow_end = (((viewport.max.y / row_h).ceil() as usize) + 1).min(total_rows);

            // Full-content hit-test rect (used for clicks/drags over visible text
            // and the blank space below it). A separate `ui.interact` keeps the
            // response geometry independent of the Label's intrinsic width, which
            // would otherwise fluctuate as the visible rows change.
            let full_rect = egui::Rect::from_min_size(
                virtual_origin,
                Vec2::new(content_w.max(viewport.width()), content_h),
            );
            let response = ui.interact(
                full_rect,
                ui.id().with(("term-grid-sense", pane_id)),
                Sense::click_and_drag(),
            );
            if response.hovered() {
                ui.ctx().set_cursor_icon(CursorIcon::Text);
            }

            if vrow_end > vrow_start {
                let mut job = LayoutJob::default();
                for vrow in vrow_start..vrow_end {
                    let mut trim_end = row_render_end_virtual(grid, vrow);
                    if show_block_cursor && vrow == cursor_row_v && cursor_col < grid.cols {
                        trim_end = trim_end.max(cursor_col + 1);
                    }
                    let mut col = 0;
                    while col < trim_end {
                        let cell = grid.virtual_cell(vrow, col);
                        if cell.wide == WideKind::Trailing {
                            col += 1;
                            continue;
                        }

                        let base_fmt_start =
                            cell_text_format(cell, font_id.clone(), p.term_bg, p.vt_default_fg);
                        let mut fmt = base_fmt_start.clone();
                        let is_selected = selection
                            .map_or(false, |sel| sel.contains(vrow, col, total_rows, grid.cols));
                        let in_search = search_highlight
                            .is_some_and(|r| r.contains(vrow, col, total_rows, grid.cols));
                        if is_selected {
                            let normal_fg = fmt.color;
                            let normal_bg = if fmt.background == Color32::TRANSPARENT {
                                p.term_bg
                            } else {
                                fmt.background
                            };
                            fmt.color = normal_bg;
                            fmt.background = normal_fg;
                        } else if in_search {
                            let normal_fg = fmt.color;
                            fmt.background = Color32::from_rgb(210, 165, 45);
                            fmt.color = normal_fg;
                        }
                        if is_focused_terminal
                            && !show_block_cursor
                            && vrow == cursor_row_v
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
                        if show_block_cursor && vrow == cursor_row_v && col == cursor_col {
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
                            if show_block_cursor && vrow == cursor_row_v && next == cursor_col {
                                break;
                            }
                            let c2 = grid.virtual_cell(vrow, next);
                            if c2.wide == WideKind::Trailing {
                                next += 1;
                                continue;
                            }
                            let fmt2_base =
                                cell_text_format(c2, font_id.clone(), p.term_bg, p.vt_default_fg);
                            let is_sel2 = selection.map_or(false, |sel| {
                                sel.contains(vrow, next, total_rows, grid.cols)
                            });
                            let in_srch2 = search_highlight
                                .is_some_and(|r| r.contains(vrow, next, total_rows, grid.cols));
                            if is_selected != is_sel2
                                || in_search != in_srch2
                                || !formats_match(&base_fmt_start, &fmt2_base)
                            {
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
                    if vrow + 1 < vrow_end {
                        job.append("\n", 0.0, newline_fmt.clone());
                    }
                }

                // Place the Label at the correct Y offset in virtual content space,
                // so it aligns with the interaction/overlay coordinates below.
                let label_rect = egui::Rect::from_min_size(
                    Pos2::new(
                        virtual_origin.x,
                        virtual_origin.y + vrow_start as f32 * row_h,
                    ),
                    Vec2::new(content_w, (vrow_end - vrow_start) as f32 * row_h),
                );
                ui.scope_builder(egui::UiBuilder::new().max_rect(label_rect), |ui| {
                    // `selectable(false)` — otherwise Cmd+A would select every section.
                    ui.add(
                        egui::Label::new(job)
                            .selectable(false)
                            .wrap_mode(egui::TextWrapMode::Extend),
                    );
                });
            }

            let virtual_origin_x = virtual_origin.x;
            let virtual_origin_y = virtual_origin.y;

            let pointer_to_cell = |pointer: Pos2| -> (usize, usize) {
                let lx = pointer.x - virtual_origin_x;
                let ly = pointer.y - virtual_origin_y;
                let row = (ly / row_h)
                    .floor()
                    .max(0.0)
                    .min(total_rows.saturating_sub(1) as f32) as usize;
                let col = (lx / glyph_w)
                    .floor()
                    .max(0.0)
                    .min(grid.cols.saturating_sub(1) as f32) as usize;
                (row, col)
            };

            if response.double_clicked() {
                if let Some(pointer) = response.interact_pointer_pos() {
                    let (vrow, col) = pointer_to_cell(pointer);
                    if let Some((sc, ec)) = terminal_word_selection_span_v(grid, vrow, col) {
                        *selection = Some(SelectionRange {
                            start_row: vrow,
                            start_col: sc,
                            end_row: vrow,
                            end_col: ec,
                            active: true,
                        });
                    }
                }
            } else if response.clicked() {
                if let Some(pointer) = response.interact_pointer_pos() {
                    let (vrow, col) = pointer_to_cell(pointer);
                    clicked_cell = Some((vrow, col));
                }
            }

            if response.drag_started() {
                if let Some(pointer) = response.interact_pointer_pos() {
                    let (vrow, col) = pointer_to_cell(pointer);
                    *selection = Some(SelectionRange {
                        start_row: vrow,
                        start_col: col,
                        end_row: vrow,
                        end_col: col,
                        active: true,
                    });
                }
            } else if response.dragged() {
                if let (Some(pointer), Some(range)) =
                    (response.interact_pointer_pos(), selection.as_mut())
                {
                    let (vrow, col) = pointer_to_cell(pointer);
                    range.end_row = vrow;
                    range.end_col = col;
                    range.active = true;
                }
            }

            if *initial_scroll_to_bottom && search_highlight.is_none() && total_rows > 0 {
                let bottom_rect = egui::Rect::from_min_size(
                    Pos2::new(virtual_origin_x, virtual_origin_y + (total_rows - 1) as f32 * row_h),
                    Vec2::new(content_w.max(viewport.width()), row_h.max(10.0)),
                );
                ui.scroll_to_rect(bottom_rect, Some(egui::Align::BOTTOM));
                *initial_scroll_to_bottom = false;
            }

            if show_block_cursor && total_rows > 0 && grid.cols > 0 {
                let caret_row_v = cursor_row_v.min(total_rows.saturating_sub(1));
                let caret_col = cursor_col.min(grid.cols.saturating_sub(1));
                let caret_x = virtual_origin_x + caret_col as f32 * glyph_w;
                let caret_y =
                    virtual_origin_y + caret_row_v as f32 * row_h + TERMINAL_CELL_OVERLAY_Y_NUDGE;
                let caret_rect = egui::Rect::from_min_size(
                    Pos2::new(caret_x, caret_y),
                    Vec2::new(glyph_w.clamp(6.0, 12.0), row_h.max(10.0)),
                );
                // Only paint when within the visible window; when off-screen the
                // caret auto-scroll below will bring it back into view.
                if caret_row_v >= vrow_start && caret_row_v < vrow_end {
                    ui.painter().rect_filled(caret_rect, 0.0, Color32::WHITE);
                    ui.painter().rect_stroke(
                        caret_rect,
                        0.0,
                        Stroke::new(1.0, Color32::BLACK),
                        egui::StrokeKind::Outside,
                    );
                }
                let caret_key = (caret_row_v, caret_col);
                if search_highlight.is_none() && (*caret_autoscroll != Some(caret_key)) {
                    ui.scroll_to_rect(caret_rect, Some(egui::Align::Center));
                    *caret_autoscroll = Some(caret_key);
                }
            }
            if let Some(range) = search_highlight {
                let ((sr, sc), _) = range.normalized_start_end();
                let sr = sr.min(total_rows.saturating_sub(1));
                let sc = sc.min(grid.cols.saturating_sub(1));
                let match_x = virtual_origin_x + sc as f32 * glyph_w;
                let match_y = virtual_origin_y + sr as f32 * row_h + TERMINAL_CELL_OVERLAY_Y_NUDGE;
                let match_rect = egui::Rect::from_min_size(
                    Pos2::new(match_x, match_y),
                    Vec2::new(glyph_w.max(8.0), row_h.max(10.0)),
                );
                ui.scroll_to_rect(match_rect, Some(egui::Align::Center));
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
        egui::Key::Enter => Some(vec![if shift { b'\n' } else { b'\r' }]),
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

fn selection_delete_bytes(
    grid: &TerminalGrid,
    mut range: SelectionRange,
    key: egui::Key,
) -> Vec<u8> {
    let total = grid.total_rows();
    if !range.active || total == 0 || grid.cols == 0 {
        return key_to_ansi_bytes(key, false, false).unwrap_or_default();
    }

    range.clamp_to_grid(total, grid.cols);
    let ((start_row, start_col), (end_row, end_col)) = range.normalized_start_end();
    if start_row < grid.scrollback_len() {
        return Vec::new();
    }
    let end_grid_row = end_row.saturating_sub(grid.scrollback_len());

    let mut selected_len = 0usize;
    for row in start_row..=end_row {
        let from_c = if row == start_row { start_col } else { 0 };
        let to_c = if row == end_row {
            end_col
        } else {
            grid.cols.saturating_sub(1)
        };
        for col in from_c..=to_c {
            if grid.virtual_cell(row, col).wide != WideKind::Trailing {
                selected_len += 1;
            }
        }
    }

    // Cursor must sit on the last selected row (normal after Cmd+A input block). Align column,
    // then emit one backspace/delete per selected cell (works for multi-line when the shell
    // joins logical lines, e.g. readline-style prompts).
    if selected_len > 0 && grid.cursor.row == end_grid_row {
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

fn header_tabs(ui: &mut egui::Ui, app: &mut MultermUi, p: UiPalette) {
    fn truncate_with_ellipsis(value: &str, max_chars: usize) -> String {
        let len = value.chars().count();
        if len <= max_chars {
            return value.to_owned();
        }
        if max_chars <= 3 {
            return "...".to_owned();
        }
        let keep = max_chars - 3;
        let mut out = value.chars().take(keep).collect::<String>();
        out.push_str("...");
        out
    }

    let mut changed = false;
    let mut close_idx: Option<usize> = None;

    // In sidebar mode the workspace switcher lives in the left side panel, so
    // the horizontal tab strip in this header is suppressed. The +/undo/redo/⚙
    // cluster on the right keeps rendering.
    let sidebar_mode = app.workspace_placement == WorkspacePlacement::Sidebar;
    let n_tabs = app.workspaces.len();
    let tab_h = 36.0_f32;
    let inner_x = 6.0_f32;
    let close_w = 14.0_f32;
    let max_tab_label_chars = 16_usize;
    let fixed_tab_width = 180.0_f32;

    // Keep workspace tabs at a fixed width so labels don't resize the strip.
    let tab_widths: Vec<f32> = (0..n_tabs).map(|_| fixed_tab_width).collect();
    let total_tab_w: f32 = if sidebar_mode {
        0.0
    } else {
        tab_widths.iter().sum()
    };

    // Determine where each tab wants to be in the "after-drop" layout so that
    // non-dragged tabs can smoothly slide to make room.
    let src_idx = app.tab_drag.as_ref().map(|d| d.source_idx);
    let ins_before = app
        .tab_drag
        .as_ref()
        .map(|d| d.insert_before)
        .unwrap_or(n_tabs);

    let target_xs: Vec<f32> = {
        let src = src_idx.unwrap_or(usize::MAX);
        let others: Vec<usize> = (0..n_tabs).filter(|&i| i != src).collect();
        let ib = ins_before.min(others.len());
        let mut order = others;
        if src < n_tabs {
            order.insert(ib, src);
        }
        let mut xs = vec![0.0_f32; n_tabs];
        let mut x = 0.0_f32;
        for &i in &order {
            xs[i] = x;
            x += tab_widths[i];
        }
        xs
    };

    // Smoothly interpolate each tab toward its target x.
    let current_xs: Vec<f32> = (0..n_tabs)
        .map(|i| {
            ui.ctx()
                .animate_value_with_time(egui::Id::new("ttab_x").with(i), target_xs[i], 0.15)
        })
        .collect();

    ui.horizontal(|ui| {
        if sidebar_mode {
            // Sidebar-mode header: show / hide toggle on the far left so it
            // remains reachable even when the sidebar is collapsed.
            let label = if app.workspace_sidebar_visible {
                "Hide sidebar"
            } else {
                "Show sidebar"
            };
            let toggle = egui::Button::new(
                RichText::new("\u{25E7}")
                    .size(14.0)
                    .family(FontFamily::Monospace)
                    .color(p.muted),
            )
            .fill(p.tab_inactive_bg)
            .stroke(Stroke::new(1.0, p.border))
            .min_size(Vec2::new(28.0, 28.0))
            .corner_radius(3.0);
            if ui.add(toggle).on_hover_text(label).clicked() {
                app.workspace_sidebar_visible = !app.workspace_sidebar_visible;
                changed = true;
            }
            // Vertical filler so the toggle row still occupies the same height
            // as a tab strip — keeps the directory bar below from shifting.
            let _ = ui.allocate_exact_size(Vec2::new(0.0, tab_h), Sense::hover());
        } else {
        // Reserve the full tab strip as one block so the + / ⚙ buttons stay to the right.
        let (strip, _) = ui.allocate_exact_size(Vec2::new(total_tab_w, tab_h), Sense::hover());
        let rx = strip.min.x;
        let ry = strip.min.y;

        let painter = ui.painter().clone();

        for idx in 0..n_tabs {
            let active = idx == app.selected_workspace;
            let fill = app.workspace_tab_fill_color(idx, active, p);
            let title = app.workspaces[idx].title.clone();
            let display_title = truncate_with_ellipsis(&title, max_tab_label_chars);
            // Live terminal count for this workspace, shown next to the close
            // button. Hidden at zero so empty workspaces stay uncluttered.
            let badge = app
                .workspace_runtime
                .get(idx)
                .map(|rt| rt.terminals.len())
                .filter(|&n| n > 0);
            let tc = if active
                && app.workspaces[idx].color_rgba.is_none()
                && app.color_picker_target_idx != Some(idx)
            {
                p.tab_label_active
            } else {
                tab_auto_text_color(fill)
            };
            let is_editing = app.editing_workspace_idx == Some(idx);
            let is_drag_src = src_idx == Some(idx);

            let tx = rx + current_xs[idx];
            let tw = tab_widths[idx];
            let tab_rect = egui::Rect::from_min_size(Pos2::new(tx, ry), Vec2::new(tw, tab_h));
            let close_rect = egui::Rect::from_min_size(
                Pos2::new(tx + tw - inner_x - close_w, ry + (tab_h - 14.0) * 0.5),
                Vec2::new(close_w, 14.0),
            );
            let label_rect = egui::Rect::from_min_max(
                tab_rect.min,
                Pos2::new(close_rect.min.x - 2.0, tab_rect.max.y),
            );

            if is_drag_src {
                // Render the ghost at the cursor position.
                let gx = app.tab_drag.as_ref().map(|d| d.ghost_x).unwrap_or(tx);
                let ghost = egui::Rect::from_min_size(Pos2::new(gx, ry), Vec2::new(tw, tab_h));
                let ghost_fill = Color32::from_rgba_unmultiplied(fill.r(), fill.g(), fill.b(), 215);
                painter.rect(
                    ghost,
                    3.0,
                    ghost_fill,
                    Stroke::new(1.5, p.border),
                    egui::StrokeKind::Inside,
                );
                painter.text(
                    Pos2::new(gx + inner_x + 2.0, ry + tab_h * 0.5),
                    Align2::LEFT_CENTER,
                    format!(">_  {}", display_title),
                    FontId::monospace(12.0),
                    Color32::from_rgba_unmultiplied(tc.r(), tc.g(), tc.b(), 215),
                );
                // Dashed placeholder where the tab originated.
                painter.rect_stroke(
                    tab_rect,
                    2.0,
                    Stroke::new(1.0, Color32::from_rgba_unmultiplied(160, 160, 160, 55)),
                    egui::StrokeKind::Inside,
                );

                // Receive drag-delta / drag-released via the same interaction ID.
                let drag_resp = ui.interact(
                    tab_rect,
                    egui::Id::new("ttab_label").with(idx),
                    Sense::drag(),
                );
                if drag_resp.dragged() {
                    if let Some(ref mut d) = app.tab_drag {
                        d.ghost_x = (d.ghost_x + drag_resp.drag_delta().x)
                            .max(rx - tw * 0.5)
                            .min(rx + total_tab_w - tw * 0.5);
                        let ghost_center = d.ghost_x + tw * 0.5;
                        let mut ib = 0;
                        let mut ax = rx;
                        for j in 0..n_tabs {
                            if j == idx {
                                continue;
                            }
                            if ghost_center > ax + tab_widths[j] * 0.5 {
                                ib += 1;
                            }
                            ax += tab_widths[j];
                        }
                        d.insert_before = ib;
                    }
                    ui.ctx().request_repaint();
                }
                if drag_resp.drag_stopped() {
                    if let Some(d) = app.tab_drag.take() {
                        let from = d.source_idx;
                        let to = d.insert_before.min(n_tabs.saturating_sub(1));
                        if from != to {
                            let tab = app.workspaces.remove(from);
                            app.workspaces.insert(to, tab);
                            let rt = app.workspace_runtime.remove(from);
                            app.workspace_runtime.insert(to, rt);
                            let hist = app.workspace_edit_histories.remove(from);
                            app.workspace_edit_histories.insert(to, hist);
                            let sel = app.selected_workspace;
                            app.selected_workspace = if sel == from {
                                to
                            } else if from < to && sel > from && sel <= to {
                                sel - 1
                            } else if from > to && sel >= to && sel < from {
                                sel + 1
                            } else {
                                sel
                            };
                            if let Some(ei) = app.editing_workspace_idx {
                                app.editing_workspace_idx = Some(if ei == from {
                                    to
                                } else if from < to && ei > from && ei <= to {
                                    ei - 1
                                } else if from > to && ei >= to && ei < from {
                                    ei + 1
                                } else {
                                    ei
                                });
                            }
                            changed = true;
                        }
                    }
                }
                continue;
            }

            // --- Normal (non-dragged) tab ---

            // Background + border.
            painter.rect(
                tab_rect,
                5.0,
                fill,
                Stroke::new(1.0, p.border),
                egui::StrokeKind::Inside,
            );
            if active {
                let indicator_color = p
                    .tab_active_indicator
                    .unwrap_or_else(|| lighten_toward_white(fill, 0.52, 255));
                let glow_color = p
                    .tab_active_indicator
                    .map(|c| color_with_alpha(c, 180))
                    .unwrap_or_else(|| lighten_toward_white(fill, 0.72, 205));
                let y = tab_rect.max.y - 1.5;
                painter.line_segment(
                    [
                        Pos2::new(tab_rect.min.x + 1.5, y),
                        Pos2::new(tab_rect.max.x - 1.5, y),
                    ],
                    Stroke::new(3.4, indicator_color),
                );
                painter.line_segment(
                    [
                        Pos2::new(tab_rect.min.x + 2.5, y - 1.2),
                        Pos2::new(tab_rect.max.x - 2.5, y - 1.2),
                    ],
                    Stroke::new(2.0, glow_color),
                );
            }

            if is_editing {
                // Inline rename editor.
                let icon_color = tc;
                ui.scope_builder(egui::UiBuilder::new().max_rect(tab_rect), |ui| {
                    ui.with_layout(egui::Layout::left_to_right(egui::Align::Center), |ui| {
                        ui.add_space(inner_x);
                        ui.label(
                            RichText::new(">_")
                                .size(12.0)
                                .family(FontFamily::Monospace)
                                .color(icon_color),
                        );
                        ui.add_space(3.0);
                        let mut output =
                            egui::TextEdit::singleline(&mut app.editing_workspace_input)
                                .id(egui::Id::new("workspace_tab_rename").with(idx))
                                .desired_width(tw - inner_x * 2.0 - 30.0)
                                .font(egui::TextStyle::Monospace)
                                .show(ui);
                        let resp = &output.response;
                        resp.request_focus();
                        if app.select_all_workspace_input_on_focus {
                            output.state.cursor.set_char_range(Some(
                                egui::text::CCursorRange::two(
                                    egui::text::CCursor::default(),
                                    egui::text::CCursor::new(
                                        app.editing_workspace_input.chars().count(),
                                    ),
                                ),
                            ));
                            output.state.store(ui.ctx(), output.response.id);
                            app.select_all_workspace_input_on_focus = false;
                        }
                        let enter = ui.input(|i| i.key_pressed(egui::Key::Enter));
                        let esc = ui.input(|i| i.key_pressed(egui::Key::Escape));
                        if resp.lost_focus() || enter {
                            let t = app.editing_workspace_input.trim().to_string();
                            if !t.is_empty() {
                                app.workspaces[idx].title = t;
                                app.next_workspace_index =
                                    compute_next_workspace_index(&app.workspaces);
                                changed = true;
                            }
                            app.editing_workspace_idx = None;
                            app.editing_workspace_input.clear();
                            app.select_all_workspace_input_on_focus = false;
                        } else if esc {
                            app.editing_workspace_idx = None;
                            app.editing_workspace_input.clear();
                            app.select_all_workspace_input_on_focus = false;
                        }
                    });
                });
                continue;
            }

            // Label text.
            painter.text(
                Pos2::new(tx + inner_x + 2.0, ry + tab_h * 0.5),
                Align2::LEFT_CENTER,
                format!(">_  {}", display_title),
                FontId::monospace(12.0),
                tc,
            );

            // Badge (optional counter).
            if let Some(count) = badge {
                painter.text(
                    Pos2::new(close_rect.min.x - 18.0, ry + tab_h * 0.5),
                    Align2::LEFT_CENTER,
                    count.to_string(),
                    FontId::monospace(11.0),
                    p.muted,
                );
            }

            // Close button with hover highlight.
            let close_resp = ui.interact(
                close_rect,
                egui::Id::new("ttab_close").with(idx),
                Sense::click(),
            );
            if close_resp.hovered() || close_resp.is_pointer_button_down_on() {
                let bg = if close_resp.is_pointer_button_down_on() {
                    p.tab_close_active_bg
                } else {
                    p.tab_close_hover_bg
                };
                let fg = if close_resp.is_pointer_button_down_on() {
                    Color32::WHITE
                } else {
                    p.tab_close_hover_text
                };
                painter.rect_filled(close_resp.rect.expand(2.0), 3.0, bg);
                painter.text(
                    close_resp.rect.center(),
                    Align2::CENTER_CENTER,
                    "x",
                    FontId::monospace(11.0),
                    fg,
                );
            } else {
                painter.text(
                    close_resp.rect.center(),
                    Align2::CENTER_CENTER,
                    "x",
                    FontId::monospace(11.0),
                    p.tab_close,
                );
            }
            if close_resp.clicked() {
                close_idx = Some(idx);
            }

            // Label area: click to select, right-click for context menu, drag to reorder.
            let label_resp = ui.interact(
                label_rect,
                egui::Id::new("ttab_label").with(idx),
                Sense::click_and_drag(),
            );
            let label_resp = if display_title != title {
                label_resp.on_hover_text(title.clone())
            } else {
                label_resp
            };
            egui::Popup::context_menu(&label_resp)
                .close_behavior(egui::PopupCloseBehavior::CloseOnClickOutside)
                .show(|ui| {
                    workspace_tab_context_menu(ui, app, idx, &mut changed, p);
                });
            if label_resp.double_clicked() {
                app.selected_workspace = idx;
                begin_workspace_tab_rename(app, idx);
                egui::Popup::close_all(ui.ctx());
                changed = true;
            } else if label_resp.clicked() {
                app.selected_workspace = idx;
                egui::Popup::close_all(ui.ctx());
                changed = true;
            }
            if label_resp.drag_started() && n_tabs > 1 && app.tab_drag.is_none() {
                app.tab_drag = Some(TabDragState {
                    source_idx: idx,
                    ghost_x: tx,
                    insert_before: idx, // initial = "no change"
                });
            }
        }

        // Animated drop indicator line between tabs.
        if let Some(ref d) = app.tab_drag {
            let src = d.source_idx;
            let others: Vec<usize> = (0..n_tabs).filter(|&i| i != src).collect();
            let ib_c = d.insert_before.min(others.len());
            let ind_x = rx + others[..ib_c].iter().map(|&i| tab_widths[i]).sum::<f32>();
            let anim_x =
                ui.ctx()
                    .animate_value_with_time(egui::Id::new("ttab_drop_x"), ind_x, 0.10);
            painter.line_segment(
                [
                    Pos2::new(anim_x, ry + 2.0),
                    Pos2::new(anim_x, ry + tab_h - 2.0),
                ],
                Stroke::new(2.5, p.tab_active_bg),
            );
        }
        } // !sidebar_mode

        // "+" new workspace button — only rendered in tabbed mode. In sidebar
        // mode the sidebar's own header "+" creates new workspaces.
        if !sidebar_mode {
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
                app.add_workspace_inherit_selected();
                changed = true;
            }
        }

        // Keep undo/redo/settings grouped at the right side of the header.
        let right_cluster_w = 34.0_f32 + 28.0_f32 + 28.0_f32 + 8.0_f32;
        let slack = (ui.available_width() - right_cluster_w).max(0.0);
        ui.add_space(slack);
        let undo_close_btn = ui
            .add_enabled(
                app.can_undo_workspace_edit(),
                egui::Button::new(
                    RichText::new("↶")
                        .size(14.0)
                        .family(FontFamily::Monospace)
                        .color(p.muted),
                )
                .fill(p.tab_inactive_bg)
                .stroke(Stroke::new(1.0, p.border))
                .min_size(Vec2::new(28.0, 28.0))
                .corner_radius(3.0),
            )
            .on_hover_text("Undo (Cmd/Ctrl+Alt+Z)");
        if undo_close_btn.clicked() {
            changed |= app.undo_workspace_edit();
        }

        let redo_close_btn = ui
            .add_enabled(
                app.can_redo_workspace_edit(),
                egui::Button::new(
                    RichText::new("↷")
                        .size(14.0)
                        .family(FontFamily::Monospace)
                        .color(p.muted),
                )
                .fill(p.tab_inactive_bg)
                .stroke(Stroke::new(1.0, p.border))
                .min_size(Vec2::new(28.0, 28.0))
                .corner_radius(3.0),
            )
            .on_hover_text("Redo (Cmd/Ctrl+Alt+Shift+Z)");
        if redo_close_btn.clicked() {
            changed |= app.redo_workspace_edit();
        }

        // ⚙ settings button.
        let _ = egui::containers::menu::MenuButton::from_button(egui::Button::new(
            RichText::new("⚙").size(18.0).family(FontFamily::Monospace),
        ))
        .config(
            egui::containers::menu::MenuConfig::new()
                .close_behavior(egui::containers::PopupCloseBehavior::CloseOnClickOutside),
        )
        .ui(ui, |ui| {
            settings_menu(ui, app, &mut changed);
        });
    });

    // Handle close workspace (done outside the closure to avoid borrow conflicts).
    if let Some(idx) = close_idx {
        changed |= app.close_workspace_at(idx);
    }

    if changed {
        app.next_workspace_index = compute_next_workspace_index(&app.workspaces);
        save_workspace_state(app);
    }
}

/// Replace the user's home directory prefix with `~` for compact display in
/// sidebar rows. Returns the original string unchanged when no match.
fn shorten_home_path(path: &str) -> String {
    if let Some(home) = std::env::var_os("HOME") {
        let home = home.to_string_lossy().to_string();
        if !home.is_empty() {
            if path == home {
                return "~".to_string();
            }
            if let Some(rest) = path.strip_prefix(&format!("{home}/")) {
                return format!("~/{rest}");
            }
        }
    }
    path.to_string()
}

/// Renders a single sidebar row representing one workspace tab.
fn workspace_sidebar_row(
    ui: &mut egui::Ui,
    app: &mut MultermUi,
    idx: usize,
    p: UiPalette,
    changed: &mut bool,
    close_idx: &mut Option<usize>,
) {
    let active = idx == app.selected_workspace;
    let tab = &app.workspaces[idx];
    let title = tab.title.clone();
    let cwd_display = shorten_home_path(&tab.working_dir);
    let row_h = 52.0_f32;
    let icon_diameter = 28.0_f32;
    let inner_pad = 10.0_f32;
    let close_w = 18.0_f32;

    // Match tabbed-mode coloring: workspace color shows only when the row is
    // focused (active). Inactive rows use the neutral inactive fill regardless
    // of any custom workspace color.
    let bg = app.workspace_tab_fill_color(idx, active, p);

    let (rect, response) =
        ui.allocate_exact_size(Vec2::new(ui.available_width(), row_h), Sense::click());

    let painter = ui.painter().clone();

    // Theme accent: cyberpunk uses its terminal-glow cyan; dark/light use the
    // tab-active-indicator (amber / terracotta). Falls back to the panel border
    // color if a theme defines neither.
    let cyber_glow = p.terminal_glow;
    let accent = cyber_glow
        .or(p.tab_active_indicator)
        .unwrap_or(p.border);

    // Cyberpunk: soft cyan halo behind the active row. Dark/Light skip the halo
    // (it would look out of place outside the neon aesthetic).
    if let Some(glow) = cyber_glow {
        if active {
            for (expand, alpha) in [(8.0_f32, 22u8), (4.0_f32, 50u8)] {
                painter.rect_filled(
                    rect.expand(expand),
                    8.0 + expand,
                    Color32::from_rgba_unmultiplied(glow.r(), glow.g(), glow.b(), alpha),
                );
            }
        }
    }

    // Row outline: brighter when active, faint on hover, none otherwise.
    // Cyberpunk uses a stronger active outline because the halo demands it;
    // the dark/light themes use the same accent at a softer weight.
    let row_stroke = if active {
        let alpha = if cyber_glow.is_some() { 200 } else { 150 };
        Stroke::new(
            1.0,
            Color32::from_rgba_unmultiplied(accent.r(), accent.g(), accent.b(), alpha),
        )
    } else if response.hovered() {
        Stroke::new(
            1.0,
            Color32::from_rgba_unmultiplied(accent.r(), accent.g(), accent.b(), 70),
        )
    } else {
        Stroke::NONE
    };
    painter.rect(rect, 6.0, bg, row_stroke, egui::StrokeKind::Inside);

    // Active-row left-edge accent bar — anchors the selection visually. Skipped
    // for cyberpunk because the outer halo already saturates that role.
    if active && cyber_glow.is_none() {
        let bar_w = 3.0_f32;
        let bar_inset = 4.0_f32;
        let bar_rect = egui::Rect::from_min_max(
            Pos2::new(rect.min.x + 2.0, rect.min.y + bar_inset),
            Pos2::new(rect.min.x + 2.0 + bar_w, rect.max.y - bar_inset),
        );
        painter.rect_filled(bar_rect, 1.5, accent);
    }

    // Round icon on the left containing the generic terminal glyph. Disc fill
    // is tinted with the theme accent so the same code reads correctly on both
    // dark and light backgrounds. Cyberpunk gets a brighter ring; dark/light
    // get a subtler one.
    let icon_center = Pos2::new(rect.min.x + inner_pad + icon_diameter * 0.5, rect.center().y);
    let icon_rect = egui::Rect::from_center_size(icon_center, Vec2::splat(icon_diameter));
    let icon_fill_alpha = if active { 60 } else { 30 };
    let icon_ring_alpha = match (cyber_glow.is_some(), active) {
        (true, true) => 210,
        (true, false) => 130,
        (false, true) => 170,
        (false, false) => 90,
    };
    painter.circle_filled(
        icon_center,
        icon_diameter * 0.5,
        Color32::from_rgba_unmultiplied(accent.r(), accent.g(), accent.b(), icon_fill_alpha),
    );
    painter.circle_stroke(
        icon_center,
        icon_diameter * 0.5 - 0.5,
        Stroke::new(
            1.0,
            Color32::from_rgba_unmultiplied(accent.r(), accent.g(), accent.b(), icon_ring_alpha),
        ),
    );
    let fg = if active { p.tab_label_active } else { p.text };
    // Glyph color: cyberpunk wants the cyan accent; dark/light want plain
    // foreground so the icon doesn't muddy against tinted backgrounds.
    let icon_fg = if cyber_glow.is_some() { accent } else { fg };
    // The sidebar represents terminal workspaces, so always paint the generic
    // terminal glyph here regardless of which agent kind is currently focused.
    paint_terminal_agent_icon(&painter, icon_rect.shrink(4.0), TerminalAgentKind::Terminal, icon_fg);

    // Close button rect — right-aligned. The row click handler ignores clicks
    // that land inside this rect so removing a workspace doesn't also select
    // it on the way down.
    let close_rect = egui::Rect::from_min_size(
        Pos2::new(rect.max.x - inner_pad - close_w, rect.center().y - close_w * 0.5),
        Vec2::splat(close_w),
    );
    // Live terminal count for this workspace — drawn just left of the close
    // button. Skipped at zero so the row stays clean for fresh workspaces.
    let terminal_count = app
        .workspace_runtime
        .get(idx)
        .map(|rt| rt.terminals.len())
        .filter(|&n| n > 0);

    // Title + cwd path stacked to the right of the icon. Right edge stops
    // before the count badge / close button so long titles don't run under
    // them.
    let text_x = rect.min.x + inner_pad + icon_diameter + 10.0;
    let count_reservation = if terminal_count.is_some() { 22.0 } else { 0.0 };
    let text_right_limit = close_rect.min.x - 6.0 - count_reservation;
    let title_galley = painter.layout_no_wrap(title.clone(), FontId::proportional(13.0), fg);
    let title_pos = Pos2::new(text_x, rect.center().y - 8.0 - title_galley.size().y * 0.5);
    let title_clip = egui::Rect::from_min_max(
        Pos2::new(text_x, rect.min.y),
        Pos2::new(text_right_limit, rect.max.y),
    );
    painter
        .with_clip_rect(title_clip)
        .galley(title_pos, title_galley, fg);
    let cwd_galley = painter.layout_no_wrap(cwd_display, FontId::monospace(11.0), p.muted);
    let cwd_pos = Pos2::new(text_x, rect.center().y + 9.0 - cwd_galley.size().y * 0.5);
    painter
        .with_clip_rect(title_clip)
        .galley(cwd_pos, cwd_galley, p.muted);

    if let Some(count) = terminal_count {
        // Cyberpunk wants the neon glow; dark/light themes use a brighter
        // accent on the active row and stay muted on inactive rows so the
        // badge doesn't compete with the title.
        let count_fg = if cyber_glow.is_some() {
            Color32::from_rgba_unmultiplied(accent.r(), accent.g(), accent.b(), 230)
        } else if active {
            Color32::from_rgba_unmultiplied(accent.r(), accent.g(), accent.b(), 220)
        } else {
            p.muted
        };
        painter.text(
            Pos2::new(close_rect.min.x - 6.0, rect.center().y),
            Align2::RIGHT_CENTER,
            count.to_string(),
            FontId::monospace(11.0),
            count_fg,
        );
    }

    // Close button — always visible, slightly higher contrast on hover.
    let close_resp = ui.interact(
        close_rect,
        egui::Id::new("sidebar_close").with(idx),
        Sense::click(),
    );
    let close_hover = close_resp.hovered() || close_resp.is_pointer_button_down_on();
    let close_fg = if close_hover {
        p.tab_close_hover_text
    } else {
        p.tab_close
    };
    if close_hover {
        let bg = if close_resp.is_pointer_button_down_on() {
            p.tab_close_active_bg
        } else {
            p.tab_close_hover_bg
        };
        painter.rect_filled(close_rect, 3.0, bg);
    }
    painter.text(
        close_rect.center(),
        Align2::CENTER_CENTER,
        "x",
        FontId::monospace(11.0),
        close_fg,
    );
    if close_resp.clicked() {
        *close_idx = Some(idx);
    }

    // Suppress row-level click when the pointer is over the close button so
    // closing a row doesn't first select it.
    let pointer_on_close = ui
        .ctx()
        .input(|i| i.pointer.interact_pos())
        .is_some_and(|pos| close_rect.contains(pos));
    if response.clicked() && !pointer_on_close {
        app.selected_workspace = idx;
        *changed = true;
    }
    if response.double_clicked() && !pointer_on_close {
        app.selected_workspace = idx;
        begin_workspace_tab_rename(app, idx);
        *changed = true;
    }

    // Right-click → reuse existing tab context menu (rename/colors/etc.).
    response.context_menu(|ui| {
        workspace_tab_context_menu(ui, app, idx, changed, p);
        ui.separator();
        if ui.button("Close workspace").clicked() {
            *close_idx = Some(idx);
            ui.close();
        }
    });
}

/// Renders the workspace switcher as a left-side panel. Always called when
/// the placement is Sidebar — visibility is driven through `show_animated`
/// so hide/show interpolates smoothly instead of snapping.
fn render_workspace_sidebar(ctx: &egui::Context, app: &mut MultermUi, p: UiPalette) {
    let mut changed = false;
    let mut close_idx: Option<usize> = None;
    let target_width = app
        .workspace_sidebar_width
        .clamp(WORKSPACE_SIDEBAR_MIN_WIDTH, WORKSPACE_SIDEBAR_MAX_WIDTH);

    // Captured before the panel renders so a row click that switches workspaces
    // still unfocuses the terminal of the workspace that *was* active at click
    // time, rather than the newly selected one.
    let prev_active_ws = app.selected_workspace;

    // No frame stroke on the right edge — it would paint over the SidePanel's
    // resize grip and make it visually unclear that the panel is draggable.
    let resp = egui::SidePanel::left("workspace_sidebar")
        .resizable(true)
        .default_width(target_width)
        .min_width(WORKSPACE_SIDEBAR_MIN_WIDTH)
        .max_width(WORKSPACE_SIDEBAR_MAX_WIDTH)
        .frame(
            egui::Frame::default()
                .fill(p.header_strip)
                .inner_margin(Margin::same(0)),
        )
        .show_animated(ctx, app.workspace_sidebar_visible, |ui| {
            ui.add_space(8.0);
            // Cyberpunk theme borrows the terminal-glow cyan for accents.
            let cyber_glow = p.terminal_glow;
            // ── Search + filter + new (search/filter visual only) ──────────
            egui::Frame::default()
                .inner_margin(Margin::symmetric(8, 0))
                .show(ui, |ui| {
                    ui.horizontal(|ui| {
                        let avail = ui.available_width();
                        let trailing_w = 28.0 + 28.0 + 8.0;
                        let search_w = (avail - trailing_w).max(60.0);
                        let btn_stroke_color = match cyber_glow {
                            Some(glow) => Color32::from_rgba_unmultiplied(
                                glow.r(), glow.g(), glow.b(), 130,
                            ),
                            None => p.border,
                        };
                        let btn_text_color = match cyber_glow {
                            Some(glow) => Color32::from_rgba_unmultiplied(
                                glow.r(), glow.g(), glow.b(), 230,
                            ),
                            None => p.muted,
                        };
                        let search_resp = ui.add_sized(
                            Vec2::new(search_w, 28.0),
                            egui::TextEdit::singleline(&mut app.workspace_sidebar_search)
                                .hint_text("\u{1F50D}  Search tabs…"),
                        );
                        // Visual-only for now — never persists or filters.
                        let _ = search_resp;
                        let _ = ui.add_sized(
                            Vec2::new(28.0, 28.0),
                            egui::Button::new(
                                RichText::new("\u{2261}")
                                    .size(13.0)
                                    .family(FontFamily::Monospace)
                                    .color(btn_text_color),
                            )
                            .fill(Color32::TRANSPARENT)
                            .stroke(Stroke::new(1.0, btn_stroke_color))
                            .corner_radius(4.0),
                        );
                        let plus_resp = ui.add_sized(
                            Vec2::new(28.0, 28.0),
                            egui::Button::new(
                                RichText::new("+")
                                    .size(14.0)
                                    .family(FontFamily::Monospace)
                                    .color(btn_text_color),
                            )
                            .fill(Color32::TRANSPARENT)
                            .stroke(Stroke::new(1.0, btn_stroke_color))
                            .corner_radius(4.0),
                        );
                        if plus_resp.on_hover_text("New workspace").clicked() {
                            app.add_workspace_inherit_selected();
                            changed = true;
                        }
                    });
                });

            ui.add_space(6.0);
            if let Some(glow) = cyber_glow {
                // Glowing cyan divider — matches the in-pane cyberpunk separator.
                let sep_h = 1.5_f32;
                let (sep_rect, _) = ui.allocate_exact_size(
                    Vec2::new(ui.available_width(), sep_h + 4.0),
                    Sense::hover(),
                );
                let y = sep_rect.center().y;
                ui.painter().line_segment(
                    [Pos2::new(sep_rect.min.x, y), Pos2::new(sep_rect.max.x, y)],
                    Stroke::new(
                        sep_h,
                        Color32::from_rgba_unmultiplied(glow.r(), glow.g(), glow.b(), 80),
                    ),
                );
                ui.painter().line_segment(
                    [
                        Pos2::new(sep_rect.min.x + 2.0, y - 1.5),
                        Pos2::new(sep_rect.max.x - 2.0, y - 1.5),
                    ],
                    Stroke::new(
                        1.0,
                        Color32::from_rgba_unmultiplied(glow.r(), glow.g(), glow.b(), 35),
                    ),
                );
            } else {
                ui.separator();
            }
            ui.add_space(6.0);

            // ── Tab list ──────────────────────────────────────────────────
            egui::ScrollArea::vertical()
                .auto_shrink([false, false])
                .show(ui, |ui| {
                    egui::Frame::default()
                        .inner_margin(Margin::symmetric(8, 0))
                        .show(ui, |ui| {
                            let n = app.workspaces.len();
                            for idx in 0..n {
                                workspace_sidebar_row(
                                    ui,
                                    app,
                                    idx,
                                    p,
                                    &mut changed,
                                    &mut close_idx,
                                );
                                ui.add_space(6.0);
                            }
                        });
                });
        });

    // Any click landing inside the sidebar removes focus from whichever
    // terminal was focused at the moment of the click — so further keystrokes
    // don't get routed to the terminal once the user is interacting with the
    // workspace switcher.
    if let Some(inner) = resp.as_ref() {
        let panel_rect = inner.response.rect;
        let clicked_in_panel = ctx.input(|i| {
            i.pointer.any_click()
                && i.pointer
                    .interact_pos()
                    .is_some_and(|pos| panel_rect.contains(pos))
        });
        if clicked_in_panel {
            if let Some(rt) = app.workspace_runtime.get_mut(prev_active_ws) {
                rt.active_terminal = None;
                rt.active_terminal_rect = None;
            }
        }
    }

    // Persist the user's resized width — but only when the panel is fully
    // expanded. While `show_animated` is animating, the rendered rect is
    // narrower than the user's intended width and we'd otherwise overwrite
    // the saved value with the animation's intermediate sample.
    if app.workspace_sidebar_visible {
        if let Some(inner) = resp.as_ref() {
            let rendered = inner.response.rect.width();
            // Only treat differences > 4 px as a real user drag, so the
            // final frame of an expand animation (which lands right on
            // `target_width`) doesn't trigger a redundant save.
            if rendered >= WORKSPACE_SIDEBAR_MIN_WIDTH
                && (rendered - app.workspace_sidebar_width).abs() > 4.0
            {
                app.workspace_sidebar_width = rendered
                    .clamp(WORKSPACE_SIDEBAR_MIN_WIDTH, WORKSPACE_SIDEBAR_MAX_WIDTH);
                changed = true;
            }
        }
    }

    if let Some(idx) = close_idx {
        changed |= app.close_workspace_at(idx);
    }

    if changed {
        app.next_workspace_index = compute_next_workspace_index(&app.workspaces);
        save_workspace_state(app);
    }
}

fn settings_menu(ui: &mut egui::Ui, app: &mut MultermUi, changed: &mut bool) {
    ui.set_width(200.0);
    egui::ScrollArea::vertical()
        .max_height(720.0)
        .auto_shrink([false, true])
        .show(ui, |ui| {
            egui::Frame::NONE
                .inner_margin(Margin::symmetric(10, 8))
                .show(ui, |ui| {

            // ── Theme ─────────────────────────────────────────────────────────
            // "Theme" = color scheme (Dark / Light / Cyberpunk)
            ui.label("Theme");
            ui.horizontal(|ui| {
                *changed |= ui
                    .selectable_value(&mut app.ui_theme, UiTheme::Dark, "Dark")
                    .clicked();
                *changed |= ui
                    .selectable_value(&mut app.ui_theme, UiTheme::Light, "Light")
                    .clicked();
                *changed |= ui
                    .selectable_value(&mut app.ui_theme, UiTheme::Cyberpunk, "Cyberpunk")
                    .clicked();
            });

            // ── Workspace placement ───────────────────────────────────────────
            // Switches workspace navigation between the top tab strip and a
            // left-side vertical sidebar.
            ui.separator();
            ui.label("Workspace placement");
            ui.horizontal(|ui| {
                if ui
                    .selectable_value(
                        &mut app.workspace_placement,
                        WorkspacePlacement::Tabbed,
                        "Tabbed",
                    )
                    .clicked()
                {
                    *changed = true;
                }
                if ui
                    .selectable_value(
                        &mut app.workspace_placement,
                        WorkspacePlacement::Sidebar,
                        "Sidebar",
                    )
                    .clicked()
                {
                    // Reopen the sidebar when switching into sidebar mode so the
                    // user immediately sees the panel they just enabled.
                    app.workspace_sidebar_visible = true;
                    *changed = true;
                }
            });

            // ── Style ─────────────────────────────────────────────────────────
            // "Style" = visual treatment applied on top of the theme (Normal / Glass)
            ui.separator();
            ui.label("Style");
            ui.horizontal(|ui| {
                *changed |= ui
                    .selectable_value(&mut app.ui_style, UiStyle::Normal, "Normal")
                    .clicked();
                *changed |= ui
                    .selectable_value(&mut app.ui_style, UiStyle::Glass, "Glass")
                    .clicked();
            });

            // ── Performance ───────────────────────────────────────────────────
            ui.separator();
            *changed |= ui
                .checkbox(&mut app.performance_mode, "Performance mode")
                .on_hover_text("Disables animations; keeps all visual styles intact")
                .changed();

            // ── Cyberpunk-specific settings ───────────────────────────────────
            if app.ui_theme == UiTheme::Cyberpunk {
                ui.separator();
                ui.horizontal(|ui| {
                    ui.label("Cyberpunk");
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        if ui.small_button("Reset").clicked() {
                            app.cyberpunk_settings = CyberpunkSettings::default();
                            *changed = true;
                        }
                    });
                });

                let cs = &mut app.cyberpunk_settings;

                egui::CollapsingHeader::new("Light")
                    .default_open(true)
                    .show(ui, |ui| {
                        *changed |= ui.checkbox(&mut cs.show_light, "Show light").changed();
                        ui.add_enabled_ui(cs.show_light, |ui| {
                            *changed |= ui.checkbox(&mut cs.follows_mouse, "Follow mouse").changed();
                            ui.add_enabled_ui(cs.follows_mouse, |ui| {
                                *changed |= ui.checkbox(&mut cs.all_panes, "All panes").changed();
                                ui.add_enabled_ui(!app.performance_mode, |ui| {
                                    *changed |= ui
                                        .add(egui::Slider::new(&mut cs.speed, 0.3..=8.0)
                                            .text("Speed").step_by(0.1))
                                        .changed();
                                });
                            });
                            *changed |= ui
                                .add(egui::Slider::new(&mut cs.sigma, 0.10..=0.70)
                                    .text("Beam width").step_by(0.01))
                                .changed();
                            *changed |= ui
                                .add(egui::Slider::new(&mut cs.brightness, 0.1..=2.0)
                                    .text("Brightness").step_by(0.05))
                                .changed();
                        });
                    });

                egui::CollapsingHeader::new("Atmosphere")
                    .default_open(false)
                    .show(ui, |ui| {
                        ui.add_enabled_ui(!app.performance_mode, |ui| {
                            *changed |= ui.checkbox(&mut cs.shimmer, "Shimmer drift").changed();
                            ui.add_enabled_ui(cs.shimmer, |ui| {
                                *changed |= ui
                                    .add(egui::Slider::new(&mut cs.shimmer_strength, 0.0..=3.0)
                                        .text("Strength").step_by(0.05))
                                    .changed();
                            });
                        });
                        if app.performance_mode {
                            ui.label(egui::RichText::new("Disabled in performance mode").italics().weak());
                        }
                    });

                egui::CollapsingHeader::new("Radial glow")
                    .default_open(false)
                    .show(ui, |ui| {
                        *changed |= ui.checkbox(&mut cs.show_radial, "Show radial glow").changed();
                        ui.add_enabled_ui(cs.show_radial, |ui| {
                            *changed |= ui
                                .checkbox(&mut cs.follow_cursor, "Follow cursor freely")
                                .on_hover_text("Glow sits at cursor — unchecked = orbits the pane edge")
                                .changed();
                        });
                    });

                egui::CollapsingHeader::new("Decoration")
                    .default_open(false)
                    .show(ui, |ui| {
                        *changed |= ui.checkbox(&mut cs.show_halos, "Glow halos").changed();
                        *changed |= ui.checkbox(&mut cs.show_dots, "Orbit dots").changed();
                    });
            }
            }); // Frame
        });
}

fn workspace_tab_context_menu(
    ui: &mut egui::Ui,
    app: &mut MultermUi,
    idx: usize,
    changed: &mut bool,
    p: UiPalette,
) {
    if ui.button("Rename").clicked() {
        begin_workspace_tab_rename(app, idx);
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

fn begin_workspace_tab_rename(app: &mut MultermUi, idx: usize) {
    clear_active_workspace_terminal_focus(app);
    app.editing_workspace_idx = Some(idx);
    app.editing_workspace_input = app.workspaces[idx].title.clone();
    app.select_all_workspace_input_on_focus = true;
}

fn clear_active_workspace_terminal_focus(app: &mut MultermUi) {
    if let Some(runtime) = app.active_workspace_runtime_mut() {
        runtime.active_terminal = None;
        runtime.active_terminal_rect = None;
    }
}

/// `create_dir_all` is expected to succeed (parent exists and is a directory).
fn can_create_missing_workspace_dir(path: &Path) -> bool {
    if path.as_os_str().is_empty() || path.exists() {
        return false;
    }
    match path.parent() {
        None => false,
        Some(p) if p.as_os_str().is_empty() => false,
        Some(p) => p.is_dir(),
    }
}

/// Expand a leading `~` for filesystem checks and completion (matches common shell paths).
fn expand_tilde_in_working_dir_input(raw: &str) -> String {
    let t = raw.trim();
    if t.is_empty() {
        return String::new();
    }
    if t == "~" {
        return std::env::var("HOME").unwrap_or_default();
    }
    if let Some(rest) = t.strip_prefix("~/") {
        if let Ok(h) = std::env::var("HOME") {
            let h = h.trim_end_matches('/');
            return format!("{h}/{rest}");
        }
    }
    t.to_string()
}

/// Resolve `~`, join relative paths to the process cwd, for spawn and validation (matches user intent).
fn resolve_workspace_path_for_spawn(raw: &str) -> PathBuf {
    let expanded = expand_tilde_in_working_dir_input(raw);
    let t = expanded.trim();
    if t.is_empty() {
        return PathBuf::new();
    }
    let mut path = PathBuf::from(t);
    if path.is_relative() {
        if let Ok(cwd) = std::env::current_dir() {
            path = cwd.join(path);
        }
    }
    path
}

/// `None` when the workspace string can be used as a terminal cwd.
fn workspace_terminal_cwd_block(raw: &str) -> Option<WorkspaceTerminalCwdBlock> {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return Some(WorkspaceTerminalCwdBlock::Empty);
    }
    let path = resolve_workspace_path_for_spawn(raw);
    if path.as_os_str().is_empty() {
        return Some(WorkspaceTerminalCwdBlock::Empty);
    }
    if path.is_dir() {
        return None;
    }
    if path.exists() {
        return Some(WorkspaceTerminalCwdBlock::NotADir);
    }
    Some(WorkspaceTerminalCwdBlock::Missing)
}

fn workspace_terminal_spawn_block_message(block: WorkspaceTerminalCwdBlock) -> &'static str {
    match block {
        WorkspaceTerminalCwdBlock::Empty => "Set a workspace folder path first.",
        WorkspaceTerminalCwdBlock::NotADir => "Workspace path is not a folder.",
        WorkspaceTerminalCwdBlock::Missing => {
            "Workspace folder does not exist. Create it or pick another folder."
        }
    }
}

fn workspace_spawn_notice_from_block(
    raw: &str,
    block: WorkspaceTerminalCwdBlock,
) -> WorkspaceSpawnNotice {
    let message = workspace_terminal_spawn_block_message(block).to_string();
    let create_target = match block {
        WorkspaceTerminalCwdBlock::Missing => {
            let resolved = resolve_workspace_path_for_spawn(raw);
            can_create_missing_workspace_dir(&resolved).then_some(resolved)
        }
        _ => None,
    };
    WorkspaceSpawnNotice {
        message,
        create_target,
    }
}

/// Directory entries under the deepest usable prefix of the typed path, filtered by the last path segment.
fn workspace_dir_completion_candidates(raw_input: &str, max_entries: usize) -> Vec<String> {
    let expanded = expand_tilde_in_working_dir_input(raw_input);
    let t = expanded.trim();
    if t.is_empty() {
        return Vec::new();
    }
    let mut path = PathBuf::from(t);
    if path.is_relative() {
        if let Ok(cwd) = std::env::current_dir() {
            path = cwd.join(path);
        } else {
            return Vec::new();
        }
    }

    let (scan_dir, filter): (PathBuf, &str) = if path.is_dir() {
        (path, "")
    } else {
        let Some(parent) = path.parent() else {
            return Vec::new();
        };
        if parent.as_os_str().is_empty() || !parent.is_dir() {
            return Vec::new();
        }
        let filter = path.file_name().and_then(|n| n.to_str()).unwrap_or("");
        (parent.to_path_buf(), filter)
    };

    let Ok(rd) = fs::read_dir(&scan_dir) else {
        return Vec::new();
    };

    let mut out: Vec<String> = Vec::new();
    for entry in rd.flatten() {
        if out.len() >= max_entries {
            break;
        }
        let name = entry.file_name();
        let candidate = scan_dir.join(&name);
        // `DirEntry::file_type()` does not follow symlinks; `Path::is_dir()` does, so we only list
        // real directories (never plain files, including symlink → file).
        if !candidate.is_dir() {
            continue;
        }
        let n_lossy = name.to_string_lossy();
        if !filter.is_empty() && !n_lossy.starts_with(filter) {
            continue;
        }
        out.push(candidate.to_string_lossy().into_owned());
    }
    out.sort();
    out
}

fn directory_path_bar(ui: &mut egui::Ui, app: &mut MultermUi, p: UiPalette) {
    let full_width = ui.available_width();
    let bar_stroke = if app.ui_theme == UiTheme::Light {
        Stroke::NONE
    } else {
        Stroke::new(1.0, p.path_bar_border)
    };
    egui::Frame::default()
        .fill(p.path_bar_bg)
        .stroke(bar_stroke)
        .inner_margin(Margin::symmetric(10, 2))
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

            ui.vertical(|ui| {
                let mut working_dir_edit_response: Option<egui::Response> = None;
                ui.horizontal(|ui| {
                    let row_h = ui.spacing().interact_size.y.max(30.0);
                    let path_field_stroke = Stroke::new(1.0, p.path_bar_border);
                    let path_field_fill = p.term_bg;
                    let picker_btn = egui::Button::new(
                        RichText::new("Browse…")
                            .size(11.0)
                            .family(FontFamily::Monospace)
                            .color(p.muted),
                    )
                    .frame(true)
                    .fill(path_field_fill)
                    .stroke(path_field_stroke)
                    .corner_radius(3.0)
                    .min_size(egui::vec2(72.0, row_h));
                    if ui
                        .add(picker_btn)
                        .on_hover_text("Open the folder picker")
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
                                app.working_dir_editor_focus_next_frame = false;
                                app.working_dir_input.clear();
                            }
                        }
                    }
                    ui.add_space(6.0);
                    // Path field + completion list share this column so suggestions align with the
                    // TextEdit (same x as typed text), not with the full bar under "Browse…".
                    ui.vertical(|ui| {
                        let path_slot_width = ui.available_width();
                        if app.editing_working_dir {
                            let response = ui.add(
                                egui::TextEdit::singleline(&mut app.working_dir_input)
                                    .frame(true)
                                    .background_color(path_field_fill)
                                    .horizontal_align(egui::Align::Min)
                                    .vertical_align(egui::Align::Center)
                                    .desired_width(path_slot_width.max(120.0))
                                    .min_size(egui::vec2(path_slot_width.max(120.0), row_h))
                                    .font(egui::TextStyle::Monospace),
                            );
                            working_dir_edit_response = Some(response.clone());
                            if app.working_dir_editor_focus_next_frame {
                                response.request_focus();
                                app.working_dir_editor_focus_next_frame = false;
                            }
                            if response.changed() {
                                let candidate = app.working_dir_input.trim();
                                if working_dir_path_ok_to_store(Path::new(candidate)) {
                                    if let Some(w) = app.workspaces.get_mut(selected_idx) {
                                        if w.working_dir.as_str() != candidate {
                                            w.working_dir = candidate.to_string();
                                            save_workspace_state(app);
                                        }
                                    }
                                }
                            }
                        } else {
                            let slot_w = path_slot_width.max(0.0);
                        // `Button` always paints its text centered in the atom rect; use a framed label
                        // so the path stays left-aligned when not editing (same as the TextEdit).
                        let path_response = ui
                            .allocate_ui(egui::vec2(slot_w, row_h), |ui| {
                                egui::Frame::default()
                                    .fill(path_field_fill)
                                    .stroke(path_field_stroke)
                                    .corner_radius(3.0)
                                    .inner_margin(Margin::symmetric(8, 5))
                                    .show(ui, |ui| {
                                        ui.set_min_size(ui.available_size());
                                        let grab_rect = ui.max_rect();
                                        // `Label` only hit-tests the text galley; add a full-rect click target
                                        // so the empty area to the right of the path is also clickable.
                                        let full_click = ui.interact(
                                            grab_rect,
                                            ui.id().with("cwd_path_slot_click"),
                                            Sense::click(),
                                        );
                                        let label_resp = ui
                                            .new_child(
                                                egui::UiBuilder::new()
                                                    .max_rect(grab_rect)
                                                    .layout(egui::Layout::top_down(egui::Align::Min))
                                                    .id_salt("cwd_path_label"),
                                            )
                                            .add(
                                                egui::Label::new(
                                                    RichText::new(&displayed_dir)
                                                        .size(12.0)
                                                        .family(FontFamily::Monospace)
                                                        .color(p.muted),
                                                )
                                                .halign(egui::Align::Min)
                                                .truncate(),
                                            );
                                        full_click.union(label_resp)
                                    })
                                    .inner
                            })
                            .inner;
                        if path_response
                            .on_hover_cursor(CursorIcon::PointingHand)
                            .on_hover_text(
                                "Click to edit. Saves unless the path is an existing file. New terminals are blocked until this path is an existing folder; use the banner actions or path suggestions when offered.",
                            )
                            .clicked()
                        {
                            app.editing_working_dir = true;
                            app.working_dir_editor_focus_next_frame = true;
                            app.working_dir_input = app
                                .workspaces
                                .get(selected_idx)
                                .map(|w| w.working_dir.clone())
                                .unwrap_or_else(default_working_dir);
                        }
                        }

                        if app.editing_working_dir {
                            if let Some(base_resp) = working_dir_edit_response.clone() {
                                let suggestions = workspace_dir_completion_candidates(
                                    &app.working_dir_input,
                                    60,
                                );
                                let expanded =
                                    expand_tilde_in_working_dir_input(&app.working_dir_input);
                                let pb_for_create = PathBuf::from(expanded.trim());
                                let show_create_row =
                                    can_create_missing_workspace_dir(&pb_for_create);

                                let mut dismiss = base_resp;
                                if !suggestions.is_empty() || show_create_row {
                                    ui.add_space(4.0);
                                    let frame_out = egui::Frame::default()
                                        .fill(p.term_bg)
                                        .stroke(Stroke::new(1.0, p.path_bar_border))
                                        .corner_radius(3.0)
                                        // Match TextEdit frame inset so list text lines up with field text.
                                        .inner_margin(Margin::symmetric(4, 4))
                                        .show(ui, |ui| {
                                            ui.set_max_width(ui.available_width());
                                            egui::ScrollArea::vertical()
                                                .max_height(180.0)
                                                .id_salt(ui.id().with("cwd_path_complete_scroll"))
                                                .auto_shrink([true, true])
                                                .show(ui, |ui| {
                                                    ui.spacing_mut().item_spacing.y = 2.0;
                                                    for s in &suggestions {
                                                        if ui
                                                            .add(egui::Button::selectable(
                                                                false,
                                                                RichText::new(s)
                                                                    .size(11.0)
                                                                    .family(FontFamily::Monospace)
                                                                    .color(p.text),
                                                            )
                                                            .frame(false))
                                                            .clicked()
                                                        {
                                                            app.working_dir_input = s.clone();
                                                            let candidate = app.working_dir_input.trim();
                                                            if working_dir_path_ok_to_store(
                                                                Path::new(candidate),
                                                            ) {
                                                                if let Some(w) = app
                                                                    .workspaces
                                                                    .get_mut(selected_idx)
                                                                {
                                                                    if w.working_dir.as_str()
                                                                        != candidate
                                                                    {
                                                                        w.working_dir =
                                                                            candidate.to_string();
                                                                        save_workspace_state(app);
                                                                    }
                                                                }
                                                            }
                                                        }
                                                    }
                                                    if show_create_row {
                                                        ui.add_space(4.0);
                                                        ui.separator();
                                                        if ui
                                                            .small_button("Create this folder…")
                                                            .on_hover_text(
                                                                "Create this folder and all missing parents",
                                                            )
                                                            .clicked()
                                                        {
                                                            let _ = fs::create_dir_all(&pb_for_create);
                                                            if pb_for_create.is_dir() {
                                                                if let Some(w) = app
                                                                    .workspaces
                                                                    .get_mut(selected_idx)
                                                                {
                                                                    w.working_dir = pb_for_create
                                                                        .to_string_lossy()
                                                                        .into_owned();
                                                                    save_workspace_state(app);
                                                                }
                                                                app.working_dir_input.clear();
                                                            }
                                                        }
                                                    }
                                                });
                                        });
                                    dismiss = dismiss.union(frame_out.response);
                                }

                                let enter_pressed =
                                    ui.input(|i| i.key_pressed(egui::Key::Enter));
                                let esc_pressed =
                                    ui.input(|i| i.key_pressed(egui::Key::Escape));
                                if esc_pressed || enter_pressed || dismiss.clicked_elsewhere() {
                                    app.editing_working_dir = false;
                                    app.working_dir_editor_focus_next_frame = false;
                                    app.working_dir_input.clear();
                                }
                            }
                        }
                    });
                });
            });
        });
}

fn workspace_state_path() -> PathBuf {
    if let Ok(home) = std::env::var("HOME") {
        return PathBuf::from(home)
            .join(".multerm")
            .join("multerm-ui-workspaces.json");
    }
    PathBuf::from(".multerm-ui-workspaces.json")
}

fn save_workspace_state(app: &mut MultermUi) {
    app.ensure_workspace_runtime_slots();
    app.sync_all_workspace_history_snapshots();
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
                                agent_kind: pane.agent_kind,
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
                uploaded_images: app
                    .workspace_runtime
                    .get(idx)
                    .map(|runtime| runtime.uploaded_images.clone())
                    .unwrap_or_default(),
            })
            .collect(),
        next_workspace_index: app.next_workspace_index,
        color_history: app.color_history.clone(),
        usage_panel_pinned_scope: app.usage_panel_open_order.last().copied(),
        usage_panel_open_order: app.usage_panel_open_order.clone(),
        show_multerm_only_status: app.show_multerm_only_status,
        cyberpunk_settings: app.cyberpunk_settings,
        performance_mode: app.performance_mode,
        workspace_placement: app.workspace_placement,
        workspace_sidebar_visible: app.workspace_sidebar_visible,
        workspace_sidebar_width: app
            .workspace_sidebar_width
            .clamp(WORKSPACE_SIDEBAR_MIN_WIDTH, WORKSPACE_SIDEBAR_MAX_WIDTH),
    };

    let path = workspace_state_path();
    if let Some(parent) = path.parent() {
        if let Err(e) = fs::create_dir_all(parent) {
            tracing::warn!(
                error = %e,
                dir = %parent.display(),
                "failed to create workspace state directory"
            );
            return;
        }
    }

    let json = match serde_json::to_string_pretty(&state) {
        Ok(j) => j,
        Err(e) => {
            tracing::warn!(error = %e, "failed to serialize workspace state");
            return;
        }
    };

    let tmp = path.with_extension("json.tmp");
    if let Err(e) = fs::write(&tmp, &json) {
        tracing::warn!(
            error = %e,
            path = %tmp.display(),
            "failed to write workspace state temp file"
        );
        return;
    }
    if let Err(e) = fs::rename(&tmp, &path) {
        tracing::warn!(
            error = %e,
            from = %tmp.display(),
            to = %path.display(),
            "failed to finalize workspace state file"
        );
    }
}

fn load_workspace_state() -> Option<WorkspaceState> {
    let path = workspace_state_path();
    let json = match fs::read_to_string(&path) {
        Ok(j) => j,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return None,
        Err(e) => {
            tracing::warn!(
                error = %e,
                path = %path.display(),
                "failed to read workspace state file"
            );
            return None;
        }
    };
    let state: WorkspaceState = match serde_json::from_str(&json) {
        Ok(s) => s,
        Err(e) => {
            tracing::warn!(
                error = %e,
                path = %path.display(),
                "failed to parse workspace state JSON"
            );
            return None;
        }
    };
    if state.workspaces.is_empty() {
        tracing::warn!("workspace state contained no workspaces; ignoring");
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

impl MultermUi {
    fn workspace_tab_state_from_runtime(
        tab: &WorkspaceTab,
        runtime: Option<&WorkspaceRuntime>,
    ) -> WorkspaceTabState {
        WorkspaceTabState {
            title: tab.title.clone(),
            badge: tab.badge,
            color_rgba: tab.color_rgba,
            panel_layout: tab.panel_layout.sanitized(),
            sync_terminals_to_columns: Some(tab.sync_terminals_to_columns),
            uniform_equal_terminals: Some(tab.uniform_equal_terminals),
            working_dir: Some(tab.working_dir.clone()),
            terminal_sessions: runtime
                .map(|rt| {
                    rt.terminals
                        .iter()
                        .map(|pane| TerminalPaneState {
                            id: pane.id,
                            title: pane.title.clone(),
                            tmux_session: Some(pane.tmux_session.clone()),
                            agent_kind: pane.agent_kind,
                            width: pane.desired_size.x,
                            height: pane.desired_size.y,
                            x: pane.position.map(|p| p.x),
                            y: pane.position.map(|p| p.y),
                        })
                        .collect()
                })
                .unwrap_or_default(),
            active_terminal: runtime.and_then(|rt| rt.active_terminal),
            equal_size_source_terminal_id: runtime.and_then(|rt| rt.equal_size_source_terminal_id),
            uploaded_images: runtime
                .map(|rt| rt.uploaded_images.clone())
                .unwrap_or_default(),
        }
    }

    fn workspace_tab_from_state(state: &WorkspaceTabState) -> WorkspaceTab {
        WorkspaceTab {
            title: state.title.clone(),
            badge: state.badge,
            color_rgba: state.color_rgba,
            working_dir: state
                .working_dir
                .clone()
                .unwrap_or_else(default_working_dir),
            panel_layout: state.panel_layout.sanitized(),
            sync_terminals_to_columns: state.sync_terminals_to_columns.unwrap_or(false),
            uniform_equal_terminals: state.uniform_equal_terminals.unwrap_or(false),
        }
    }

    /// Reconcile an existing workspace runtime against a persisted snapshot in place.
    ///
    /// Used for both initial restore and undo/redo. Terminal panes that already exist
    /// (matched by `id`, falling back to `tmux_session`) are reused and just get their
    /// title/size/position updated, so undo/redo doesn't tear down live PTYs or do
    /// a synchronous `connect_daemon()` round trip per pane (which blocked the UI
    /// thread and showed a spinning cursor while terminals briefly went blank).
    fn reconcile_workspace_runtime_with_snapshot(
        &mut self,
        workspace_idx: usize,
        state: &WorkspaceTabState,
    ) {
        while self.workspace_runtime.len() <= workspace_idx {
            self.workspace_runtime.push(WorkspaceRuntime::default());
        }

        let mut existing: Vec<TerminalPane> =
            std::mem::take(&mut self.workspace_runtime[workspace_idx].terminals);
        let mut new_terminals: Vec<TerminalPane> =
            Vec::with_capacity(state.terminal_sessions.len());

        for pane_state in &state.terminal_sessions {
            let terminal_id = if pane_state.id == 0 {
                self.next_terminal_id
            } else {
                pane_state.id
            };
            let tmux_session = pane_state
                .tmux_session
                .clone()
                .unwrap_or_else(|| tmux_session_name(workspace_idx, terminal_id));

            let match_idx = existing
                .iter()
                .position(|p| p.id == terminal_id)
                .or_else(|| existing.iter().position(|p| p.tmux_session == tmux_session));

            let mut pane = if let Some(i) = match_idx {
                existing.remove(i)
            } else {
                spawn_terminal_pane(
                    pane_state.title.clone(),
                    terminal_id,
                    state.working_dir.as_deref().unwrap_or(""),
                    &tmux_session,
                )
            };

            pane.id = terminal_id;
            pane.title = pane_state.title.clone();
            pane.agent_kind = pane_state.agent_kind;
            pane.tmux_session = tmux_session;
            pane.desired_size =
                Vec2::new(pane_state.width.max(220.0), pane_state.height.max(120.0));
            pane.position = match (pane_state.x, pane_state.y) {
                (Some(x), Some(y)) => Some(Pos2::new(x, y)),
                _ => None,
            };

            self.next_terminal_id = self.next_terminal_id.max(terminal_id + 1);
            new_terminals.push(pane);
        }

        // Any pane left in `existing` is no longer in the snapshot; dropping it
        // closes its daemon / PTY connection.
        drop(existing);

        let runtime = &mut self.workspace_runtime[workspace_idx];
        runtime.terminals = new_terminals;
        runtime.active_terminal = state.active_terminal.and_then(|i| {
            if i < runtime.terminals.len() {
                Some(i)
            } else {
                runtime.terminals.len().checked_sub(1)
            }
        });
        runtime.equal_size_source_terminal_id = state.equal_size_source_terminal_id;
        runtime.uploaded_images = state.uploaded_images.clone();
        Self::sync_workspace_runtime_buffers(runtime);
    }

    fn capture_workspace_history_snapshot_for_idx(&self, idx: usize) -> Option<WorkspaceTabState> {
        let tab = self.workspaces.get(idx)?;
        Some(Self::workspace_tab_state_from_runtime(
            tab,
            self.workspace_runtime.get(idx),
        ))
    }

    fn workspace_tab_state_signature(state: &WorkspaceTabState) -> String {
        serde_json::to_string(state).unwrap_or_default()
    }

    fn describe_workspace_history_change(
        from: &WorkspaceTabState,
        to: &WorkspaceTabState,
    ) -> String {
        if from.title != to.title {
            return "Rename workspace".to_string();
        }
        if from.working_dir != to.working_dir {
            return "Change workspace folder".to_string();
        }
        if from.panel_layout.sanitized() != to.panel_layout.sanitized() {
            return "Change workspace layout".to_string();
        }
        if from.sync_terminals_to_columns != to.sync_terminals_to_columns {
            return "Toggle auto-fit width".to_string();
        }
        if from.uniform_equal_terminals != to.uniform_equal_terminals {
            return "Toggle equal-size terminals".to_string();
        }
        if from.color_rgba != to.color_rgba || from.badge != to.badge {
            return "Change workspace style".to_string();
        }

        let mut added = 0usize;
        let mut removed = 0usize;
        let mut moved = 0usize;
        let mut resized = 0usize;
        let mut renamed_terminal = 0usize;

        let pane_key = |pane: &TerminalPaneState| -> String {
            if pane.id != 0 {
                format!("id:{}", pane.id)
            } else if let Some(sess) = &pane.tmux_session {
                format!("session:{sess}")
            } else {
                format!("title:{}", pane.title)
            }
        };
        let near = |a: f32, b: f32| (a - b).abs() <= 0.5;
        let pos_near = |a: Option<f32>, b: Option<f32>| match (a, b) {
            (Some(ax), Some(bx)) => near(ax, bx),
            (None, None) => true,
            _ => false,
        };

        for pane in &to.terminal_sessions {
            if from
                .terminal_sessions
                .iter()
                .any(|old| pane_key(old) == pane_key(pane))
            {
                continue;
            }
            added += 1;
        }
        for pane in &from.terminal_sessions {
            if to
                .terminal_sessions
                .iter()
                .any(|now| pane_key(now) == pane_key(pane))
            {
                continue;
            }
            removed += 1;
        }

        if added > 0 || removed > 0 {
            return match (added, removed) {
                (1, 0) => "Add terminal".to_string(),
                (0, 1) => "Close terminal".to_string(),
                _ => format!("Change terminals (+{added}/-{removed})"),
            };
        }

        for before in &from.terminal_sessions {
            let key = pane_key(before);
            let Some(after) = to
                .terminal_sessions
                .iter()
                .find(|pane| pane_key(pane) == key)
            else {
                continue;
            };
            if before.title != after.title {
                renamed_terminal += 1;
            }
            let did_move = !pos_near(before.x, after.x) || !pos_near(before.y, after.y);
            let did_resize = !near(before.width, after.width) || !near(before.height, after.height);
            if did_move {
                moved += 1;
            }
            if did_resize {
                resized += 1;
            }
        }

        if renamed_terminal > 0 {
            return "Rename terminal".to_string();
        }
        if moved > 0 && resized > 0 {
            return "Move/resize terminal".to_string();
        }
        if moved > 0 {
            return "Move terminal".to_string();
        }
        if resized > 0 {
            return "Resize terminal".to_string();
        }
        if from.active_terminal != to.active_terminal {
            return "Change active terminal".to_string();
        }
        if from.equal_size_source_terminal_id != to.equal_size_source_terminal_id {
            return "Change equal-size template".to_string();
        }
        if from.uploaded_images.len() != to.uploaded_images.len() {
            return "Update uploaded images".to_string();
        }

        "Edit workspace".to_string()
    }

    fn workspace_history_neighbor_labels(
        &self,
        workspace_idx: usize,
    ) -> (Option<String>, Option<String>) {
        let Some(history) = self.workspace_edit_histories.get(workspace_idx) else {
            return (None, None);
        };
        let Some(current) = history.current.as_ref() else {
            return (None, None);
        };
        let prev_label = history
            .undo_stack
            .last()
            .map(|prev| Self::describe_workspace_history_change(prev, current));
        let next_label = history
            .redo_stack
            .last()
            .map(|next| Self::describe_workspace_history_change(current, next));
        (prev_label, next_label)
    }

    fn sync_all_workspace_history_snapshots(&mut self) {
        if self.workspace_history_suspended {
            return;
        }
        self.ensure_workspace_runtime_slots();
        for idx in 0..self.workspaces.len() {
            let Some(snapshot) = self.capture_workspace_history_snapshot_for_idx(idx) else {
                continue;
            };
            let history = &mut self.workspace_edit_histories[idx];
            let Some(current) = history.current.clone() else {
                history.current = Some(snapshot);
                continue;
            };
            if Self::workspace_tab_state_signature(&current)
                == Self::workspace_tab_state_signature(&snapshot)
            {
                continue;
            }
            history.undo_stack.push(current);
            if history.undo_stack.len() > WORKSPACE_EDIT_HISTORY_MAX {
                let overflow = history.undo_stack.len() - WORKSPACE_EDIT_HISTORY_MAX;
                history.undo_stack.drain(0..overflow);
            }
            history.redo_stack.clear();
            history.current = Some(snapshot);
        }
    }

    fn restore_workspace_snapshot_at_idx(&mut self, idx: usize, snapshot: &WorkspaceTabState) {
        if idx >= self.workspaces.len() {
            return;
        }
        self.workspaces[idx] = Self::workspace_tab_from_state(snapshot);
        self.ensure_workspace_runtime_slots();
        self.reconcile_workspace_runtime_with_snapshot(idx, snapshot);
        self.selected_workspace = idx;
        self.next_workspace_index = compute_next_workspace_index(&self.workspaces);
        self.ensure_workspace_runtime_slots();
    }

    /// Append a fresh workspace, inheriting working_dir / panel_layout / size
    /// flags from the currently selected workspace. Selects the new tab.
    fn add_workspace_inherit_selected(&mut self) {
        let title = format!("Workspace {}", self.next_workspace_index);
        let inherit_dir = self
            .workspaces
            .get(self.selected_workspace)
            .map(|w| w.working_dir.clone())
            .unwrap_or_else(default_working_dir);
        let inherit_layout = self
            .workspaces
            .get(self.selected_workspace)
            .map(|w| w.panel_layout)
            .unwrap_or_default();
        let sync = self
            .workspaces
            .get(self.selected_workspace)
            .map(|w| w.sync_terminals_to_columns)
            .unwrap_or(false);
        let uniform = self
            .workspaces
            .get(self.selected_workspace)
            .map(|w| w.uniform_equal_terminals)
            .unwrap_or(false);
        self.next_workspace_index += 1;
        self.workspaces.push(WorkspaceTab {
            title,
            badge: None,
            color_rgba: None,
            working_dir: inherit_dir,
            panel_layout: inherit_layout,
            sync_terminals_to_columns: sync,
            uniform_equal_terminals: uniform,
        });
        self.workspace_runtime.push(WorkspaceRuntime::default());
        self.selected_workspace = self.workspaces.len() - 1;
    }

    fn close_workspace_at(&mut self, idx: usize) -> bool {
        if idx >= self.workspaces.len() {
            return false;
        }

        if self.tab_drag.as_ref().map(|d| d.source_idx) == Some(idx) {
            self.tab_drag = None;
        }
        self.workspaces.remove(idx);
        if idx < self.workspace_runtime.len() {
            self.workspace_runtime.remove(idx);
        }
        if idx < self.workspace_edit_histories.len() {
            self.workspace_edit_histories.remove(idx);
        }
        if self.editing_workspace_idx == Some(idx) {
            self.editing_workspace_idx = None;
            self.editing_workspace_input.clear();
        } else if let Some(ei) = self.editing_workspace_idx {
            if ei > idx {
                self.editing_workspace_idx = Some(ei - 1);
            }
        }
        if self.workspaces.is_empty() {
            self.selected_workspace = 0;
        } else if self.selected_workspace > idx {
            self.selected_workspace -= 1;
        } else if self.selected_workspace == idx {
            self.selected_workspace = self.selected_workspace.saturating_sub(1);
        } else if self.selected_workspace >= self.workspaces.len() {
            self.selected_workspace = self.workspaces.len().saturating_sub(1);
        }
        true
    }

    fn can_undo_workspace_edit(&self) -> bool {
        self.workspace_edit_histories
            .get(self.selected_workspace)
            .is_some_and(|h| !h.undo_stack.is_empty())
    }

    fn can_redo_workspace_edit(&self) -> bool {
        self.workspace_edit_histories
            .get(self.selected_workspace)
            .is_some_and(|h| !h.redo_stack.is_empty())
    }

    fn undo_workspace_edit(&mut self) -> bool {
        self.ensure_workspace_runtime_slots();
        let idx = self
            .selected_workspace
            .min(self.workspaces.len().saturating_sub(1));
        let current_snapshot = self.capture_workspace_history_snapshot_for_idx(idx);
        let Some(history) = self.workspace_edit_histories.get_mut(idx) else {
            return false;
        };
        let Some(snapshot) = history.undo_stack.pop() else {
            return false;
        };
        let detail = current_snapshot
            .as_ref()
            .map(|current| Self::describe_workspace_history_change(&snapshot, current))
            .unwrap_or_else(|| "Edit workspace".to_string());
        if let Some(current) = current_snapshot {
            history.redo_stack.push(current);
        }
        self.restore_workspace_snapshot_at_idx(idx, &snapshot);
        if let Some(h) = self.workspace_edit_histories.get_mut(idx) {
            h.current = Some(snapshot);
        }
        self.push_workspace_history_overlay_entry(WorkspaceHistoryOverlayAction::Undo, detail);
        self.workspace_history_suspended = true;
        save_workspace_state(self);
        self.workspace_history_suspended = false;
        true
    }

    fn redo_workspace_edit(&mut self) -> bool {
        self.ensure_workspace_runtime_slots();
        let idx = self
            .selected_workspace
            .min(self.workspaces.len().saturating_sub(1));
        let current_snapshot = self.capture_workspace_history_snapshot_for_idx(idx);
        let Some(history) = self.workspace_edit_histories.get_mut(idx) else {
            return false;
        };
        let Some(snapshot) = history.redo_stack.pop() else {
            return false;
        };
        let detail = current_snapshot
            .as_ref()
            .map(|current| Self::describe_workspace_history_change(current, &snapshot))
            .unwrap_or_else(|| "Edit workspace".to_string());
        if let Some(current) = current_snapshot {
            history.undo_stack.push(current);
            if history.undo_stack.len() > WORKSPACE_EDIT_HISTORY_MAX {
                let overflow = history.undo_stack.len() - WORKSPACE_EDIT_HISTORY_MAX;
                history.undo_stack.drain(0..overflow);
            }
        }
        self.restore_workspace_snapshot_at_idx(idx, &snapshot);
        if let Some(h) = self.workspace_edit_histories.get_mut(idx) {
            h.current = Some(snapshot);
        }
        self.push_workspace_history_overlay_entry(WorkspaceHistoryOverlayAction::Redo, detail);
        self.workspace_history_suspended = true;
        save_workspace_state(self);
        self.workspace_history_suspended = false;
        true
    }

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
        while self.workspace_edit_histories.len() < self.workspaces.len() {
            self.workspace_edit_histories
                .push(WorkspaceEditHistory::default());
        }
        if self.workspace_edit_histories.len() > self.workspaces.len() {
            self.workspace_edit_histories
                .truncate(self.workspaces.len());
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
        if runtime.scrollback_searches.len() < runtime.terminals.len() {
            runtime
                .scrollback_searches
                .resize_with(runtime.terminals.len(), ScrollbackSearchPaneState::default);
        } else if runtime.scrollback_searches.len() > runtime.terminals.len() {
            runtime
                .scrollback_searches
                .truncate(runtime.terminals.len());
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
        let current = runtime.equal_size_source_terminal_id.or(fallback);
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
                let down = (start_y + delta <= max_y + 0.01)
                    .then_some((start_y + delta).clamp(0.0, max_y));
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
    let left =
        intrusion_left_right_aligned(terminals, stripe_left, stripe_right, y, h, GRID_SPACING);
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
    let slot = column_stripe_width(available_width, layout).clamp(1.0, available_width.max(1.0));
    let max_x = (available_width - slot).max(0.0);

    let mut idxs: Vec<usize> = (0..terminals.len()).collect();
    idxs.sort_by(|&a, &b| {
        let pa = terminals[a].position.unwrap_or_default();
        let pb = terminals[b].position.unwrap_or_default();
        let ca = pick_column_at_x(pa.x + slot * 0.25, available_width, layout);
        let cb = pick_column_at_x(pb.x + slot * 0.25, available_width, layout);
        ca.cmp(&cb).then(pa.y.total_cmp(&pb.y)).then(a.cmp(&b))
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
    let grid_cell_w =
        column_stripe_width(available_width, layout).clamp(1.0, available_width.max(1.0));
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
    let preferred_has_native = column_has_native_pane(terminals, area_width, preferred, layout);
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

/// Reject only paths that already exist and are not directories (e.g. files).
/// Missing paths are allowed so the user can type a new folder before creating it explicitly.
fn working_dir_path_ok_to_store(path: &Path) -> bool {
    if path.as_os_str().is_empty() {
        return false;
    }
    match fs::metadata(path) {
        Ok(m) => m.is_dir(),
        Err(_) => true,
    }
}

/// Returns a directory suitable for `spawn_pty` cwd when the workspace path is an existing folder.
/// Does not create missing directories (use the path editor's create action).
fn ensure_working_dir_for_spawn(raw: &str) -> String {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return default_working_dir();
    }
    let path = resolve_workspace_path_for_spawn(raw);
    if path.is_dir() {
        path.to_string_lossy().into_owned()
    } else {
        default_working_dir()
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
    format!("#{:02X}{:02X}{:02X}", color.r(), color.g(), color.b())
}

/// Load an image file as an egui texture, downscaling to `max_dim` pixels on the longest side.
fn load_image_thumbnail(
    path: &str,
    ctx: &egui::Context,
    max_dim: u32,
    texture_key: &str,
) -> Option<egui::TextureHandle> {
    let dyn_img = image::ImageReader::open(path).ok()?.decode().ok()?;
    let rgba_image = dyn_img.to_rgba8();
    let (w0, h0) = rgba_image.dimensions();
    let w = w0 as usize;
    let h = h0 as usize;
    if w == 0 || h == 0 {
        return None;
    }
    let rgba = rgba_image.as_raw();

    // Nearest-neighbour downscale if larger than max_dim.
    let scale = (max_dim as f32 / w.max(h) as f32).min(1.0);
    let (tw, th) = (
        ((w as f32 * scale) as usize).max(1),
        ((h as f32 * scale) as usize).max(1),
    );

    let scaled: Vec<u8> = if scale < 1.0 {
        let inv = 1.0 / scale;
        let mut out = Vec::with_capacity(tw * th * 4);
        for ty in 0..th {
            for tx in 0..tw {
                let sx = ((tx as f32 * inv) as usize).min(w - 1);
                let sy = ((ty as f32 * inv) as usize).min(h - 1);
                let base = (sy * w + sx) * 4;
                out.extend_from_slice(&rgba[base..base + 4]);
            }
        }
        out
    } else {
        rgba.to_vec()
    };

    let color_image = egui::ColorImage::from_rgba_unmultiplied([tw, th], &scaled);
    Some(ctx.load_texture(
        format!("gallery_{texture_key}:{path}"),
        color_image,
        egui::TextureOptions::default(),
    ))
}
