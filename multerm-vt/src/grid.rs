use std::collections::VecDeque;

use crate::cell::{Cell, CellAttrs, Color, WideKind};

/// Hard cap on saved primary-buffer lines (oldest dropped first).
pub const SCROLLBACK_MAX_LINES: usize = 100_000;

/// Cursor position within the grid.
#[derive(Clone, Copy, Debug, Default)]
pub struct Cursor {
    pub row: usize,
    pub col: usize,
}

/// The 2-D terminal screen buffer.
#[derive(Clone, Debug)]
pub struct TerminalGrid {
    pub rows: usize,
    pub cols: usize,
    /// Row-major, size = rows * cols.
    pub cells: Vec<Cell>,
    pub cursor: Cursor,
    /// Per-row dirty flag; reset by the renderer after each frame.
    pub dirty: Vec<bool>,
    /// Top of scrolling region (0-based, inclusive).
    pub scroll_top: usize,
    /// Bottom of scrolling region (0-based, inclusive).
    pub scroll_bot: usize,
    /// Saved primary screen when inside alternate screen; `None` when on primary.
    alternate: Option<Box<TerminalGrid>>,
    pub in_alt: bool,
    /// Primary-buffer lines that scrolled off the top (oldest at front). Not used on the
    /// alternate screen. Each row has length `cols`.
    pub scrollback: VecDeque<Vec<Cell>>,
}

impl TerminalGrid {
    pub fn new(rows: usize, cols: usize) -> Self {
        let total = rows.max(1) * cols.max(1);
        Self {
            rows,
            cols,
            cells: vec![Cell::default(); total],
            cursor: Cursor::default(),
            dirty: vec![true; rows.max(1)],
            scroll_top: 0,
            scroll_bot: rows.saturating_sub(1),
            alternate: None,
            in_alt: false,
            scrollback: VecDeque::new(),
        }
    }

    // ── Accessors ────────────────────────────────────────────────────────────

    #[inline]
    pub fn cell(&self, row: usize, col: usize) -> &Cell {
        &self.cells[row * self.cols + col]
    }

    #[inline]
    pub fn cell_mut(&mut self, row: usize, col: usize) -> &mut Cell {
        self.dirty[row] = true;
        &mut self.cells[row * self.cols + col]
    }

    // ── Resize ───────────────────────────────────────────────────────────────

    pub fn resize(&mut self, new_rows: usize, new_cols: usize) {
        let new_rows = new_rows.max(1);
        let new_cols = new_cols.max(1);
        let mut new_cells = vec![Cell::default(); new_rows * new_cols];
        let copy_rows = self.rows.min(new_rows);
        let copy_cols = self.cols.min(new_cols);
        for r in 0..copy_rows {
            for c in 0..copy_cols {
                new_cells[r * new_cols + c] = self.cells[r * self.cols + c].clone();
            }
        }
        self.rows = new_rows;
        self.cols = new_cols;
        self.cells = new_cells;
        self.dirty = vec![true; new_rows];
        self.cursor.row = self.cursor.row.min(new_rows - 1);
        self.cursor.col = self.cursor.col.min(new_cols - 1);
        self.scroll_top = 0;
        self.scroll_bot = new_rows - 1;
        for line in self.scrollback.iter_mut() {
            line.resize(new_cols, Cell::default());
        }
    }

    /// Rows in the live buffer plus saved scrollback (for display / hit testing).
    #[inline]
    pub fn total_rows(&self) -> usize {
        self.scrollback.len() + self.rows
    }

    #[inline]
    pub fn scrollback_len(&self) -> usize {
        self.scrollback.len()
    }

    /// Row index in the combined view: `0..scrollback_len()` then live rows.
    #[inline]
    pub fn virtual_cell(&self, vrow: usize, col: usize) -> &Cell {
        let sb = self.scrollback.len();
        if vrow < sb {
            &self.scrollback[vrow][col]
        } else {
            &self.cells[(vrow - sb) * self.cols + col]
        }
    }

    /// Exclusive end column on virtual row `vrow` (trim trailing unstyled spaces).
    pub fn virtual_row_render_end(&self, vrow: usize) -> usize {
        if vrow >= self.total_rows() || self.cols == 0 {
            return 0;
        }
        let visible_space_attrs =
            CellAttrs::REVERSE | CellAttrs::UNDERLINE | CellAttrs::STRIKETHROUGH;
        let mut end = self.cols;
        while end > 0 {
            let cell = self.virtual_cell(vrow, end - 1);
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

    fn push_scrollback_line(&mut self, line: Vec<Cell>) {
        self.scrollback.push_back(line);
        while self.scrollback.len() > SCROLLBACK_MAX_LINES {
            self.scrollback.pop_front();
        }
    }

    // ── Scroll helpers ───────────────────────────────────────────────────────

    /// Scroll lines `top..=bot` up by `n`, clearing vacated rows at bottom.
    pub fn scroll_up(&mut self, top: usize, bot: usize, n: usize) {
        let n = n.min(bot - top + 1);
        for _ in 0..n {
            if !self.in_alt {
                let mut line = Vec::with_capacity(self.cols);
                for c in 0..self.cols {
                    line.push(self.cells[top * self.cols + c].clone());
                }
                self.push_scrollback_line(line);
            }
            for r in top..bot {
                for c in 0..self.cols {
                    self.cells[r * self.cols + c] = self.cells[(r + 1) * self.cols + c].clone();
                }
                self.dirty[r] = true;
            }
            for c in 0..self.cols {
                self.cells[bot * self.cols + c] = Cell::default();
            }
            self.dirty[bot] = true;
        }
    }

    /// Scroll lines `top..=bot` down by `n`, clearing vacated rows at top.
    pub fn scroll_down(&mut self, top: usize, bot: usize, n: usize) {
        let n = n.min(bot - top + 1);
        for _ in 0..n {
            for r in (top..bot).rev() {
                for c in 0..self.cols {
                    self.cells[(r + 1) * self.cols + c] = self.cells[r * self.cols + c].clone();
                }
                self.dirty[r + 1] = true;
            }
            for c in 0..self.cols {
                self.cells[top * self.cols + c] = Cell::default();
            }
            self.dirty[top] = true;
        }
    }

    // ── Erase helpers ────────────────────────────────────────────────────────

    pub fn clear_all(&mut self) {
        self.cells.iter_mut().for_each(|c| *c = Cell::default());
        self.dirty.iter_mut().for_each(|d| *d = true);
    }

    /// Drop primary-buffer lines saved from scrolling (no-op on alternate screen callers
    /// that target this grid's deque; nested primary while in alt is unchanged).
    pub fn clear_scrollback(&mut self) {
        self.scrollback.clear();
    }

    /// Erase columns `from_col..to_col` on `row` (to_col is exclusive).
    pub fn clear_line_range(&mut self, row: usize, from_col: usize, to_col: usize) {
        let end = to_col.min(self.cols);
        for c in from_col..end {
            self.cells[row * self.cols + c] = Cell::default();
        }
        self.dirty[row] = true;
    }

    // ── Alternate screen ─────────────────────────────────────────────────────

    pub fn enter_alternate(&mut self) {
        if self.in_alt {
            return;
        }
        // Swap self with a fresh alternate grid; save current primary.
        let rows = self.rows;
        let cols = self.cols;
        let fresh = Box::new(TerminalGrid::new(rows, cols));
        let primary = std::mem::replace(self, *fresh);
        self.alternate = Some(Box::new(primary));
        self.in_alt = true;
    }

    pub fn leave_alternate(&mut self) {
        if !self.in_alt {
            return;
        }
        if let Some(primary) = self.alternate.take() {
            let rows = self.rows;
            let cols = self.cols;
            *self = *primary;
            // Resize primary to current terminal size if it changed while in alt.
            if self.rows != rows || self.cols != cols {
                self.resize(rows, cols);
            }
        }
        self.in_alt = false;
    }

    // ── Dirty tracking ───────────────────────────────────────────────────────

    pub fn mark_all_dirty(&mut self) {
        self.dirty.iter_mut().for_each(|d| *d = true);
    }

    pub fn clear_dirty(&mut self) {
        self.dirty.iter_mut().for_each(|d| *d = false);
    }
}
