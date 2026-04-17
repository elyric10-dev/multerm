use std::io::Write as IoWrite;
use unicode_width::UnicodeWidthChar;
use vte::{Params, Parser, Perform};

use crate::cell::{Cell, CellAttrs, Color, WideKind};
use crate::grid::TerminalGrid;

// ── Performer ────────────────────────────────────────────────────────────────

struct VtePerformer {
    pub grid: TerminalGrid,

    // Unique instance id for log correlation
    id: u32,

    // Current SGR state
    fg:    Color,
    bg:    Color,
    attrs: CellAttrs,

    // Saved cursor + SGR
    saved_row:   usize,
    saved_col:   usize,
    saved_fg:    Color,
    saved_bg:    Color,
    saved_attrs: CellAttrs,

    // Modes
    pub app_cursor_keys: bool,
    pub cursor_visible:  bool,

    // Auto-wrap pending flag: set when last printed char reached last column.
    pending_wrap: bool,

    // Tab stops (true = stop exists at that column index)
    tab_stops: Vec<bool>,
}

impl VtePerformer {
    fn new(rows: usize, cols: usize) -> Self {
        use std::sync::atomic::{AtomicU32, Ordering};
        static NEXT_ID: AtomicU32 = AtomicU32::new(1);
        let id = NEXT_ID.fetch_add(1, Ordering::Relaxed);
        let mut tab_stops = vec![false; cols.max(1)];
        for i in (0..cols).step_by(8) {
            tab_stops[i] = true;
        }
        Self {
            grid:            TerminalGrid::new(rows, cols),
            id,
            fg:              Color::Default,
            bg:              Color::Default,
            attrs:           CellAttrs::empty(),
            saved_row:       0,
            saved_col:       0,
            saved_fg:        Color::Default,
            saved_bg:        Color::Default,
            saved_attrs:     CellAttrs::empty(),
            app_cursor_keys: false,
            cursor_visible:  true,
            pending_wrap:    false,
            tab_stops,
        }
    }

    fn resize_tab_stops(&mut self, cols: usize) {
        self.tab_stops = vec![false; cols.max(1)];
        for i in (0..cols).step_by(8) {
            self.tab_stops[i] = true;
        }
    }

    // ── Debug helper (writes to /tmp/termite_vt.log) ─────────────────────────

    fn vt_log(&self, msg: &str) {
        if let Ok(mut f) = std::fs::OpenOptions::new()
            .create(true).append(true)
            .open("/tmp/termite_vt.log")
        {
            let _ = writeln!(f, "p{} t={} rows={} cursor=({},{}) scroll=({},{}) | {}",
                self.id,
                std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .map(|d| d.as_millis())
                    .unwrap_or(0),
                self.grid.rows,
                self.grid.cursor.row, self.grid.cursor.col,
                self.grid.scroll_top, self.grid.scroll_bot,
                msg);
        }
    }

    // ── Cursor movement helpers ───────────────────────────────────────────────

    fn cursor_up(&mut self, n: usize) {
        self.pending_wrap = false;
        let n = n.max(1);
        self.vt_log(&format!("cursor_up({})", n));
        // CUU should move within the full screen, not be clamped to DECSTBM.
        // Clamping to scroll_top traps some TUIs' cursors near the bottom.
        self.grid.cursor.row = self.grid.cursor.row.saturating_sub(n);
    }

    fn cursor_down(&mut self, n: usize) {
        self.pending_wrap = false;
        let n = n.max(1);
        // CUD should move within the full screen, not be clamped to DECSTBM.
        self.grid.cursor.row = (self.grid.cursor.row + n).min(self.grid.rows - 1);
    }

    fn cursor_forward(&mut self, n: usize) {
        self.pending_wrap = false;
        let n = n.max(1);
        self.grid.cursor.col = (self.grid.cursor.col + n).min(self.grid.cols - 1);
    }

    fn cursor_back(&mut self, n: usize) {
        self.pending_wrap = false;
        let n = n.max(1);
        self.grid.cursor.col = self.grid.cursor.col.saturating_sub(n);
    }

    fn advance_line(&mut self) {
        let bot = self.grid.scroll_bot;
        let top = self.grid.scroll_top;
        self.vt_log("LF");
        if self.grid.cursor.row >= bot {
            self.grid.scroll_up(top, bot, 1);
        } else {
            self.grid.cursor.row += 1;
        }
    }

    // ── SGR ──────────────────────────────────────────────────────────────────

    fn process_sgr(&mut self, params: &Params) {
        // Flatten params into a Vec<u16> for easy indexed parsing.
        let flat: Vec<u16> = params.iter().map(|sub| sub[0]).collect();
        if flat.is_empty() {
            self.reset_sgr();
            return;
        }
        let mut i = 0usize;
        while i < flat.len() {
            match flat[i] {
                0  => self.reset_sgr(),
                1  => self.attrs.insert(CellAttrs::BOLD),
                2  => self.attrs.insert(CellAttrs::DIM),
                3  => self.attrs.insert(CellAttrs::ITALIC),
                4  => self.attrs.insert(CellAttrs::UNDERLINE),
                5 | 6 => self.attrs.insert(CellAttrs::BLINK),
                7  => self.attrs.insert(CellAttrs::REVERSE),
                8  => self.attrs.insert(CellAttrs::INVISIBLE),
                9  => self.attrs.insert(CellAttrs::STRIKETHROUGH),
                22 => self.attrs.remove(CellAttrs::BOLD | CellAttrs::DIM),
                23 => self.attrs.remove(CellAttrs::ITALIC),
                24 => self.attrs.remove(CellAttrs::UNDERLINE),
                25 => self.attrs.remove(CellAttrs::BLINK),
                27 => self.attrs.remove(CellAttrs::REVERSE),
                28 => self.attrs.remove(CellAttrs::INVISIBLE),
                29 => self.attrs.remove(CellAttrs::STRIKETHROUGH),
                // Foreground: 30-37 ANSI, 39 default
                n @ 30..=37 => self.fg = Color::Indexed(n as u8 - 30),
                38 => {
                    if i + 2 < flat.len() && flat[i + 1] == 5 {
                        self.fg = Color::Indexed(flat[i + 2] as u8);
                        i += 2;
                    } else if i + 4 < flat.len() && flat[i + 1] == 2 {
                        self.fg = Color::Rgb(flat[i+2] as u8, flat[i+3] as u8, flat[i+4] as u8);
                        i += 4;
                    }
                }
                39 => self.fg = Color::Default,
                // Background: 40-47 ANSI, 49 default
                n @ 40..=47 => self.bg = Color::Indexed(n as u8 - 40 + 8 + 8),
                48 => {
                    if i + 2 < flat.len() && flat[i + 1] == 5 {
                        self.bg = Color::Indexed(flat[i + 2] as u8);
                        i += 2;
                    } else if i + 4 < flat.len() && flat[i + 1] == 2 {
                        self.bg = Color::Rgb(flat[i+2] as u8, flat[i+3] as u8, flat[i+4] as u8);
                        i += 4;
                    }
                }
                49 => self.bg = Color::Default,
                // Bright foreground: 90-97
                n @ 90..=97 => self.fg = Color::Indexed(n as u8 - 90 + 8),
                // Bright background: 100-107
                n @ 100..=107 => self.bg = Color::Indexed(n as u8 - 100 + 8),
                _ => {}
            }
            i += 1;
        }
    }

    fn reset_sgr(&mut self) {
        self.fg    = Color::Default;
        self.bg    = Color::Default;
        self.attrs = CellAttrs::empty();
    }

    // ── Erase helpers ────────────────────────────────────────────────────────

    fn erase_in_display(&mut self, n: u16) {
        let rows = self.grid.rows;
        let cols = self.grid.cols;
        let (crow, ccol) = (self.grid.cursor.row, self.grid.cursor.col);
        match n {
            // Erase from cursor to end of screen
            0 => {
                self.grid.clear_line_range(crow, ccol, cols);
                for r in (crow + 1)..rows {
                    self.grid.clear_line_range(r, 0, cols);
                }
            }
            // Erase from start to cursor
            1 => {
                for r in 0..crow {
                    self.grid.clear_line_range(r, 0, cols);
                }
                self.grid.clear_line_range(crow, 0, ccol + 1);
            }
            // Erase entire screen
            2 | 3 => self.grid.clear_all(),
            _ => {}
        }
    }

    fn erase_in_line(&mut self, n: u16) {
        let cols  = self.grid.cols;
        let (row, col) = (self.grid.cursor.row, self.grid.cursor.col);
        match n {
            0 => self.grid.clear_line_range(row, col, cols),
            1 => self.grid.clear_line_range(row, 0, col + 1),
            2 => self.grid.clear_line_range(row, 0, cols),
            _ => {}
        }
    }
}

// ── vte::Perform impl ────────────────────────────────────────────────────────

impl Perform for VtePerformer {
    fn print(&mut self, c: char) {
        // Handle pending auto-wrap
        if self.pending_wrap {
            self.pending_wrap = false;
            self.grid.cursor.col = 0;
            self.advance_line();
        }

        let row = self.grid.cursor.row;
        let col = self.grid.cursor.col;
        if row >= self.grid.rows || col >= self.grid.cols {
            return;
        }

        let width = UnicodeWidthChar::width(c).unwrap_or(1);
        let is_wide = width >= 2;

        {
            let cell = self.grid.cell_mut(row, col);
            cell.ch    = c;
            cell.fg    = self.fg;
            cell.bg    = self.bg;
            cell.attrs = self.attrs;
            cell.wide  = if is_wide { WideKind::Leading } else { WideKind::None };
        }

        if is_wide {
            // Fill trailing cell
            if col + 1 < self.grid.cols {
                let trail = self.grid.cell_mut(row, col + 1);
                trail.ch    = ' ';
                trail.fg    = self.fg;
                trail.bg    = self.bg;
                trail.attrs = self.attrs;
                trail.wide  = WideKind::Trailing;
            }
            let next_col = col + 2;
            if next_col >= self.grid.cols {
                self.pending_wrap = true;
            } else {
                self.grid.cursor.col = next_col;
            }
        } else {
            let next_col = col + 1;
            if next_col >= self.grid.cols {
                self.pending_wrap = true;
            } else {
                self.grid.cursor.col = next_col;
            }
        }
    }

    fn execute(&mut self, byte: u8) {
        match byte {
            // BEL — ignore
            0x07 => {}
            // BS — backspace
            0x08 => {
                self.pending_wrap = false;
                if self.grid.cursor.col > 0 {
                    self.grid.cursor.col -= 1;
                }
            }
            // HT — horizontal tab
            0x09 => {
                self.pending_wrap = false;
                let cols = self.tab_stops.len();
                let mut col = self.grid.cursor.col + 1;
                while col < cols && !self.tab_stops[col] {
                    col += 1;
                }
                self.grid.cursor.col = col.min(cols.saturating_sub(1));
            }
            // LF / VT / FF
            0x0A | 0x0B | 0x0C => {
                self.pending_wrap = false;
                self.advance_line();
            }
            // CR
            0x0D => {
                self.pending_wrap = false;
                self.grid.cursor.col = 0;
            }
            _ => {}
        }
    }

    fn csi_dispatch(&mut self, params: &Params, intermediates: &[u8], _ignore: bool, c: char) {
        let is_private = intermediates.first() == Some(&b'?');

        // Collect first param (default 0 or 1 depending on command)
        let p0 = params.iter().next().map(|s| s[0]).unwrap_or(0);
        let p0_1 = p0.max(1); // default-1 params

        match (is_private, c) {
            // Cursor Up
            (false, 'A') => self.cursor_up(p0_1 as usize),
            // Cursor Down
            (false, 'B') => self.cursor_down(p0_1 as usize),
            // Cursor Forward
            (false, 'C') => self.cursor_forward(p0_1 as usize),
            // Cursor Back
            (false, 'D') => self.cursor_back(p0_1 as usize),
            // Cursor Next Line
            (false, 'E') => {
                self.cursor_down(p0_1 as usize);
                self.grid.cursor.col = 0;
            }
            // Cursor Previous Line
            (false, 'F') => {
                self.cursor_up(p0_1 as usize);
                self.grid.cursor.col = 0;
            }
            // Cursor Horizontal Absolute
            (false, 'G') => {
                self.pending_wrap = false;
                self.grid.cursor.col = (p0_1 as usize).saturating_sub(1).min(self.grid.cols - 1);
            }
            // Cursor Position (row, col) — 1-based
            (false, 'H') | (false, 'f') => {
                self.pending_wrap = false;
                let mut params_iter = params.iter();
                let row = params_iter.next().map(|s| s[0]).unwrap_or(1).max(1) as usize;
                let col = params_iter.next().map(|s| s[0]).unwrap_or(1).max(1) as usize;
                self.vt_log(&format!("CUP ({},{}) → grid rows={}", row, col, self.grid.rows));
                self.grid.cursor.row = (row - 1).min(self.grid.rows - 1);
                self.grid.cursor.col = (col - 1).min(self.grid.cols - 1);
            }
            // Erase in Display
            (false, 'J') => self.erase_in_display(p0),
            // Erase in Line
            (false, 'K') => self.erase_in_line(p0),
            // Insert Lines
            (false, 'L') => {
                let row = self.grid.cursor.row;
                let bot = self.grid.scroll_bot;
                self.grid.scroll_down(row, bot, p0_1 as usize);
            }
            // Delete Lines
            (false, 'M') => {
                let row = self.grid.cursor.row;
                let bot = self.grid.scroll_bot;
                self.grid.scroll_up(row, bot, p0_1 as usize);
            }
            // Delete Characters
            (false, 'P') => {
                let row = self.grid.cursor.row;
                let col = self.grid.cursor.col;
                let n   = (p0_1 as usize).min(self.grid.cols - col);
                let cols = self.grid.cols;
                for c in col..(cols - n) {
                    self.grid.cells[row * cols + c] = self.grid.cells[row * cols + c + n].clone();
                    self.grid.dirty[row] = true;
                }
                for c in (cols - n)..cols {
                    self.grid.cells[row * cols + c] = Cell::default();
                }
            }
            // Scroll Up
            (false, 'S') => {
                let top = self.grid.scroll_top;
                let bot = self.grid.scroll_bot;
                self.grid.scroll_up(top, bot, p0_1 as usize);
            }
            // Scroll Down
            (false, 'T') => {
                let top = self.grid.scroll_top;
                let bot = self.grid.scroll_bot;
                self.grid.scroll_down(top, bot, p0_1 as usize);
            }
            // Erase Characters
            (false, 'X') => {
                let row = self.grid.cursor.row;
                let col = self.grid.cursor.col;
                let end = (col + p0_1 as usize).min(self.grid.cols);
                self.grid.clear_line_range(row, col, end);
            }
            // Cursor Vertical Absolute
            (false, 'd') => {
                self.pending_wrap = false;
                self.grid.cursor.row = (p0_1 as usize).saturating_sub(1).min(self.grid.rows - 1);
            }
            // Set/Reset mode (DEC private)
            (true, 'h') | (true, 'l') => {
                let set = c == 'h';
                for sub in params.iter() {
                    match sub[0] {
                        1    => self.app_cursor_keys = set,
                        7    => {} // auto-wrap (always on for us)
                        25   => self.cursor_visible = set,
                        1049 => {
                            if set { self.grid.enter_alternate(); }
                            else   { self.grid.leave_alternate(); }
                        }
                        47 | 1047 => {
                            if set { self.grid.enter_alternate(); }
                            else   { self.grid.leave_alternate(); }
                        }
                        _ => {}
                    }
                }
            }
            // SGR
            (false, 'm') => self.process_sgr(params),
            // Device Status Report — send cursor position
            (false, 'n') if p0 == 6 => {
                // We can't send a response here without write access; ignore for now.
            }
            // ANSI cursor save (same semantics as ESC 7 / DECSC)
            (false, 's') => {
                self.vt_log("CSI s  (ANSI save)");
                self.saved_row   = self.grid.cursor.row;
                self.saved_col   = self.grid.cursor.col;
                self.saved_fg    = self.fg;
                self.saved_bg    = self.bg;
                self.saved_attrs = self.attrs;
            }
            // ANSI cursor restore (same semantics as ESC 8 / DECRC)
            (false, 'u') => {
                self.vt_log(&format!("CSI u  (ANSI restore) → saved=({},{})",
                    self.saved_row, self.saved_col));
                self.grid.cursor.row = self.saved_row.min(self.grid.rows - 1);
                self.grid.cursor.col = self.saved_col.min(self.grid.cols - 1);
                self.fg              = self.saved_fg;
                self.bg              = self.saved_bg;
                self.attrs           = self.saved_attrs;
                self.pending_wrap    = false;
            }
            // Set Scrolling Region (DECSTBM)
            (false, 'r') => {
                let mut pi = params.iter();
                let top = pi.next().map(|s| s[0]).unwrap_or(1).max(1) as usize;
                let bot = pi.next().map(|s| s[0]).unwrap_or(self.grid.rows as u16) as usize;
                let top = (top - 1).min(self.grid.rows - 1);
                let bot = bot.min(self.grid.rows).saturating_sub(1);
                self.vt_log(&format!("DECSTBM top={} bot={}", top, bot));
                if top < bot {
                    self.grid.scroll_top = top;
                    self.grid.scroll_bot = bot;
                    // Move cursor to home
                    self.grid.cursor.row = 0;
                    self.grid.cursor.col = 0;
                }
            }
            _ => {}
        }
    }

    fn esc_dispatch(&mut self, intermediates: &[u8], _ignore: bool, byte: u8) {
        match (intermediates.first(), byte) {
            // Save cursor (DECSC)
            (None, b'7') => {
                self.vt_log("ESC 7  (DEC save)");
                self.saved_row   = self.grid.cursor.row;
                self.saved_col   = self.grid.cursor.col;
                self.saved_fg    = self.fg;
                self.saved_bg    = self.bg;
                self.saved_attrs = self.attrs;
            }
            // Restore cursor (DECRC)
            (None, b'8') => {
                self.vt_log(&format!("ESC 8  (DEC restore) → saved=({},{})",
                    self.saved_row, self.saved_col));
                self.grid.cursor.row = self.saved_row.min(self.grid.rows - 1);
                self.grid.cursor.col = self.saved_col.min(self.grid.cols - 1);
                self.fg              = self.saved_fg;
                self.bg              = self.saved_bg;
                self.attrs           = self.saved_attrs;
                self.pending_wrap    = false;
            }
            // RIS — reset
            (None, b'c') => {
                self.grid.clear_all();
                self.grid.cursor = Default::default();
                self.reset_sgr();
                self.app_cursor_keys = false;
                self.cursor_visible  = true;
                self.pending_wrap    = false;
                let cols = self.grid.cols;
                self.resize_tab_stops(cols);
            }
            _ => {}
        }
    }

    // Ignore OSC, DCS, APC for now (title setting, etc.)
    fn osc_dispatch(&mut self, _params: &[&[u8]], _bell_terminated: bool) {}
    fn hook(&mut self, _params: &Params, _intermediates: &[u8], _ignore: bool, _c: char) {}
    fn put(&mut self, _byte: u8) {}
    fn unhook(&mut self) {}
}

// ── Public API ───────────────────────────────────────────────────────────────

/// Wraps `vte::Parser` and `VtePerformer`; processes raw PTY bytes into a `TerminalGrid`.
pub struct TerminalParser {
    parser:    Parser,
    performer: VtePerformer,
}

impl TerminalParser {
    pub fn new(rows: usize, cols: usize) -> Self {
        Self {
            parser:    Parser::new(),
            performer: VtePerformer::new(rows, cols),
        }
    }

    /// Feed raw bytes from the PTY into the parser.
    pub fn process(&mut self, data: &[u8]) {
        for &byte in data {
            self.parser.advance(&mut self.performer, byte);
        }
    }


    pub fn resize(&mut self, rows: usize, cols: usize) {
        self.performer.grid.resize(rows, cols);
        self.performer.resize_tab_stops(cols);
    }

    /// Read-only access to the current terminal grid.
    pub fn grid(&self) -> &TerminalGrid {
        &self.performer.grid
    }

    /// Mutable access (e.g. for dirty-flag clearing).
    pub fn grid_mut(&mut self) -> &mut TerminalGrid {
        &mut self.performer.grid
    }

    pub fn app_cursor_keys(&self) -> bool {
        self.performer.app_cursor_keys
    }

    pub fn cursor_visible(&self) -> bool {
        self.performer.cursor_visible
    }
}
