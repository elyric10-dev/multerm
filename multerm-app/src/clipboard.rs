use arboard::Clipboard;
use multerm_render::SelectionRange;
use multerm_vt::{cell::CellAttrs, Color, TerminalGrid, WideKind};
use std::process::{Command, Stdio};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct SgrProps {
    fg: Color,
    bg: Color,
    attrs: CellAttrs,
}

fn cell_props(cell: &multerm_vt::Cell) -> SgrProps {
    SgrProps {
        fg: cell.fg,
        bg: cell.bg,
        attrs: cell.attrs,
    }
}

fn push_sgr_for_props(out: &mut String, props: SgrProps) {
    let mut codes: Vec<String> = Vec::new();

    // Text attributes
    if props.attrs.contains(CellAttrs::BOLD) {
        codes.push("1".into());
    }
    if props.attrs.contains(CellAttrs::DIM) {
        codes.push("2".into());
    }
    if props.attrs.contains(CellAttrs::ITALIC) {
        codes.push("3".into());
    }
    if props.attrs.contains(CellAttrs::UNDERLINE) {
        codes.push("4".into());
    }
    if props.attrs.contains(CellAttrs::BLINK) {
        codes.push("5".into());
    }
    if props.attrs.contains(CellAttrs::REVERSE) {
        codes.push("7".into());
    }
    if props.attrs.contains(CellAttrs::INVISIBLE) {
        codes.push("8".into());
    }
    if props.attrs.contains(CellAttrs::STRIKETHROUGH) {
        codes.push("9".into());
    }

    // Foreground
    match props.fg {
        Color::Default => codes.push("39".into()),
        Color::Indexed(i) => {
            codes.push("38".into());
            codes.push("5".into());
            codes.push(i.to_string());
        }
        Color::Rgb(r, g, b) => {
            codes.push("38".into());
            codes.push("2".into());
            codes.push(r.to_string());
            codes.push(g.to_string());
            codes.push(b.to_string());
        }
    }

    // Background
    match props.bg {
        Color::Default => codes.push("49".into()),
        Color::Indexed(i) => {
            codes.push("48".into());
            codes.push("5".into());
            codes.push(i.to_string());
        }
        Color::Rgb(r, g, b) => {
            codes.push("48".into());
            codes.push("2".into());
            codes.push(r.to_string());
            codes.push(g.to_string());
            codes.push(b.to_string());
        }
    }

    // Apply if we have at least one code.
    if !codes.is_empty() {
        out.push_str("\x1b[");
        out.push_str(&codes.join(";"));
        out.push('m');
    }
}

/// Upper bound on how many rows Cmd+A can pull into an "input" block (stops whole-TUI grabs).
const SELECT_INPUT_BLOCK_MAX_LINES: usize = 64;

/// Select-all target: the current **input block** around the VT cursor — not full scrollback.
///
/// The block is the cursor row plus any **live-buffer** rows above it that are still
/// non-empty (no blank line gap), capped at [`SELECT_INPUT_BLOCK_MAX_LINES`]. Scrollback
/// is never included so history above the visible screen is not selected.
pub fn selection_range_select_input_block(grid: &TerminalGrid) -> Option<SelectionRange> {
    if grid.rows == 0 || grid.cols == 0 {
        return None;
    }
    let total = grid.total_rows();
    if total == 0 {
        return None;
    }
    let v_cursor = grid.scrollback_len() + grid.cursor.row.min(grid.rows.saturating_sub(1));
    let v_cursor = v_cursor.min(total.saturating_sub(1));

    let live_top = grid.scrollback_len();
    let mut start_row = v_cursor;
    let mut lines = 1usize;
    while start_row > live_top && lines < SELECT_INPUT_BLOCK_MAX_LINES {
        let prev = start_row - 1;
        if grid.virtual_row_render_end(prev) == 0 {
            break;
        }
        start_row = prev;
        lines += 1;
    }

    let end_re = grid.virtual_row_render_end(v_cursor);
    let end_col = if end_re == 0 {
        0
    } else {
        end_re.saturating_sub(1).min(grid.cols.saturating_sub(1))
    };

    Some(SelectionRange {
        start_row,
        start_col: 0,
        end_row: v_cursor,
        end_col,
        active: true,
    })
}

/// Convert the current selection into ANSI SGR text so that pasting back into the terminal
/// preserves colors/styles (this is the "rich text" part).
pub fn selection_to_ansi_sgr_text(grid: &TerminalGrid, range: SelectionRange) -> String {
    if !range.active {
        return String::new();
    }

    let mut r = range;
    let total_rows = grid.total_rows();
    r.clamp_to_grid(total_rows, grid.cols);

    let ((sr, sc), (er, ec)) = r.normalized_start_end();

    let mut out = String::new();
    out.push_str("\x1b[0m");

    let mut last: Option<SgrProps> = None;

    for row in sr..=er {
        let from_col = if row == sr { sc } else { 0 };
        let to_col = if row == er {
            ec
        } else {
            grid.cols.saturating_sub(1)
        };

        for col in from_col..=to_col {
            let cell = grid.virtual_cell(row, col);
            if cell.wide == WideKind::Trailing {
                continue;
            }

            let props = cell_props(cell);
            if last != Some(props) {
                out.push_str("\x1b[0m");
                push_sgr_for_props(&mut out, props);
                last = Some(props);
            }

            out.push(cell.ch);
        }

        if row != er {
            out.push('\n');
        }
    }

    out.push_str("\x1b[0m");
    out
}

/// Plain UTF-8 from a grid selection (no ANSI), for pasting back into the shell or other apps.
pub fn selection_to_plain_text(grid: &TerminalGrid, range: SelectionRange) -> String {
    if !range.active {
        return String::new();
    }

    let mut r = range;
    let total_rows = grid.total_rows();
    r.clamp_to_grid(total_rows, grid.cols);

    let ((sr, sc), (er, ec)) = r.normalized_start_end();

    let mut out = String::new();
    for row in sr..=er {
        let from_col = if row == sr { sc } else { 0 };
        let to_col = if row == er {
            ec
        } else {
            grid.cols.saturating_sub(1)
        };

        for col in from_col..=to_col {
            let cell = grid.virtual_cell(row, col);
            if cell.wide == WideKind::Trailing {
                continue;
            }
            out.push(cell.ch);
        }

        if row != er {
            out.push('\n');
        }
    }
    out
}

/// True if the trimmed line is only "ruler" / frame characters (no letters/digits).
fn is_separator_only_line(t: &str) -> bool {
    !t.is_empty()
        && t.chars().all(|c| {
            c.is_whitespace()
                || matches!(
                    c,
                    '-' | '=' | '_' | '·' | '•' | '*' | '~' | '─' | '━' | '═' | '│' | '┃'
                )
                || ('\u{2500}'..='\u{257F}').contains(&c)
        })
}

/// Strip common TUI / Claude-style prompt prefix on the first content line.
fn strip_shell_line_prompt(line: &str) -> String {
    let no_trail = line.trim_end();
    let first_non_ws = no_trail
        .char_indices()
        .find(|(_, c)| !c.is_whitespace())
        .map(|(i, _)| i)
        .unwrap_or(no_trail.len());
    let (indent, body) = no_trail.split_at(first_non_ws);
    let body_trim = body.trim_start();
    const PREFIXES: &[&str] = &[
        "❯ ", "❯", "> ", ">", "$ ", "$", "# ", "#", "● ", "●", "» ", "»", "▶ ", "▶", "λ ", "λ",
    ];
    let mut rest = body_trim;
    for p in PREFIXES {
        if let Some(r) = rest.strip_prefix(p) {
            rest = r.trim_start();
            break;
        }
    }
    format!("{indent}{rest}").trim_end().to_string()
}

/// Remove decorative lines, prompt glyphs, continuation indent, and trailing spaces from
/// text pasted back into the terminal (e.g. after copy from Claude Code).
pub fn sanitize_pasted_terminal_text(s: &str) -> String {
    let normalized = s.replace("\r\n", "\n").replace('\r', "\n");
    let mut lines: Vec<String> = normalized.lines().map(String::from).collect();

    while let Some(f) = lines.first() {
        let t = f.trim();
        if t.is_empty() {
            lines.remove(0);
            continue;
        }
        if is_separator_only_line(t) {
            lines.remove(0);
            continue;
        }
        break;
    }
    while let Some(l) = lines.last() {
        let t = l.trim();
        if t.is_empty() {
            lines.pop();
            continue;
        }
        if is_separator_only_line(t) {
            lines.pop();
            continue;
        }
        break;
    }

    if lines.is_empty() {
        return String::new();
    }

    lines[0] = strip_shell_line_prompt(&lines[0]);

    if lines.len() > 1 {
        let nonempty: Vec<&String> = lines[1..].iter().filter(|l| !l.trim().is_empty()).collect();
        if !nonempty.is_empty() {
            let min_sp = nonempty
                .iter()
                .map(|l| l.chars().take_while(|&c| c == ' ').count())
                .min()
                .unwrap_or(0);
            if min_sp > 0 {
                for line in lines.iter_mut().skip(1) {
                    let n = line.chars().take_while(|&c| c == ' ').count().min(min_sp);
                    *line = line.chars().skip(n).collect();
                }
            }
        }
    }

    for line in &mut lines {
        *line = line.trim_end().to_string();
    }

    lines.join("\n")
}

pub fn clipboard_text_to_pty_bytes(text: &str) -> Vec<u8> {
    let text = sanitize_pasted_terminal_text(text);
    let mut bytes = Vec::with_capacity(text.len());
    for ch in text.chars() {
        match ch {
            // Many programs expect "Enter" semantics when pasting.
            '\n' => {
                // Move to beginning-of-line then down.
                bytes.push(b'\r');
                bytes.push(b'\n');
            }
            '\r' => bytes.push(b'\r'),
            _ => {
                let mut buf = [0u8; 4];
                let s = ch.encode_utf8(&mut buf);
                bytes.extend_from_slice(s.as_bytes());
            }
        }
    }
    bytes
}

pub fn set_clipboard_text(text: &str) -> Result<(), anyhow::Error> {
    // Primary: arboard (cross-platform).
    if let Ok(mut clipboard) = Clipboard::new() {
        if clipboard.set_text(text.to_string()).is_ok() {
            return Ok(());
        }
    }

    // Fallback on macOS: pbcopy/pbpaste.
    #[cfg(target_os = "macos")]
    {
        let mut child = Command::new("pbcopy")
            .stdin(Stdio::piped())
            .spawn()
            .map_err(|e| anyhow::anyhow!("pbcopy spawn failed: {e}"))?;
        {
            let stdin = child
                .stdin
                .as_mut()
                .ok_or_else(|| anyhow::anyhow!("pbcopy stdin unavailable"))?;
            use std::io::Write;
            stdin.write_all(text.as_bytes())?;
        }
        let status = child.wait()?;
        if status.success() {
            Ok(())
        } else {
            anyhow::bail!("pbcopy failed: {status}");
        }
    }

    #[cfg(not(target_os = "macos"))]
    {
        anyhow::bail!("clipboard set failed (arboard unavailable and no pbcopy fallback)");
    }
}

pub fn get_clipboard_text() -> Result<String, anyhow::Error> {
    // Primary: arboard (cross-platform).
    if let Ok(mut clipboard) = Clipboard::new() {
        if let Ok(t) = clipboard.get_text() {
            return Ok(t);
        }
    }

    // Fallback on macOS: pbpaste.
    #[cfg(target_os = "macos")]
    {
        let out = Command::new("pbpaste")
            .output()
            .map_err(|e| anyhow::anyhow!("pbpaste failed: {e}"))?;
        if !out.status.success() {
            anyhow::bail!("pbpaste non-zero exit: {}", out.status);
        }
        let s = String::from_utf8(out.stdout)?;
        Ok(s)
    }

    #[cfg(not(target_os = "macos"))]
    {
        anyhow::bail!("clipboard read failed (arboard unavailable and no pbpaste fallback)");
    }
}
