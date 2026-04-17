use arboard::Clipboard;
use termite_render::SelectionRange;
use termite_vt::{cell::CellAttrs, Color, TerminalGrid, WideKind};
use std::process::{Command, Stdio};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct SgrProps {
    fg: Color,
    bg: Color,
    attrs: CellAttrs,
}

fn cell_props(cell: &termite_vt::Cell) -> SgrProps {
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

/// Convert the current selection into ANSI SGR text so that pasting back into the terminal
/// preserves colors/styles (this is the "rich text" part).
pub fn selection_to_ansi_sgr_text(grid: &TerminalGrid, range: SelectionRange) -> String {
    if !range.active {
        return String::new();
    }

    let mut r = range;
    r.clamp_to_grid(grid.rows, grid.cols);

    let ((sr, sc), (er, ec)) = r.normalized_start_end();

    let mut out = String::new();
    out.push_str("\x1b[0m");

    let mut last: Option<SgrProps> = None;

    for row in sr..=er {
        let from_col = if row == sr { sc } else { 0 };
        let to_col = if row == er { ec } else { grid.cols.saturating_sub(1) };

        for col in from_col..=to_col {
            let cell = grid.cell(row, col);
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

pub fn clipboard_text_to_pty_bytes(text: &str) -> Vec<u8> {
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

