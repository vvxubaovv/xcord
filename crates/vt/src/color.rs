//! Terminal colors and attributes.

use serde::{Deserialize, Serialize};

/// A terminal color, independent of the active theme.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum Color {
    #[default]
    Default,
    Indexed(u8),
    Rgb(u8, u8, u8),
}

impl Color {
    pub const BLACK: Color = Color::Indexed(0);
    pub const RED: Color = Color::Indexed(1);
    pub const GREEN: Color = Color::Indexed(2);
    pub const YELLOW: Color = Color::Indexed(3);
    pub const BLUE: Color = Color::Indexed(4);
    pub const MAGENTA: Color = Color::Indexed(5);
    pub const CYAN: Color = Color::Indexed(6);
    pub const WHITE: Color = Color::Indexed(7);

    pub fn indexed(index: u8) -> Self {
        Color::Indexed(index)
    }
}

/// The default xterm 256-color palette. Themes can override it; the low 16
/// colors follow the familiar xterm defaults.
pub fn default_palette(index: u8) -> (u8, u8, u8) {
    const ANSI16: [(u8, u8, u8); 16] = [
        (0x00, 0x00, 0x00),
        (0xcd, 0x00, 0x00),
        (0x00, 0xcd, 0x00),
        (0xcd, 0xcd, 0x00),
        (0x00, 0x00, 0xee),
        (0xcd, 0x00, 0xcd),
        (0x00, 0xcd, 0xcd),
        (0xe5, 0xe5, 0xe5),
        (0x7f, 0x7f, 0x7f),
        (0xff, 0x00, 0x00),
        (0x00, 0xff, 0x00),
        (0xff, 0xff, 0x00),
        (0x5c, 0x5c, 0xff),
        (0xff, 0x00, 0xff),
        (0x00, 0xff, 0xff),
        (0xff, 0xff, 0xff),
    ];
    if index < 16 {
        return ANSI16[index as usize];
    }
    if index >= 232 {
        let v = 8 + (index - 232) * 10;
        return (v, v, v);
    }
    let index = index - 16;
    let r = index / 36;
    let g = (index % 36) / 6;
    let b = index % 6;
    let level = |v: u8| if v == 0 { 0 } else { 55 + v * 40 };
    (level(r), level(g), level(b))
}

/// Stylistic flags for a cell.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub struct Flags(u16);

impl Flags {
    pub const BOLD: Flags = Flags(1 << 0);
    pub const DIM: Flags = Flags(1 << 1);
    pub const ITALIC: Flags = Flags(1 << 2);
    pub const UNDERLINE: Flags = Flags(1 << 3);
    pub const BLINK: Flags = Flags(1 << 4);
    pub const REVERSE: Flags = Flags(1 << 5);
    pub const HIDDEN: Flags = Flags(1 << 6);
    pub const STRIKETHROUGH: Flags = Flags(1 << 7);
    pub const DOUBLE_UNDERLINE: Flags = Flags(1 << 8);
    pub const CURLY_UNDERLINE: Flags = Flags(1 << 9);

    pub const fn empty() -> Self {
        Flags(0)
    }

    pub const fn contains(self, other: Flags) -> bool {
        self.0 & other.0 == other.0
    }

    pub fn insert(&mut self, other: Flags) {
        self.0 |= other.0;
    }

    pub fn remove(&mut self, other: Flags) {
        self.0 &= !other.0;
    }

    pub fn set(&mut self, other: Flags, enabled: bool) {
        if enabled {
            self.insert(other);
        } else {
            self.remove(other);
        }
    }

    pub const fn is_empty(self) -> bool {
        self.0 == 0
    }

    pub const fn bits(self) -> u16 {
        self.0
    }
}

/// Visual attributes of a cell.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub struct Attrs {
    pub fg: Color,
    pub bg: Color,
    pub flags: Flags,
    pub underline_color: Color,
}

impl Attrs {
    pub fn is_default(&self) -> bool {
        self.fg == Color::Default
            && self.bg == Color::Default
            && self.flags.is_empty()
            && self.underline_color == Color::Default
    }

    pub fn reversed(&self) -> bool {
        self.flags.contains(Flags::REVERSE)
    }
}
