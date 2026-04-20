use bitflags::bitflags;

bitflags! {
    #[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
    pub struct CellAttrs: u8 {
        const BOLD          = 0b0000_0001;
        const DIM           = 0b0000_0010;
        const ITALIC        = 0b0000_0100;
        const UNDERLINE     = 0b0000_1000;
        const BLINK         = 0b0001_0000;
        const REVERSE       = 0b0010_0000;
        const INVISIBLE     = 0b0100_0000;
        const STRIKETHROUGH = 0b1000_0000;
    }
}

/// Terminal foreground/background color.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Color {
    /// Use the terminal default (fg: white-ish, bg: transparent).
    #[default]
    Default,
    /// One of the 256 ANSI indexed colors.
    Indexed(u8),
    /// True-color RGB.
    Rgb(u8, u8, u8),
}

/// Whether a cell is part of a wide (e.g. CJK/emoji) character.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum WideKind {
    #[default]
    None,
    /// Left half of a wide glyph.
    Leading,
    /// Right half — stores `' '`; renderer skips this cell.
    Trailing,
}

/// A single terminal cell.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Cell {
    pub ch:    char,
    pub fg:    Color,
    pub bg:    Color,
    pub attrs: CellAttrs,
    pub wide:  WideKind,
}

impl Default for Cell {
    fn default() -> Self {
        Self {
            ch:    ' ',
            fg:    Color::Default,
            bg:    Color::Default,
            attrs: CellAttrs::empty(),
            wide:  WideKind::None,
        }
    }
}
