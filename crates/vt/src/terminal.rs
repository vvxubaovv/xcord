//! A compact, correct-enough VT100/xterm terminal emulator.
//!
//! The emulator keeps a screen, a scrollback history, and enough xterm state
//! (alternate screen, scroll regions, SGR, bracketed paste, mouse modes,
//! cursor shape) to feel native in an SSH session. It intentionally does not
//! reflow wrapped lines on resize yet; that is a documented trade-off.

use std::collections::VecDeque;

use unicode_width::UnicodeWidthChar;
use vte::{Params, Perform};

use crate::cell::{Cell, Row};
use crate::color::{Attrs, Color, Flags};

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum MouseMode {
    #[default]
    None,
    Click,
    Drag,
    Any,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum MouseEncoding {
    #[default]
    X10,
    Sgr,
}

/// Modes that affect input encoding and how the UI should behave.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Modes {
    pub autowrap: bool,
    pub origin: bool,
    pub insert: bool,
    pub cursor_visible: bool,
    pub application_cursor: bool,
    pub application_keypad: bool,
    pub bracketed_paste: bool,
    pub focus_events: bool,
    pub mouse_mode: MouseMode,
    pub mouse_encoding: MouseEncoding,
    pub alt_screen: bool,
}

impl Default for Modes {
    fn default() -> Self {
        Self {
            autowrap: true,
            origin: false,
            insert: false,
            cursor_visible: true,
            application_cursor: false,
            application_keypad: false,
            bracketed_paste: false,
            focus_events: false,
            mouse_mode: MouseMode::None,
            mouse_encoding: MouseEncoding::X10,
            alt_screen: false,
        }
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum CursorStyle {
    #[default]
    Block,
    Bar,
    Underline,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Cursor {
    pub row: usize,
    pub col: usize,
    pub style: CursorStyle,
    pub visible: bool,
    pub blinking: bool,
}

#[derive(Clone, Debug)]
pub struct SnapshotRow {
    pub cells: Vec<Cell>,
    pub wrapped: bool,
    /// Row text without trailing blanks, ready for highlighting/search.
    pub text: String,
}

#[derive(Clone, Debug)]
pub struct Snapshot {
    pub cols: usize,
    pub rows: usize,
    pub lines: Vec<SnapshotRow>,
    /// Cursor as a viewport coordinate; only set when scrolled to the bottom.
    pub cursor: Option<(usize, usize)>,
    pub cursor_style: CursorStyle,
    pub cursor_blinking: bool,
    pub title: String,
    pub alt_screen: bool,
    pub scroll_offset: usize,
    pub scrollback_len: usize,
    pub version: u64,
    pub modes: Modes,
}

impl Snapshot {
    pub fn line_text(&self, row: usize) -> &str {
        self.lines
            .get(row)
            .map(|line| line.text.as_str())
            .unwrap_or("")
    }

    /// Plain text of the viewport, preserving wrapped rows.
    pub fn text(&self) -> String {
        let mut out = String::new();
        for (index, line) in self.lines.iter().enumerate() {
            out.push_str(&line.text);
            if index + 1 < self.lines.len() && !line.wrapped {
                out.push('\n');
            }
        }
        out
    }
}

struct SavedScreen {
    lines: Vec<Row>,
    cursor_row: usize,
    cursor_col: usize,
    scroll_top: usize,
    scroll_bottom: usize,
}

/// A terminal emulator instance.
pub struct Terminal {
    cols: usize,
    rows: usize,
    screen: Vec<Row>,
    history: VecDeque<Row>,
    scrollback_limit: usize,
    cursor: Cursor,
    scroll_top: usize,
    scroll_bottom: usize,
    tabs: Vec<bool>,
    attrs: Attrs,
    modes: Modes,
    saved_cursor: Option<(usize, usize, Attrs)>,
    alt_saved_cursor: Option<(usize, usize, Attrs)>,
    main_screen: Option<SavedScreen>,
    alt_screen: bool,
    wrap_pending: bool,
    last_printed: Option<char>,
    parser: vte::Parser,
    title: String,
    responses: Vec<Vec<u8>>,
    clipboard: Option<String>,
    bell_count: u64,
    version: u64,
}

impl Terminal {
    pub fn new(cols: usize, rows: usize, scrollback: usize) -> Self {
        let cols = cols.max(1);
        let rows = rows.max(1);
        let attrs = Attrs::default();
        let mut tabs = vec![false; cols];
        for (index, tab) in tabs.iter_mut().enumerate() {
            *tab = index % 8 == 0;
        }
        Self {
            cols,
            rows,
            screen: vec![Row::blank(cols, attrs); rows],
            history: VecDeque::new(),
            scrollback_limit: scrollback,
            cursor: Cursor {
                row: 0,
                col: 0,
                style: CursorStyle::Block,
                visible: true,
                blinking: true,
            },
            scroll_top: 0,
            scroll_bottom: rows - 1,
            tabs,
            attrs,
            modes: Modes::default(),
            saved_cursor: None,
            alt_saved_cursor: None,
            main_screen: None,
            alt_screen: false,
            wrap_pending: false,
            last_printed: None,
            parser: vte::Parser::new(),
            title: String::new(),
            responses: Vec::new(),
            clipboard: None,
            bell_count: 0,
            version: 1,
        }
    }

    pub fn feed(&mut self, bytes: &[u8]) {
        let mut parser = std::mem::replace(&mut self.parser, vte::Parser::new());
        parser.advance(self, bytes);
        self.parser = parser;
        self.version = self.version.wrapping_add(1);
    }

    pub fn resize(&mut self, cols: usize, rows: usize) {
        let cols = cols.max(1);
        let rows = rows.max(1);
        if cols == self.cols && rows == self.rows {
            return;
        }

        for line in &mut self.screen {
            line.resize(cols, self.attrs);
        }

        while self.screen.len() > rows {
            if self.cursor.row >= rows && !self.screen.is_empty() {
                let line = self.screen.remove(0);
                if !self.alt_screen {
                    self.push_history(line);
                }
                self.cursor.row = self.cursor.row.saturating_sub(1);
                self.scroll_top = self.scroll_top.saturating_sub(1);
                self.scroll_bottom = self.scroll_bottom.saturating_sub(1);
            } else {
                self.screen.pop();
            }
        }
        while self.screen.len() < rows {
            self.screen.push(Row::blank(cols, self.attrs));
        }

        self.cols = cols;
        self.rows = rows;
        self.scroll_top = 0;
        self.scroll_bottom = rows - 1;
        self.cursor.row = self.cursor.row.min(rows - 1);
        self.cursor.col = self.cursor.col.min(cols - 1);
        self.wrap_pending = false;

        self.tabs.resize(cols, false);
        for (index, tab) in self.tabs.iter_mut().enumerate() {
            if *tab && index % 8 != 0 {
                *tab = false;
            }
        }
        for index in (0..cols).step_by(8) {
            self.tabs[index] = true;
        }
        self.version = self.version.wrapping_add(1);
    }

    /// Plain text of the scrollback plus the current screen.
    pub fn history_text(&self) -> String {
        let mut out = String::new();
        for row in &self.history {
            out.push_str(&row.trimmed_text());
            out.push('\n');
        }
        for row in &self.screen {
            out.push_str(&row.trimmed_text());
            out.push('\n');
        }
        out
    }

    pub fn snapshot(&self, scroll_offset: usize) -> Snapshot {
        let scroll_offset = if self.alt_screen {
            0
        } else {
            scroll_offset.min(self.history.len())
        };
        let start = self.viewport_start(scroll_offset);
        let mut lines = Vec::with_capacity(self.rows);

        for index in 0..self.rows {
            let absolute = start + index;
            let row = if self.alt_screen {
                self.screen.get(index)
            } else {
                self.row_at(absolute)
            };
            if let Some(row) = row {
                lines.push(SnapshotRow {
                    cells: row.cells.clone(),
                    wrapped: row.wrapped,
                    text: row.trimmed_text(),
                });
            } else {
                lines.push(SnapshotRow {
                    cells: vec![Cell::blank(self.attrs); self.cols],
                    wrapped: false,
                    text: String::new(),
                });
            }
        }

        let cursor = (scroll_offset == 0 && self.cursor.visible)
            .then_some((self.cursor.row, self.cursor.col));

        Snapshot {
            cols: self.cols,
            rows: self.rows,
            lines,
            cursor,
            cursor_style: self.cursor.style,
            cursor_blinking: self.cursor.blinking,
            title: self.title.clone(),
            alt_screen: self.alt_screen,
            scroll_offset,
            scrollback_len: self.history.len(),
            version: self.version,
            modes: self.modes,
        }
    }

    pub fn title(&self) -> &str {
        &self.title
    }

    pub fn version(&self) -> u64 {
        self.version
    }

    pub fn modes(&self) -> Modes {
        self.modes
    }

    pub fn cursor(&self) -> Cursor {
        self.cursor
    }

    pub fn size(&self) -> (usize, usize) {
        (self.cols, self.rows)
    }

    pub fn scrollback_len(&self) -> usize {
        self.history.len()
    }

    pub fn bell_count(&self) -> u64 {
        self.bell_count
    }

    pub fn take_responses(&mut self) -> Vec<Vec<u8>> {
        std::mem::take(&mut self.responses)
    }

    pub fn take_clipboard(&mut self) -> Option<String> {
        self.clipboard.take()
    }

    pub fn set_scrollback_limit(&mut self, lines: usize) {
        self.scrollback_limit = lines;
        while self.history.len() > self.scrollback_limit {
            self.history.pop_front();
        }
    }

    pub fn history_len(&self) -> usize {
        self.history.len()
    }

    /// Plain text of `count` scrollback lines above the live screen.
    pub fn scrollback_text(&self, count: usize) -> String {
        let mut out = String::new();
        let start = self.history.len().saturating_sub(count);
        for row in self.history.iter().skip(start) {
            out.push_str(&row.trimmed_text());
            out.push('\n');
        }
        out
    }

    /// Text between two viewport coordinates (inclusive start, exclusive end).
    /// Coordinates are clamped to the viewport.
    pub fn text_in_viewport(
        &self,
        start: (usize, usize),
        end: (usize, usize),
        scroll_offset: usize,
    ) -> String {
        let (start, end) = normalize(start, end);
        let start_abs = self.viewport_start(if self.alt_screen {
            0
        } else {
            scroll_offset.min(self.history.len())
        });

        let mut out = String::new();
        for viewport_row in start.0..=end.0.min(self.rows.saturating_sub(1)) {
            let row = if self.alt_screen {
                self.screen.get(viewport_row)
            } else {
                self.row_at(start_abs + viewport_row)
            };
            let Some(row) = row else {
                continue;
            };
            let from = if viewport_row == start.0 {
                start.1.min(self.cols)
            } else {
                0
            };
            let to = if viewport_row == end.0 {
                end.1.min(self.cols)
            } else {
                self.cols
            };
            let mut line = String::new();
            for col in from..to {
                if let Some(cell) = row.cells.get(col) {
                    cell.write_text(&mut line);
                }
            }
            out.push_str(line.trim_end());
            if viewport_row != end.0 && !row.wrapped {
                out.push('\n');
            }
        }
        out
    }

    pub fn screen_text(&self) -> String {
        let mut out = String::new();
        for row in &self.screen {
            out.push_str(&row.trimmed_text());
            out.push('\n');
        }
        out
    }

    fn viewport_start(&self, scroll_offset: usize) -> usize {
        let total = if self.alt_screen {
            self.screen.len()
        } else {
            self.history.len() + self.screen.len()
        };
        total.saturating_sub(self.rows + scroll_offset)
    }

    fn row_at(&self, absolute: usize) -> Option<&Row> {
        if absolute < self.history.len() {
            self.history.get(absolute)
        } else {
            self.screen.get(absolute - self.history.len())
        }
    }

    fn push_history(&mut self, row: Row) {
        if self.scrollback_limit == 0 {
            return;
        }
        self.history.push_back(row);
        while self.history.len() > self.scrollback_limit {
            self.history.pop_front();
        }
    }

    fn blank_row(&self) -> Row {
        Row::blank(self.cols, self.attrs)
    }

    fn linefeed(&mut self) {
        if self.cursor.row == self.scroll_bottom {
            self.scroll_up(self.scroll_top, self.scroll_bottom, 1);
        } else if self.cursor.row + 1 < self.rows {
            self.cursor.row += 1;
        }
    }

    fn reverse_index(&mut self) {
        if self.cursor.row == self.scroll_top {
            self.scroll_down(self.scroll_top, self.scroll_bottom, 1);
        } else {
            self.cursor.row = self.cursor.row.saturating_sub(1);
        }
    }

    fn scroll_up(&mut self, top: usize, bottom: usize, count: usize) {
        let count = count.min(bottom - top + 1).max(1);
        for _ in 0..count {
            if top == 0 && bottom == self.rows - 1 && !self.alt_screen {
                let row = self.screen.remove(0);
                self.screen.push(self.blank_row());
                self.push_history(row);
            } else {
                self.screen.remove(top);
                self.screen.insert(bottom, self.blank_row());
            }
        }
        self.version = self.version.wrapping_add(1);
    }

    fn scroll_down(&mut self, top: usize, bottom: usize, count: usize) {
        let count = count.min(bottom - top + 1).max(1);
        for _ in 0..count {
            self.screen.remove(bottom);
            self.screen.insert(top, self.blank_row());
        }
        self.version = self.version.wrapping_add(1);
    }

    fn tab(&mut self) {
        let mut col = self.cursor.col + 1;
        while col < self.cols && !self.tabs.get(col).copied().unwrap_or(false) {
            col += 1;
        }
        self.cursor.col = if col >= self.cols { self.cols - 1 } else { col };
        self.wrap_pending = false;
    }

    fn back_tab(&mut self, count: usize) {
        for _ in 0..count.max(1) {
            let mut col = self.cursor.col;
            loop {
                if col == 0 {
                    break;
                }
                col -= 1;
                if self.tabs.get(col).copied().unwrap_or(false) {
                    break;
                }
            }
            self.cursor.col = col;
        }
        self.wrap_pending = false;
    }

    fn clear_wide_at(&mut self, row: usize, col: usize) {
        if row >= self.screen.len() {
            return;
        }
        let width = self.screen[row]
            .cells
            .get(col)
            .map(|cell| cell.width)
            .unwrap_or(1);
        if width == 0 && col > 0 {
            if let Some(previous) = self.screen[row].cells.get_mut(col - 1) {
                *previous = Cell::blank(self.attrs);
            }
        } else if width == 2 {
            if let Some(next) = self.screen[row].cells.get_mut(col + 1) {
                *next = Cell::blank(self.attrs);
            }
        }
    }

    fn print_char(&mut self, ch: char) {
        let width = UnicodeWidthChar::width(ch).unwrap_or(0);
        if width == 0 {
            // Combining mark: attach to the previous cell.
            let row = self.cursor.row;
            let mut col = self.cursor.col;
            if self.wrap_pending {
                if col > 0 {
                    col -= 1;
                }
            } else if col > 0 {
                col -= 1;
            }
            if self.screen[row].cells.get(col).map(|c| c.width) == Some(0) && col > 0 {
                col -= 1;
            }
            if let Some(cell) = self.screen[row].cells.get_mut(col) {
                cell.push_combining(ch);
            }
            self.version = self.version.wrapping_add(1);
            return;
        }

        if self.wrap_pending {
            self.wrap_pending = false;
            if self.modes.autowrap {
                self.screen[self.cursor.row].wrapped = true;
                self.linefeed();
                self.cursor.col = 0;
            }
        }

        if width == 2 && self.cursor.col + 1 >= self.cols {
            if self.modes.autowrap {
                self.screen[self.cursor.row].wrapped = true;
                self.linefeed();
                self.cursor.col = 0;
            } else {
                self.cursor.col = self.cols.saturating_sub(2);
            }
        }

        if self.modes.insert {
            self.insert_chars(width, self.attrs);
        }

        let row = self.cursor.row;
        let col = self.cursor.col;
        self.clear_wide_at(row, col);
        if width == 2 {
            self.clear_wide_at(row, col + 1);
        }

        if let Some(cell) = self.screen[row].cells.get_mut(col) {
            *cell = Cell::new(ch, width as u8, self.attrs);
        }
        if width == 2 {
            if let Some(cell) = self.screen[row].cells.get_mut(col + 1) {
                *cell = Cell {
                    ch: ' ',
                    combining: None,
                    attrs: self.attrs,
                    width: 0,
                };
            }
        }

        self.last_printed = Some(ch);
        self.cursor.col += width;
        if self.cursor.col >= self.cols {
            self.cursor.col = self.cols - 1;
            self.wrap_pending = true;
        }
        self.version = self.version.wrapping_add(1);
    }

    fn insert_chars(&mut self, count: usize, attrs: Attrs) {
        let row = self.cursor.row;
        let col = self.cursor.col;
        let len = self.cols;
        let cells = &mut self.screen[row].cells;
        for _ in 0..count.min(len) {
            cells.insert(col, Cell::blank(attrs));
            cells.truncate(len);
        }
        self.version = self.version.wrapping_add(1);
    }

    fn delete_chars(&mut self, count: usize) {
        let row = self.cursor.row;
        let col = self.cursor.col;
        let cells = &mut self.screen[row].cells;
        for _ in 0..count.min(self.cols) {
            if col < cells.len() {
                cells.remove(col);
                cells.push(Cell::blank(self.attrs));
            }
        }
        self.version = self.version.wrapping_add(1);
    }

    fn erase_cells(&mut self, row: usize, from: usize, to: usize) {
        if row >= self.screen.len() {
            return;
        }
        let attrs = self.attrs;
        for col in from..to.min(self.cols) {
            if let Some(cell) = self.screen[row].cells.get_mut(col) {
                *cell = Cell::blank(attrs);
            }
        }
        self.version = self.version.wrapping_add(1);
    }

    fn erase_display(&mut self, mode: u16) {
        let (row, col) = (self.cursor.row, self.cursor.col);
        match mode {
            0 => {
                self.erase_cells(row, col, self.cols);
                for r in row + 1..self.rows {
                    self.erase_cells(r, 0, self.cols);
                }
            }
            1 => {
                for r in 0..row {
                    self.erase_cells(r, 0, self.cols);
                }
                self.erase_cells(row, 0, col + 1);
            }
            2 => {
                for r in 0..self.rows {
                    self.erase_cells(r, 0, self.cols);
                }
            }
            3 => {
                for r in 0..self.rows {
                    self.erase_cells(r, 0, self.cols);
                }
                self.history.clear();
            }
            _ => {}
        }
        self.version = self.version.wrapping_add(1);
    }

    fn erase_line(&mut self, mode: u16) {
        let (row, col) = (self.cursor.row, self.cursor.col);
        match mode {
            0 => self.erase_cells(row, col, self.cols),
            1 => self.erase_cells(row, 0, col + 1),
            2 => self.erase_cells(row, 0, self.cols),
            _ => {}
        }
    }

    fn insert_lines(&mut self, count: usize) {
        if self.cursor.row < self.scroll_top || self.cursor.row > self.scroll_bottom {
            return;
        }
        let count = count.min(self.scroll_bottom - self.cursor.row + 1).max(1);
        for _ in 0..count {
            self.screen.remove(self.scroll_bottom);
            self.screen.insert(self.cursor.row, self.blank_row());
        }
        self.version = self.version.wrapping_add(1);
    }

    fn delete_lines(&mut self, count: usize) {
        if self.cursor.row < self.scroll_top || self.cursor.row > self.scroll_bottom {
            return;
        }
        let count = count.min(self.scroll_bottom - self.cursor.row + 1).max(1);
        for _ in 0..count {
            self.screen.remove(self.cursor.row);
            self.screen.insert(self.scroll_bottom, self.blank_row());
        }
        self.version = self.version.wrapping_add(1);
    }

    fn set_cursor(&mut self, row: usize, col: usize) {
        let (min_row, max_row) = if self.modes.origin {
            (self.scroll_top, self.scroll_bottom)
        } else {
            (0, self.rows - 1)
        };
        self.cursor.row = (min_row + row).min(max_row);
        self.cursor.col = col.min(self.cols - 1);
        self.wrap_pending = false;
    }

    fn save_cursor(&mut self) {
        self.saved_cursor = Some((self.cursor.row, self.cursor.col, self.attrs));
    }

    fn restore_cursor(&mut self) {
        if let Some((row, col, attrs)) = self.saved_cursor {
            self.cursor.row = row.min(self.rows - 1);
            self.cursor.col = col.min(self.cols - 1);
            self.attrs = attrs;
            self.wrap_pending = false;
        }
    }

    fn enter_alt_screen(&mut self, save_cursor: bool) {
        if self.alt_screen {
            return;
        }
        if save_cursor {
            self.alt_saved_cursor = Some((self.cursor.row, self.cursor.col, self.attrs));
        }
        let saved = SavedScreen {
            lines: std::mem::replace(
                &mut self.screen,
                vec![Row::blank(self.cols, self.attrs); self.rows],
            ),
            cursor_row: self.cursor.row,
            cursor_col: self.cursor.col,
            scroll_top: self.scroll_top,
            scroll_bottom: self.scroll_bottom,
        };
        self.main_screen = Some(saved);
        self.alt_screen = true;
        self.modes.alt_screen = true;
        self.cursor.row = 0;
        self.cursor.col = 0;
        self.scroll_top = 0;
        self.scroll_bottom = self.rows - 1;
        self.wrap_pending = false;
        self.version = self.version.wrapping_add(1);
    }

    fn exit_alt_screen(&mut self, restore_cursor: bool) {
        if !self.alt_screen {
            return;
        }
        if let Some(saved) = self.main_screen.take() {
            self.screen = saved.lines;
            self.cursor.row = saved.cursor_row.min(self.rows - 1);
            self.cursor.col = saved.cursor_col.min(self.cols - 1);
            self.scroll_top = saved.scroll_top;
            self.scroll_bottom = saved.scroll_bottom;
        }
        self.alt_screen = false;
        self.modes.alt_screen = false;
        if restore_cursor {
            if let Some((row, col, attrs)) = self.alt_saved_cursor.take() {
                self.cursor.row = row.min(self.rows - 1);
                self.cursor.col = col.min(self.cols - 1);
                self.attrs = attrs;
            }
        }
        self.wrap_pending = false;
        self.version = self.version.wrapping_add(1);
    }

    fn reset(&mut self) {
        let attrs = Attrs::default();
        self.screen = vec![Row::blank(self.cols, attrs); self.rows];
        self.history.clear();
        self.cursor.row = 0;
        self.cursor.col = 0;
        self.cursor.style = CursorStyle::Block;
        self.cursor.visible = true;
        self.scroll_top = 0;
        self.scroll_bottom = self.rows - 1;
        self.attrs = attrs;
        self.modes = Modes::default();
        self.saved_cursor = None;
        self.alt_saved_cursor = None;
        self.main_screen = None;
        self.alt_screen = false;
        self.wrap_pending = false;
        self.tabs = (0..self.cols).map(|index| index % 8 == 0).collect();
        self.version = self.version.wrapping_add(1);
    }

    fn set_mode(&mut self, mode: u16, enabled: bool) {
        match mode {
            1 => self.modes.application_cursor = enabled,
            4 => self.modes.insert = enabled,
            6 => {
                self.modes.origin = enabled;
                self.set_cursor(0, 0);
            }
            7 => {
                self.modes.autowrap = enabled;
                if !enabled {
                    self.wrap_pending = false;
                }
            }
            12 => self.cursor.blinking = enabled,
            25 => {
                self.cursor.visible = enabled;
                self.modes.cursor_visible = enabled;
            }
            1000 => {
                self.modes.mouse_mode = if enabled {
                    MouseMode::Click
                } else {
                    MouseMode::None
                }
            }
            1002 => {
                self.modes.mouse_mode = if enabled {
                    MouseMode::Drag
                } else {
                    MouseMode::None
                }
            }
            1003 => {
                self.modes.mouse_mode = if enabled {
                    MouseMode::Any
                } else {
                    MouseMode::None
                }
            }
            1004 => self.modes.focus_events = enabled,
            1005 | 1006 => {
                self.modes.mouse_encoding = if enabled {
                    MouseEncoding::Sgr
                } else {
                    MouseEncoding::X10
                }
            }
            1047 => {
                if enabled {
                    self.enter_alt_screen(false);
                } else {
                    self.exit_alt_screen(false);
                }
            }
            1049 => {
                if enabled {
                    self.enter_alt_screen(true);
                } else {
                    self.exit_alt_screen(true);
                }
            }
            2004 => self.modes.bracketed_paste = enabled,
            _ => {}
        }
        self.version = self.version.wrapping_add(1);
    }

    fn set_private_mode(&mut self, mode: u16, enabled: bool) {
        if mode == 47 {
            if enabled {
                self.enter_alt_screen(false);
            } else {
                self.exit_alt_screen(false);
            }
            self.version = self.version.wrapping_add(1);
            return;
        }
        self.set_mode(mode, enabled);
    }

    fn sgr(&mut self, params: &Params) {
        let groups: Vec<&[u16]> = params.iter().collect();
        let mut index = 0;
        while index < groups.len() {
            let group = groups[index];
            if group.is_empty() {
                index += 1;
                continue;
            }

            // Colon sub-parameters, e.g. `4:3` or `38:2::r:g:b`.
            if group.len() > 1 {
                match group[0] {
                    4 => {
                        self.attrs.flags.remove(Flags::UNDERLINE);
                        self.attrs.flags.remove(Flags::DOUBLE_UNDERLINE);
                        self.attrs.flags.remove(Flags::CURLY_UNDERLINE);
                        match group.get(1).copied().unwrap_or(1) {
                            0 => {}
                            2 => self.attrs.flags.insert(Flags::DOUBLE_UNDERLINE),
                            3 => self.attrs.flags.insert(Flags::CURLY_UNDERLINE),
                            4 => self.attrs.flags.insert(Flags::UNDERLINE),
                            _ => self.attrs.flags.insert(Flags::UNDERLINE),
                        }
                    }
                    38 | 48 | 58 => {
                        let color = self.extended_color(&group[1..]);
                        match group[0] {
                            38 => self.attrs.fg = color,
                            48 => self.attrs.bg = color,
                            _ => self.attrs.underline_color = color,
                        }
                    }
                    _ => {}
                }
                index += 1;
                continue;
            }

            let param = group[0];
            match param {
                0 => self.attrs = Attrs::default(),
                1 => self.attrs.flags.insert(Flags::BOLD),
                2 => self.attrs.flags.insert(Flags::DIM),
                3 => self.attrs.flags.insert(Flags::ITALIC),
                4 => self.attrs.flags.insert(Flags::UNDERLINE),
                5 | 6 => self.attrs.flags.insert(Flags::BLINK),
                7 => self.attrs.flags.insert(Flags::REVERSE),
                8 => self.attrs.flags.insert(Flags::HIDDEN),
                9 => self.attrs.flags.insert(Flags::STRIKETHROUGH),
                21 => {
                    self.attrs.flags.remove(Flags::BOLD);
                    self.attrs.flags.insert(Flags::DOUBLE_UNDERLINE);
                }
                22 => {
                    self.attrs.flags.remove(Flags::BOLD);
                    self.attrs.flags.remove(Flags::DIM);
                }
                23 => self.attrs.flags.remove(Flags::ITALIC),
                24 => {
                    self.attrs.flags.remove(Flags::UNDERLINE);
                    self.attrs.flags.remove(Flags::DOUBLE_UNDERLINE);
                    self.attrs.flags.remove(Flags::CURLY_UNDERLINE);
                }
                25 => self.attrs.flags.remove(Flags::BLINK),
                27 => self.attrs.flags.remove(Flags::REVERSE),
                28 => self.attrs.flags.remove(Flags::HIDDEN),
                29 => self.attrs.flags.remove(Flags::STRIKETHROUGH),
                30..=37 => self.attrs.fg = Color::Indexed((param - 30) as u8),
                39 => self.attrs.fg = Color::Default,
                40..=47 => self.attrs.bg = Color::Indexed((param - 40) as u8),
                49 => self.attrs.bg = Color::Default,
                58 => {
                    // Underline color with simple semicolon form.
                    let (color, consumed) = self.parse_extended_color(&groups, index + 1);
                    self.attrs.underline_color = color;
                    index += consumed;
                }
                38 | 48 => {
                    let (color, consumed) = self.parse_extended_color(&groups, index + 1);
                    if param == 38 {
                        self.attrs.fg = color;
                    } else {
                        self.attrs.bg = color;
                    }
                    index += consumed;
                }
                90..=97 => self.attrs.fg = Color::Indexed((param - 90 + 8) as u8),
                100..=107 => self.attrs.bg = Color::Indexed((param - 100 + 8) as u8),
                _ => {}
            }
            index += 1;
        }
        self.version = self.version.wrapping_add(1);
    }

    fn extended_color(&self, sub: &[u16]) -> Color {
        match sub.first().copied().unwrap_or(0) {
            5 => Color::Indexed(sub.get(1).copied().unwrap_or(0) as u8),
            2 => {
                let offset = if sub.len() >= 5 { 2 } else { 1 };
                Color::Rgb(
                    sub.get(offset).copied().unwrap_or(0) as u8,
                    sub.get(offset + 1).copied().unwrap_or(0) as u8,
                    sub.get(offset + 2).copied().unwrap_or(0) as u8,
                )
            }
            _ => Color::Default,
        }
    }

    fn parse_extended_color(&self, groups: &[&[u16]], start: usize) -> (Color, usize) {
        let Some(mode) = groups.get(start).and_then(|group| group.first()).copied() else {
            return (Color::Default, 0);
        };
        match mode {
            5 => {
                let value = groups
                    .get(start + 1)
                    .and_then(|group| group.first())
                    .copied()
                    .unwrap_or(0);
                (Color::Indexed(value as u8), 2)
            }
            2 => {
                let red = groups
                    .get(start + 1)
                    .and_then(|group| group.first())
                    .copied()
                    .unwrap_or(0);
                let green = groups
                    .get(start + 2)
                    .and_then(|group| group.first())
                    .copied()
                    .unwrap_or(0);
                let blue = groups
                    .get(start + 3)
                    .and_then(|group| group.first())
                    .copied()
                    .unwrap_or(0);
                (Color::Rgb(red as u8, green as u8, blue as u8), 4)
            }
            _ => (Color::Default, 1),
        }
    }

    fn device_status(&mut self, kind: u16) {
        match kind {
            5 => self.responses.push(b"\x1b[0n".to_vec()),
            6 => {
                let row = self.cursor.row + 1;
                let col = self.cursor.col + 1;
                self.responses
                    .push(format!("\x1b[{row};{col}R").into_bytes());
            }
            _ => {}
        }
    }

    fn device_attributes(&mut self, secondary: bool) {
        if secondary {
            self.responses.push(b"\x1b[>0;10;0c".to_vec());
        } else {
            self.responses.push(b"\x1b[?1;2c".to_vec());
        }
    }

    fn osc_dispatch(&mut self, params: &[&[u8]]) {
        let Some(command) = params.first() else {
            return;
        };
        let command = String::from_utf8_lossy(command);
        match command.as_ref() {
            "0" | "1" | "2" => {
                if let Some(title) = params.get(1) {
                    self.title = String::from_utf8_lossy(title).into_owned();
                }
            }
            "52" => {
                // OSC 52 ; c ; base64
                if let Some(data) = params.get(2) {
                    if let Ok(decoded) = base64_decode(data) {
                        if decoded.len() <= 1024 * 1024 {
                            self.clipboard = Some(decoded);
                        }
                    }
                }
            }
            _ => {}
        }
    }
}

impl Perform for Terminal {
    fn print(&mut self, c: char) {
        self.print_char(c);
    }

    fn execute(&mut self, byte: u8) {
        match byte {
            0x07 => {
                self.bell_count += 1;
            }
            0x08 => {
                self.cursor.col = self.cursor.col.saturating_sub(1);
                self.wrap_pending = false;
            }
            0x09 => self.tab(),
            0x0a | 0x0b | 0x0c => {
                self.linefeed();
                self.wrap_pending = false;
            }
            0x0d => {
                self.cursor.col = 0;
                self.wrap_pending = false;
            }
            _ => {}
        }
    }

    fn hook(&mut self, _params: &Params, _intermediates: &[u8], _ignore: bool, _action: char) {}

    fn put(&mut self, _byte: u8) {}

    fn unhook(&mut self) {}

    fn osc_dispatch(&mut self, params: &[&[u8]], _bell_terminated: bool) {
        self.osc_dispatch(params);
    }

    fn csi_dispatch(&mut self, params: &Params, intermediates: &[u8], ignore: bool, action: char) {
        if ignore {
            return;
        }
        let flat: Vec<u16> = params
            .iter()
            .flat_map(|group| group.iter().copied())
            .collect();
        let private = intermediates.contains(&b'?');
        let space = intermediates.contains(&b' ');
        let first = flat.first().copied().unwrap_or(0);

        match action {
            'h' | 'l' => {
                let enabled = action == 'h';
                if private {
                    for mode in &flat {
                        self.set_private_mode(*mode, enabled);
                    }
                } else {
                    for mode in &flat {
                        self.set_mode(*mode, enabled);
                    }
                }
            }
            'm' => self.sgr(params),
            'H' | 'f' => {
                let row = param(&flat, 0, 1).saturating_sub(1);
                let col = param(&flat, 1, 1).saturating_sub(1);
                self.set_cursor(row as usize, col as usize);
            }
            'A' => {
                let count = param(&flat, 0, 1) as usize;
                let floor = if self.cursor.row >= self.scroll_top {
                    self.scroll_top
                } else {
                    0
                };
                self.cursor.row = self.cursor.row.saturating_sub(count).max(floor);
                self.wrap_pending = false;
            }
            'B' => {
                let count = param(&flat, 0, 1) as usize;
                let ceiling = if self.cursor.row <= self.scroll_bottom {
                    self.scroll_bottom
                } else {
                    self.rows - 1
                };
                self.cursor.row = (self.cursor.row + count).min(ceiling);
                self.wrap_pending = false;
            }
            'C' | 'a' => {
                let count = param(&flat, 0, 1) as usize;
                self.cursor.col = (self.cursor.col + count).min(self.cols - 1);
                self.wrap_pending = false;
            }
            'D' => {
                let count = param(&flat, 0, 1) as usize;
                self.cursor.col = self.cursor.col.saturating_sub(count);
                self.wrap_pending = false;
            }
            'E' => {
                let count = param(&flat, 0, 1) as usize;
                self.cursor.row = (self.cursor.row + count).min(self.rows - 1);
                self.cursor.col = 0;
                self.wrap_pending = false;
            }
            'F' => {
                let count = param(&flat, 0, 1) as usize;
                self.cursor.row = self.cursor.row.saturating_sub(count);
                self.cursor.col = 0;
                self.wrap_pending = false;
            }
            'G' | '`' => {
                self.cursor.col = param(&flat, 0, 1).saturating_sub(1) as usize;
                self.cursor.col = self.cursor.col.min(self.cols - 1);
                self.wrap_pending = false;
            }
            'd' => {
                let row = param(&flat, 0, 1).saturating_sub(1) as usize;
                self.cursor.row = row.min(self.rows - 1);
                self.wrap_pending = false;
            }
            'e' => {
                let count = param(&flat, 0, 1) as usize;
                self.cursor.row = (self.cursor.row + count).min(self.rows - 1);
                self.wrap_pending = false;
            }
            'J' => self.erase_display(first),
            'K' => self.erase_line(first),
            'L' => self.insert_lines(param(&flat, 0, 1) as usize),
            'M' => self.delete_lines(param(&flat, 0, 1) as usize),
            'P' => self.delete_chars(param(&flat, 0, 1) as usize),
            '@' => self.insert_chars(param(&flat, 0, 1) as usize, self.attrs),
            'X' => {
                let count = param(&flat, 0, 1) as usize;
                let (row, col) = (self.cursor.row, self.cursor.col);
                self.erase_cells(row, col, (col + count).min(self.cols));
            }
            'S' => self.scroll_up(
                self.scroll_top,
                self.scroll_bottom,
                param(&flat, 0, 1) as usize,
            ),
            'T' => self.scroll_down(
                self.scroll_top,
                self.scroll_bottom,
                param(&flat, 0, 1) as usize,
            ),
            'Z' => self.back_tab(param(&flat, 0, 1) as usize),
            'r' => {
                let top = param(&flat, 0, 1).saturating_sub(1) as usize;
                let bottom = param(&flat, 1, self.rows as u16).saturating_sub(1) as usize;
                if top < bottom && bottom < self.rows {
                    self.scroll_top = top;
                    self.scroll_bottom = bottom;
                    self.set_cursor(0, 0);
                }
            }
            's' => self.save_cursor(),
            'u' => self.restore_cursor(),
            'n' => self.device_status(first),
            'c' => {
                if intermediates.contains(&b'>') {
                    self.device_attributes(true);
                } else {
                    self.device_attributes(false);
                }
            }
            'g' => {
                if first == 0 {
                    if let Some(tab) = self.tabs.get_mut(self.cursor.col) {
                        *tab = false;
                    }
                } else if first == 3 {
                    self.tabs.iter_mut().for_each(|tab| *tab = false);
                }
            }
            'q' if space => {
                self.cursor.style = match first {
                    0 | 1 | 2 => CursorStyle::Block,
                    3 | 4 => CursorStyle::Underline,
                    5 | 6 => CursorStyle::Bar,
                    _ => CursorStyle::Block,
                };
                self.cursor.blinking = first % 2 == 0;
            }
            'b' => {
                if let Some(ch) = self.last_printed {
                    let count = param(&flat, 0, 1) as usize;
                    for _ in 0..count {
                        self.print_char(ch);
                    }
                }
            }
            _ => {}
        }
        self.version = self.version.wrapping_add(1);
    }

    fn esc_dispatch(&mut self, intermediates: &[u8], _ignore: bool, byte: u8) {
        match (intermediates, byte) {
            ([], b'7') => self.save_cursor(),
            ([], b'8') => self.restore_cursor(),
            ([], b'D') => {
                self.linefeed();
                self.wrap_pending = false;
            }
            ([], b'E') => {
                self.cursor.col = 0;
                self.linefeed();
                self.wrap_pending = false;
            }
            ([], b'M') => {
                self.reverse_index();
                self.wrap_pending = false;
            }
            ([], b'H') => {
                if let Some(tab) = self.tabs.get_mut(self.cursor.col) {
                    *tab = true;
                }
            }
            ([], b'c') => self.reset(),
            ([], b'=') => self.modes.application_keypad = true,
            ([], b'>') => self.modes.application_keypad = false,
            _ => {}
        }
        self.version = self.version.wrapping_add(1);
    }
}

fn param(params: &[u16], index: usize, default: u16) -> u16 {
    params
        .get(index)
        .copied()
        .filter(|value| *value != 0)
        .unwrap_or(default)
}

fn normalize(start: (usize, usize), end: (usize, usize)) -> ((usize, usize), (usize, usize)) {
    if start.0 < end.0 || (start.0 == end.0 && start.1 <= end.1) {
        (start, end)
    } else {
        (end, start)
    }
}

/// Minimal base64 decoder for OSC 52. Avoids a dependency for a rarely used
/// feature.
fn base64_decode(input: &[u8]) -> Result<String, ()> {
    fn value(byte: u8) -> Option<u8> {
        match byte {
            b'A'..=b'Z' => Some(byte - b'A'),
            b'a'..=b'z' => Some(byte - b'a' + 26),
            b'0'..=b'9' => Some(byte - b'0' + 52),
            b'+' => Some(62),
            b'/' => Some(63),
            _ => None,
        }
    }
    let mut out = Vec::with_capacity(input.len() * 3 / 4);
    let mut buffer = 0u32;
    let mut bits = 0;
    for &byte in input {
        if byte == b'=' || byte == b'\n' || byte == b'\r' {
            continue;
        }
        let Some(v) = value(byte) else {
            return Err(());
        };
        buffer = (buffer << 6) | v as u32;
        bits += 6;
        if bits >= 8 {
            bits -= 8;
            out.push((buffer >> bits) as u8);
        }
    }
    String::from_utf8(out).map_err(|_| ())
}
