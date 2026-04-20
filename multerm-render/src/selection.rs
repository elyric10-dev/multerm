/// Terminal selection range, in grid coordinates.
///
/// The selection is treated as a contiguous range in row-major (reading) order:
/// all cells between the start and end (inclusive) are selected.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct SelectionRange {
    pub start_row: usize,
    pub start_col: usize,
    pub end_row: usize,
    pub end_col: usize,
    /// When `false`, the range is ignored.
    pub active: bool,
}

impl SelectionRange {
    /// Clamp start/end to `[0, rows)` / `[0, cols)` bounds.
    pub fn clamp_to_grid(&mut self, rows: usize, cols: usize) {
        let rows = rows.max(1);
        let cols = cols.max(1);
        self.start_row = self.start_row.min(rows - 1);
        self.end_row = self.end_row.min(rows - 1);
        self.start_col = self.start_col.min(cols - 1);
        self.end_col = self.end_col.min(cols - 1);
    }

    /// Normalize the selection so `(sr, sc) <= (er, ec)` in row-major order.
    pub fn normalized_start_end(&self) -> ((usize, usize), (usize, usize)) {
        let a = (self.start_row, self.start_col);
        let b = (self.end_row, self.end_col);
        if a <= b {
            (a, b)
        } else {
            (b, a)
        }
    }

    /// Check whether a cell is part of the selection.
    ///
    /// `rows`/`cols` are provided so callers don't need to rely on grid state.
    pub fn contains(&self, row: usize, col: usize, rows: usize, cols: usize) -> bool {
        if !self.active {
            return false;
        }
        if row >= rows || col >= cols {
            return false;
        }

        let ((sr, sc), (er, ec)) = self.normalized_start_end();
        if sr == er {
            let (min_c, max_c) = if sc <= ec { (sc, ec) } else { (ec, sc) };
            return row == sr && col >= min_c && col <= max_c;
        }

        if row == sr {
            col >= sc
        } else if row == er {
            col <= ec
        } else {
            row > sr && row < er
        }
    }
}

#[cfg(test)]
mod tests {
    use super::SelectionRange;

    #[test]
    fn contains_single_row_only_matches_that_row() {
        let sel = SelectionRange {
            start_row: 2,
            start_col: 5,
            end_row: 2,
            end_col: 9,
            active: true,
        };
        assert!(sel.contains(2, 7, 10, 80));
        assert!(!sel.contains(0, 7, 10, 80));
        assert!(!sel.contains(3, 7, 10, 80));
        assert!(!sel.contains(2, 4, 10, 80));
        assert!(!sel.contains(2, 10, 10, 80));
    }
}
