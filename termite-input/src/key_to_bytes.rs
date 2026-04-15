use winit::event::KeyEvent;
use winit::keyboard::{Key, NamedKey, ModifiersState};

/// Translate a winit `KeyEvent` into the byte sequence that should be sent to
/// the PTY.  Returns `None` for keys that produce no output (modifier-only
/// presses, etc.).
///
/// `app_cursor` — true when the terminal has set application cursor-key mode
/// (DECCKM, `\x1b[?1h`).
pub fn key_to_bytes(event: &KeyEvent, mods: ModifiersState, app_cursor: bool) -> Option<Vec<u8>> {
    if !event.state.is_pressed() {
        return None;
    }

    let ctrl  = mods.control_key();
    let _shift = mods.shift_key();
    let _alt   = mods.alt_key();

    // ── Ctrl+letter → control codes ──────────────────────────────────────────
    if ctrl {
        if let Key::Character(s) = &event.logical_key {
            if let Some(c) = s.chars().next() {
                let lower = c.to_ascii_lowercase();
                if lower >= 'a' && lower <= 'z' {
                    return Some(vec![lower as u8 & 0x1F]);
                }
                // Ctrl+@ = NUL, Ctrl+[ = ESC, Ctrl+\ = FS, etc.
                match c {
                    '[' => return Some(vec![0x1B]),
                    '\\' => return Some(vec![0x1C]),
                    ']' => return Some(vec![0x1D]),
                    '^' | '6' => return Some(vec![0x1E]),
                    '_' | '-' => return Some(vec![0x1F]),
                    ' ' | '@' | '`' => return Some(vec![0x00]),
                    _ => {}
                }
            }
        }
    }

    // ── Named keys ────────────────────────────────────────────────────────────
    match &event.logical_key {
        Key::Named(named) => {
            Some(named_key_bytes(*named, mods, app_cursor))
        }
        Key::Character(s) => {
            // Encode the character(s) as UTF-8 bytes.
            let bytes: Vec<u8> = s.as_str().as_bytes().to_vec();
            if bytes.is_empty() { None } else { Some(bytes) }
        }
        _ => None,
    }
}

fn named_key_bytes(key: NamedKey, mods: ModifiersState, app_cursor: bool) -> Vec<u8> {
    let shift = mods.shift_key();
    let ctrl  = mods.control_key();

    // Modifier suffix for CSI sequences: ;2 shift, ;5 ctrl, ;6 ctrl+shift
    let mod_suffix: &str = match (ctrl, shift) {
        (true,  true)  => ";6",
        (true,  false) => ";5",
        (false, true)  => ";2",
        (false, false) => "",
    };

    macro_rules! cursor_seq {
        ($letter:expr) => {{
            if app_cursor {
                format!("\x1bO{}", $letter).into_bytes()
            } else if mod_suffix.is_empty() {
                format!("\x1b[{}", $letter).into_bytes()
            } else {
                format!("\x1b[1{}{}",  mod_suffix, $letter).into_bytes()
            }
        }};
    }

    match key {
        NamedKey::Enter        => b"\r".to_vec(),
        NamedKey::Backspace    => b"\x7f".to_vec(),
        NamedKey::Tab          => {
            if shift { b"\x1b[Z".to_vec() } else { b"\t".to_vec() }
        }
        NamedKey::Escape       => b"\x1b".to_vec(),
        NamedKey::Space        => b" ".to_vec(),

        NamedKey::ArrowUp      => cursor_seq!('A'),
        NamedKey::ArrowDown    => cursor_seq!('B'),
        NamedKey::ArrowRight   => cursor_seq!('C'),
        NamedKey::ArrowLeft    => cursor_seq!('D'),

        NamedKey::Home         => {
            if mod_suffix.is_empty() { b"\x1b[H".to_vec() }
            else { format!("\x1b[1{}H", mod_suffix).into_bytes() }
        }
        NamedKey::End          => {
            if mod_suffix.is_empty() { b"\x1b[F".to_vec() }
            else { format!("\x1b[1{}F", mod_suffix).into_bytes() }
        }
        NamedKey::PageUp       => {
            if mod_suffix.is_empty() { b"\x1b[5~".to_vec() }
            else { format!("\x1b[5{}~", mod_suffix).into_bytes() }
        }
        NamedKey::PageDown     => {
            if mod_suffix.is_empty() { b"\x1b[6~".to_vec() }
            else { format!("\x1b[6{}~", mod_suffix).into_bytes() }
        }
        NamedKey::Insert       => {
            if mod_suffix.is_empty() { b"\x1b[2~".to_vec() }
            else { format!("\x1b[2{}~", mod_suffix).into_bytes() }
        }
        NamedKey::Delete       => {
            if mod_suffix.is_empty() { b"\x1b[3~".to_vec() }
            else { format!("\x1b[3{}~", mod_suffix).into_bytes() }
        }

        // F-keys
        NamedKey::F1           => b"\x1bOP".to_vec(),
        NamedKey::F2           => b"\x1bOQ".to_vec(),
        NamedKey::F3           => b"\x1bOR".to_vec(),
        NamedKey::F4           => b"\x1bOS".to_vec(),
        NamedKey::F5           => b"\x1b[15~".to_vec(),
        NamedKey::F6           => b"\x1b[17~".to_vec(),
        NamedKey::F7           => b"\x1b[18~".to_vec(),
        NamedKey::F8           => b"\x1b[19~".to_vec(),
        NamedKey::F9           => b"\x1b[20~".to_vec(),
        NamedKey::F10          => b"\x1b[21~".to_vec(),
        NamedKey::F11          => b"\x1b[23~".to_vec(),
        NamedKey::F12          => b"\x1b[24~".to_vec(),

        _ => vec![],
    }
}
