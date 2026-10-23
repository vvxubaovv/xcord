use crate::color::Attrs;

/// One terminal cell. `width` is 0 for the trailing half of a wide character,
/// 1 for normal characters, and 2 for the leading half of a wide character.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Cell {
    pub ch: char,
    pub combining: Option<Box<str>>,
    pub attrs: Attrs,
    pub width: u8,
}

impl Cell {
    pub fn blank(attrs: Attrs) -> Self {
        Self {
            ch: ' ',
            combining: None,
            attrs,
            width: 1,
        }
    }

    pub fn new(ch: char, width: u8, attrs: Attrs) -> Self {
        Self {
            ch,
            combining: None,
            attrs,
            width,
        }
    }

    pub fn push_combining(&mut self, ch: char) {
        match &mut self.combining {
            Some(text) => {
                let mut owned = text.to_string();
                owned.push(ch);
                *text = owned.into_boxed_str();
            }
            None => self.combining = Some(ch.to_string().into_boxed_str()),
        }
    }

    pub fn is_blank(&self) -> bool {
        self.ch == ' ' && self.combining.is_none() && self.attrs.is_default()
    }

    pub fn text(&self) -> String {
        let mut out = String::new();
        self.write_text(&mut out);
        out
    }

    pub fn write_text(&self, out: &mut String) {
        if self.width == 0 {
            return;
        }
        out.push(self.ch);
        if let Some(combining) = &self.combining {
            out.push_str(combining);
        }
    }

    pub fn reset_wide_half(&mut self) {
        *self = Cell::blank(self.attrs);
    }
}

/// A row of cells.
#[derive(Clone, Debug)]
pub struct Row {
    pub cells: Vec<Cell>,
    /// Whether the terminal automatically wrapped out of this row.
    pub wrapped: bool,
}

impl Row {
    pub fn blank(cols: usize, attrs: Attrs) -> Self {
        Self {
            cells: vec![Cell::blank(attrs); cols],
            wrapped: false,
        }
    }

    pub fn resize(&mut self, cols: usize, attrs: Attrs) {
        if self.cells.len() > cols {
            self.cells.truncate(cols);
            if self
                .cells
                .last()
                .map(|cell| cell.width == 2)
                .unwrap_or(false)
            {
                if let Some(last) = self.cells.last_mut() {
                    *last = Cell::blank(attrs);
                }
            }
        } else {
            self.cells.resize(cols, Cell::blank(attrs));
        }
    }

    pub fn text(&self) -> String {
        let mut out = String::new();
        for cell in &self.cells {
            cell.write_text(&mut out);
        }
        out
    }

    pub fn trimmed_text(&self) -> String {
        self.text().trim_end().to_string()
    }
}
