use multerm_vt::{cell::{CellAttrs, Color}, TerminalParser};

fn parser(rows: usize, cols: usize) -> TerminalParser {
    TerminalParser::new(rows, cols)
}

fn feed(p: &mut TerminalParser, s: &str) {
    p.process(s.as_bytes());
}

fn feed_raw(p: &mut TerminalParser, b: &[u8]) {
    p.process(b);
}

// ── Basic print + cursor ──────────────────────────────────────────────────────

#[test]
fn test_print_ascii() {
    let mut p = parser(24, 80);
    feed(&mut p, "Hello");
    assert_eq!(p.grid().cell(0, 0).ch, 'H');
    assert_eq!(p.grid().cell(0, 4).ch, 'o');
    assert_eq!(p.grid().cursor.col, 5);
}

#[test]
fn test_cr_lf() {
    let mut p = parser(24, 80);
    feed(&mut p, "AB\r\nCD");
    assert_eq!(p.grid().cell(0, 0).ch, 'A');
    assert_eq!(p.grid().cell(0, 1).ch, 'B');
    assert_eq!(p.grid().cursor.row, 1);
    assert_eq!(p.grid().cursor.col, 2);
    assert_eq!(p.grid().cell(1, 0).ch, 'C');
}

#[test]
fn test_backspace() {
    let mut p = parser(24, 80);
    feed(&mut p, "AB\x08C");
    // A, then B, then BS moves back to col 1, then C overwrites B
    assert_eq!(p.grid().cell(0, 0).ch, 'A');
    assert_eq!(p.grid().cell(0, 1).ch, 'C');
}

#[test]
fn test_tab() {
    let mut p = parser(24, 80);
    feed(&mut p, "A\tB");
    assert_eq!(p.grid().cell(0, 0).ch, 'A');
    assert_eq!(p.grid().cell(0, 8).ch, 'B');
}

// ── Cursor movement ───────────────────────────────────────────────────────────

#[test]
fn test_cursor_position() {
    let mut p = parser(24, 80);
    feed(&mut p, "\x1b[5;10H");
    assert_eq!(p.grid().cursor.row, 4);
    assert_eq!(p.grid().cursor.col, 9);
}

#[test]
fn test_cursor_up_down() {
    let mut p = parser(24, 80);
    feed(&mut p, "\x1b[10;1H"); // row 9
    feed(&mut p, "\x1b[3A");    // up 3 → row 6
    assert_eq!(p.grid().cursor.row, 6);
    feed(&mut p, "\x1b[2B");    // down 2 → row 8
    assert_eq!(p.grid().cursor.row, 8);
}

#[test]
fn test_cursor_forward_back() {
    let mut p = parser(24, 80);
    feed(&mut p, "\x1b[1;1H");
    feed(&mut p, "\x1b[5C");  // forward 5 → col 5
    assert_eq!(p.grid().cursor.col, 5);
    feed(&mut p, "\x1b[2D");  // back 2 → col 3
    assert_eq!(p.grid().cursor.col, 3);
}

// ── SGR colors ────────────────────────────────────────────────────────────────

#[test]
fn test_sgr_fg_ansi() {
    let mut p = parser(24, 80);
    feed(&mut p, "\x1b[31mR");
    // ANSI color 1 = red (index 1)
    assert_eq!(p.grid().cell(0, 0).fg, Color::Indexed(1));
}

#[test]
fn test_sgr_fg_rgb() {
    let mut p = parser(24, 80);
    feed(&mut p, "\x1b[38;2;255;128;0mX");
    assert_eq!(p.grid().cell(0, 0).fg, Color::Rgb(255, 128, 0));
}

#[test]
fn test_sgr_256() {
    let mut p = parser(24, 80);
    feed(&mut p, "\x1b[38;5;200mX");
    assert_eq!(p.grid().cell(0, 0).fg, Color::Indexed(200));
}

#[test]
fn test_sgr_reset() {
    let mut p = parser(24, 80);
    feed(&mut p, "\x1b[1;31mA\x1b[0mB");
    assert!(p.grid().cell(0, 0).attrs.contains(CellAttrs::BOLD));
    assert_eq!(p.grid().cell(0, 0).fg, Color::Indexed(1));
    assert!(!p.grid().cell(0, 1).attrs.contains(CellAttrs::BOLD));
    assert_eq!(p.grid().cell(0, 1).fg, Color::Default);
}

#[test]
fn test_sgr_bold_italic() {
    let mut p = parser(24, 80);
    feed(&mut p, "\x1b[1;3mX");
    let cell = p.grid().cell(0, 0);
    assert!(cell.attrs.contains(CellAttrs::BOLD));
    assert!(cell.attrs.contains(CellAttrs::ITALIC));
}

// ── Erase ─────────────────────────────────────────────────────────────────────

#[test]
fn test_erase_line_to_end() {
    let mut p = parser(24, 80);
    feed(&mut p, "ABCDE\x1b[1;3H\x1b[K"); // print 5 chars, move to col 3, erase to end
    assert_eq!(p.grid().cell(0, 0).ch, 'A');
    assert_eq!(p.grid().cell(0, 1).ch, 'B');
    assert_eq!(p.grid().cell(0, 2).ch, ' '); // col 3 (0-based 2) erased
    assert_eq!(p.grid().cell(0, 4).ch, ' '); // col 5 erased
}

#[test]
fn test_erase_screen() {
    let mut p = parser(24, 80);
    feed(&mut p, "Hello World\x1b[2J");
    assert_eq!(p.grid().cell(0, 0).ch, ' ');
    assert_eq!(p.grid().cell(0, 5).ch, ' ');
}

#[test]
fn test_erase_screen_clears_scrollback() {
    let mut p = parser(5, 10);
    for i in 0..5u8 {
        feed(&mut p, &format!("{}\r\n", i));
    }
    assert!(p.grid().scrollback_len() > 0);
    feed(&mut p, "\x1b[2J");
    assert_eq!(p.grid().scrollback_len(), 0);
    assert_eq!(p.grid().total_rows(), 5);
}

#[test]
fn test_erase_saved_lines_3j_clears_scrollback_only() {
    let mut p = parser(5, 10);
    for i in 0..5u8 {
        feed(&mut p, &format!("{}\r\n", i));
    }
    assert!(p.grid().scrollback_len() > 0);
    feed(&mut p, "\x1b[3J");
    assert_eq!(p.grid().scrollback_len(), 0);
    // Visible grid still shows last scrolled state (not cleared by 3 J alone).
    assert_eq!(p.grid().cell(0, 0).ch, '1');
}

// ── Scroll region ─────────────────────────────────────────────────────────────

#[test]
fn test_scroll_region() {
    let mut p = parser(10, 10);
    feed(&mut p, "\x1b[3;8r"); // scroll region rows 3-8 (1-based)
    assert_eq!(p.grid().scroll_top, 2);
    assert_eq!(p.grid().scroll_bot, 7);
}

#[test]
fn test_scroll_up_wraps() {
    let mut p = parser(5, 10);
    // Fill 5 rows
    for i in 0..5u8 {
        let s = format!("{}\r\n", i);
        feed(&mut p, &s);
    }
    // Row 0 should have been scrolled off, row 0 now = '1'
    assert_eq!(p.grid().cell(0, 0).ch, '1');
}

#[test]
fn test_scroll_up_preserves_primary_scrollback() {
    let mut p = parser(5, 10);
    for i in 0..5u8 {
        let s = format!("{}\r\n", i);
        feed(&mut p, &s);
    }
    assert_eq!(p.grid().scrollback_len(), 1);
    assert_eq!(p.grid().scrollback[0][0].ch, '0');
    assert_eq!(p.grid().total_rows(), 6);
}

// ── Alternate screen ──────────────────────────────────────────────────────────

#[test]
fn test_alternate_screen() {
    let mut p = parser(24, 80);
    feed(&mut p, "Main");
    feed(&mut p, "\x1b[?1049h"); // enter alt
    assert!(p.grid().in_alt);
    assert_eq!(p.grid().cell(0, 0).ch, ' '); // alt screen is clean
    feed(&mut p, "Alt");
    feed(&mut p, "\x1b[?1049l"); // leave alt
    assert!(!p.grid().in_alt);
    assert_eq!(p.grid().cell(0, 0).ch, 'M'); // back to main
}

// ── Resize ────────────────────────────────────────────────────────────────────

#[test]
fn test_resize() {
    let mut p = parser(24, 80);
    feed(&mut p, "Hello");
    p.resize(30, 100);
    assert_eq!(p.grid().rows, 30);
    assert_eq!(p.grid().cols, 100);
    assert_eq!(p.grid().cell(0, 0).ch, 'H');
}

// ── App cursor key mode ───────────────────────────────────────────────────────

#[test]
fn test_app_cursor_keys() {
    let mut p = parser(24, 80);
    assert!(!p.app_cursor_keys());
    feed(&mut p, "\x1b[?1h");
    assert!(p.app_cursor_keys());
    feed(&mut p, "\x1b[?1l");
    assert!(!p.app_cursor_keys());
}

// ── Delete / insert ───────────────────────────────────────────────────────────

#[test]
fn test_delete_chars() {
    let mut p = parser(24, 80);
    // Print ABCDE, move to 1-indexed (row=1,col=2) = 0-indexed col 1, delete 2 chars.
    // Deletes B and C; D and E shift left → A D E ' ' ' '
    feed(&mut p, "ABCDE\x1b[1;2H\x1b[2P");
    assert_eq!(p.grid().cell(0, 0).ch, 'A');
    assert_eq!(p.grid().cell(0, 1).ch, 'D');
    assert_eq!(p.grid().cell(0, 2).ch, 'E');
    assert_eq!(p.grid().cell(0, 3).ch, ' ');
}

#[test]
fn test_insert_delete_lines() {
    let mut p = parser(10, 10);
    feed(&mut p, "Line1\r\nLine2\r\nLine3");
    feed(&mut p, "\x1b[1;1H\x1b[L"); // insert line at row 0
    assert_eq!(p.grid().cell(0, 0).ch, ' '); // blank inserted line
    assert_eq!(p.grid().cell(1, 0).ch, 'L'); // Line1 moved down
}
