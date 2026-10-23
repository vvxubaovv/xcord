use std::cell::RefCell;
use std::ops::Deref;
use std::rc::Rc;

use xcord_vt::{default_palette, Color};

/// A complete color scheme for both the terminal grid and the shell.
#[derive(Clone, Debug)]
pub struct Theme {
    pub id: &'static str,
    pub name: &'static str,
    pub background: [u8; 3],
    pub foreground: [u8; 3],
    pub panel: [u8; 3],
    pub panel_alt: [u8; 3],
    pub border: [u8; 3],
    pub accent: [u8; 3],
    pub muted: [u8; 3],
    pub danger: [u8; 3],
    pub success: [u8; 3],
    pub cursor: [u8; 3],
    pub selection: [u8; 3],
    /// ANSI colors 0..15.
    pub palette: [[u8; 3]; 16],
}

impl Theme {
    /// Resolve a terminal color to RGB using this theme's palette.
    pub fn resolve(&self, color: Color) -> [u8; 3] {
        match color {
            Color::Default => self.foreground,
            Color::Rgb(r, g, b) => [r, g, b],
            Color::Indexed(index) => {
                if index < 16 {
                    self.palette[index as usize]
                } else {
                    let (r, g, b) = default_palette(index);
                    [r, g, b]
                }
            }
        }
    }

    pub fn fallback() -> Self {
        Self {
            id: "xcord-dark",
            name: "Xcord Dark",
            background: [0x10, 0x12, 0x18],
            foreground: [0xd8, 0xdc, 0xe8],
            panel: [0x15, 0x18, 0x21],
            panel_alt: [0x1b, 0x1f, 0x2a],
            border: [0x2a, 0x30, 0x3e],
            accent: [0x6c, 0xa8, 0xff],
            muted: [0x7d, 0x86, 0x9c],
            danger: [0xff, 0x6b, 0x6b],
            success: [0x5f, 0xd7, 0x9a],
            cursor: [0x9d, 0xc0, 0xff],
            selection: [0x31, 0x4a, 0x78],
            palette: [
                [0x00, 0x00, 0x00],
                [0xe0, 0x5b, 0x5b],
                [0x5f, 0xd7, 0x9a],
                [0xe5, 0xc0, 0x7b],
                [0x6c, 0xa8, 0xff],
                [0xc0, 0x8c, 0xff],
                [0x5f, 0xd4, 0xe0],
                [0xd8, 0xdc, 0xe8],
                [0x6b, 0x73, 0x88],
                [0xff, 0x8f, 0x8f],
                [0x8f, 0xf0, 0xb8],
                [0xff, 0xd9, 0x8a],
                [0x9d, 0xc0, 0xff],
                [0xdb, 0xb0, 0xff],
                [0x9d, 0xf0, 0xff],
                [0xff, 0xff, 0xff],
            ],
        }
    }
}

/// Theme registry + active selection.
pub struct ThemeService {
    themes: Vec<Rc<Theme>>,
    active: RefCell<usize>,
}

impl ThemeService {
    pub fn new(themes: Vec<Theme>, active: &str) -> Self {
        let themes: Vec<Rc<Theme>> = themes.into_iter().map(Rc::new).collect();
        let index = themes
            .iter()
            .position(|theme| theme.id == active)
            .unwrap_or(0);
        Self {
            themes,
            active: RefCell::new(index),
        }
    }

    pub fn themes(&self) -> Vec<Rc<Theme>> {
        self.themes.clone()
    }

    pub fn active(&self) -> Rc<Theme> {
        let index = *self.active.borrow();
        self.themes
            .get(index)
            .cloned()
            .unwrap_or_else(|| Rc::new(Theme::fallback()))
    }

    pub fn active_id(&self) -> &'static str {
        self.active().id
    }

    pub fn set_active(&self, id: &str) -> bool {
        if let Some(index) = self.themes.iter().position(|theme| theme.id == id) {
            *self.active.borrow_mut() = index;
            true
        } else {
            false
        }
    }

    pub fn cycle(&self) -> Rc<Theme> {
        if self.themes.is_empty() {
            return Rc::new(Theme::fallback());
        }
        let mut index = self.active.borrow_mut();
        *index = (*index + 1) % self.themes.len();
        self.themes[*index].clone()
    }
}

/// Service wrapper: `ctx.require::<Themes>(services::THEMES)`.
#[derive(Clone)]
pub struct Themes(pub Rc<ThemeService>);

impl Deref for Themes {
    type Target = ThemeService;
    fn deref(&self) -> &Self::Target {
        &self.0
    }
}
