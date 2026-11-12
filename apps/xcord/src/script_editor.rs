//! Multiline `.rhai` editor with mouse positioning, selection, clipboard and
//! a small undo history.

use std::cell::Cell;
use std::ops::Range;
use std::rc::Rc;

use gpui::prelude::*;
use gpui::{
    div, px, Bounds, ClipboardItem, Context, CursorStyle, FocusHandle, Focusable, Font, FontWeight,
    Hsla, KeyDownEvent, MouseButton, MouseDownEvent, MouseMoveEvent, MouseUpEvent, Pixels, Point,
    Render, ScrollHandle, ShapedLine, TextRun, Window,
};

const FONT_SIZE: f32 = 11.5;
const LINE_HEIGHT: f32 = 18.0;
const GUTTER: f32 = 46.0;
const PADDING: f32 = 8.0;

pub struct ScriptEditor {
    focus_handle: FocusHandle,
    content: String,
    anchor: usize,
    head: usize,
    selecting: bool,
    active_line: Option<usize>,
    foreground: Hsla,
    muted: Hsla,
    accent: Hsla,
    danger: Hsla,
    error_line: Option<usize>,
    revision: u64,
    scroll: ScrollHandle,
    bounds: Rc<Cell<Option<Bounds<Pixels>>>>,
    char_width: f32,
    undo: Vec<(String, usize, usize)>,
    redo: Vec<(String, usize, usize)>,
}

impl ScriptEditor {
    pub fn new(
        foreground: Hsla,
        muted: Hsla,
        accent: Hsla,
        danger: Hsla,
        cx: &mut Context<Self>,
    ) -> Self {
        Self {
            focus_handle: cx.focus_handle(),
            content: String::new(),
            anchor: 0,
            head: 0,
            selecting: false,
            active_line: None,
            foreground,
            muted,
            accent,
            danger,
            error_line: None,
            revision: 0,
            scroll: ScrollHandle::new(),
            bounds: Rc::new(Cell::new(None)),
            char_width: FONT_SIZE * 0.6,
            undo: Vec::new(),
            redo: Vec::new(),
        }
    }

    pub fn value(&self) -> &str {
        &self.content
    }

    pub fn set_value(&mut self, value: impl Into<String>, cx: &mut Context<Self>) {
        self.content = value.into();
        self.anchor = self.content.len();
        self.head = self.content.len();
        self.undo.clear();
        self.redo.clear();
        self.revision = self.revision.wrapping_add(1);
        self.scroll_to_head();
        cx.notify();
    }

    pub fn revision(&self) -> u64 {
        self.revision
    }

    pub fn set_error_line(&mut self, line: Option<usize>, cx: &mut Context<Self>) {
        if self.error_line == line {
            return;
        }
        self.error_line = line;
        cx.notify();
    }

    pub fn set_active_line(&mut self, line: Option<usize>, cx: &mut Context<Self>) {
        if self.active_line == line {
            return;
        }
        self.active_line = line;
        if let Some(line) = line {
            self.scroll.scroll_to_item(line.saturating_sub(1));
        }
        cx.notify();
    }

    fn selection(&self) -> Range<usize> {
        self.anchor.min(self.head)..self.anchor.max(self.head)
    }

    fn cursor_line_column(&self) -> (usize, usize) {
        let before = &self.content[..self.head.min(self.content.len())];
        let line = before.matches('\n').count();
        let start = before.rfind('\n').map(|index| index + 1).unwrap_or(0);
        (line, self.head - start)
    }

    fn line_start(&self, line: usize) -> usize {
        let mut start = 0;
        for _ in 0..line {
            let Some(offset) = self.content[start..].find('\n') else {
                return self.content.len();
            };
            start += offset + 1;
        }
        start
    }

    fn line_end(&self, line: usize) -> usize {
        let start = self.line_start(line);
        self.content[start..]
            .find('\n')
            .map(|offset| start + offset)
            .unwrap_or(self.content.len())
    }

    fn scroll_to_head(&self) {
        let (line, _) = self.cursor_line_column();
        self.scroll.scroll_to_item(line);
    }

    fn snapshot(&mut self) {
        if self
            .undo
            .last()
            .is_some_and(|(text, _, _)| text == &self.content)
        {
            return;
        }
        self.undo
            .push((self.content.clone(), self.anchor, self.head));
        if self.undo.len() > 100 {
            self.undo.remove(0);
        }
        self.redo.clear();
    }

    fn previous_char(&self, offset: usize) -> usize {
        self.content[..offset.min(self.content.len())]
            .char_indices()
            .next_back()
            .map(|(index, _)| index)
            .unwrap_or(0)
    }

    fn next_char(&self, offset: usize) -> usize {
        self.content[offset.min(self.content.len())..]
            .char_indices()
            .nth(1)
            .map(|(index, _)| offset + index)
            .unwrap_or(self.content.len())
    }

    fn previous_word(&self, offset: usize) -> usize {
        let mut index = offset;
        while index > 0 {
            let previous = self.previous_char(index);
            let ch = self.content[previous..index].chars().next().unwrap_or(' ');
            if ch.is_alphanumeric() || ch == '_' {
                break;
            }
            index = previous;
        }
        while index > 0 {
            let previous = self.previous_char(index);
            let ch = self.content[previous..index].chars().next().unwrap_or(' ');
            if !(ch.is_alphanumeric() || ch == '_') {
                break;
            }
            index = previous;
        }
        index
    }

    fn next_word(&self, offset: usize) -> usize {
        let mut index = offset;
        while index < self.content.len() {
            let next = self.next_char(index);
            let ch = self.content[index..next].chars().next().unwrap_or(' ');
            if ch.is_alphanumeric() || ch == '_' {
                break;
            }
            index = next;
        }
        while index < self.content.len() {
            let next = self.next_char(index);
            let ch = self.content[index..next].chars().next().unwrap_or(' ');
            if !(ch.is_alphanumeric() || ch == '_') {
                break;
            }
            index = next;
        }
        index
    }

    fn word_range(&self, offset: usize) -> Range<usize> {
        let mut start = offset.min(self.content.len());
        let mut end = start;
        while start > 0 {
            let previous = self.previous_char(start);
            let ch = self.content[previous..start].chars().next().unwrap_or(' ');
            if !(ch.is_alphanumeric() || ch == '_') {
                break;
            }
            start = previous;
        }
        while end < self.content.len() {
            let next = self.next_char(end);
            let ch = self.content[end..next].chars().next().unwrap_or(' ');
            if !(ch.is_alphanumeric() || ch == '_') {
                break;
            }
            end = next;
        }
        if start == end {
            start..self.next_char(end)
        } else {
            start..end
        }
    }

    fn move_to(&mut self, offset: usize, extend: bool, cx: &mut Context<Self>) {
        let offset = offset.min(self.content.len());
        if extend {
            self.head = offset;
        } else {
            self.anchor = offset;
            self.head = offset;
        }
        self.scroll_to_head();
        cx.notify();
    }

    fn move_vertical(&mut self, delta: isize, extend: bool, cx: &mut Context<Self>) {
        let (line, _) = self.cursor_line_column();
        let line_start = self.line_start(line);
        let column = self.head.saturating_sub(line_start);
        let target = (line as isize + delta).clamp(0, self.content.split('\n').count() as isize - 1)
            as usize;
        let start = self.line_start(target);
        let end = self.line_end(target);
        let offset = start + column.min(end - start);
        self.move_to(offset, extend, cx);
    }

    fn insert(&mut self, text: &str, cx: &mut Context<Self>) {
        self.snapshot();
        self.revision = self.revision.wrapping_add(1);
        let range = self.selection();
        self.content.replace_range(range.clone(), text);
        self.anchor = range.start + text.len();
        self.head = self.anchor;
        self.scroll_to_head();
        cx.notify();
    }

    fn delete_selection(&mut self, cx: &mut Context<Self>) -> bool {
        let range = self.selection();
        if range.is_empty() {
            return false;
        }
        self.snapshot();
        self.revision = self.revision.wrapping_add(1);
        self.content.replace_range(range.clone(), "");
        self.anchor = range.start;
        self.head = range.start;
        self.scroll_to_head();
        cx.notify();
        true
    }

    fn backspace(&mut self, word: bool, cx: &mut Context<Self>) {
        if self.delete_selection(cx) {
            return;
        }
        if self.head == 0 {
            return;
        }
        let start = if word {
            self.previous_word(self.head)
        } else {
            self.previous_char(self.head)
        };
        self.snapshot();
        self.revision = self.revision.wrapping_add(1);
        self.content.replace_range(start..self.head, "");
        self.anchor = start;
        self.head = start;
        self.scroll_to_head();
        cx.notify();
    }

    fn delete(&mut self, word: bool, cx: &mut Context<Self>) {
        if self.delete_selection(cx) {
            return;
        }
        if self.head >= self.content.len() {
            return;
        }
        let end = if word {
            self.next_word(self.head)
        } else {
            self.next_char(self.head)
        };
        self.snapshot();
        self.revision = self.revision.wrapping_add(1);
        self.content.replace_range(self.head..end, "");
        self.scroll_to_head();
        cx.notify();
    }

    fn copy(&mut self, cut: bool, cx: &mut Context<Self>) {
        let range = self.selection();
        if range.is_empty() {
            return;
        }
        cx.write_to_clipboard(ClipboardItem::new_string(
            self.content[range.clone()].to_string(),
        ));
        if cut {
            self.delete_selection(cx);
        }
    }

    fn paste(&mut self, cx: &mut Context<Self>) {
        if let Some(text) = cx.read_from_clipboard().and_then(|item| item.text()) {
            let text = text.replace("\r\n", "\n").replace('\r', "\n");
            self.insert(&text, cx);
        }
    }

    fn offset_for_line_column(&self, line: usize, visual_column: usize) -> usize {
        let start = self.line_start(line);
        let end = self.line_end(line);
        let mut visual = 0usize;
        for (index, ch) in self.content[start..end].char_indices() {
            let width = if ch.is_ascii() { 1 } else { 2 };
            if visual + width / 2 >= visual_column {
                return start + index;
            }
            visual += width;
        }
        end
    }

    fn offset_for_position(&self, line: usize, position: Point<Pixels>) -> usize {
        let Some(bounds) = self.bounds.get() else {
            return self.line_start(line);
        };
        let content_x = f32::from(bounds.left()) + GUTTER + PADDING;
        let column = ((f32::from(position.x) - content_x) / self.char_width.max(1.0))
            .round()
            .max(0.0) as usize;
        self.offset_for_line_column(line, column)
    }

    fn begin_mouse_selection(
        &mut self,
        line: usize,
        event: &MouseDownEvent,
        cx: &mut Context<Self>,
    ) {
        let offset = self.offset_for_position(line, event.position);
        if event.click_count >= 2 {
            let range = self.word_range(offset);
            self.anchor = range.start;
            self.head = range.end;
        } else if event.modifiers.shift {
            self.head = offset;
        } else {
            self.anchor = offset;
            self.head = offset;
        }
        self.selecting = true;
        self.scroll_to_head();
        cx.notify();
    }

    fn on_mouse_move(&mut self, line: usize, event: &MouseMoveEvent, cx: &mut Context<Self>) {
        if !self.selecting {
            return;
        }
        self.head = self.offset_for_position(line, event.position);
        self.scroll_to_head();
        cx.notify();
    }

    fn on_key_down(&mut self, event: &KeyDownEvent, _window: &mut Window, cx: &mut Context<Self>) {
        let keystroke = &event.keystroke;
        let word = keystroke.modifiers.control || keystroke.modifiers.platform;
        let shift = keystroke.modifiers.shift;

        if word {
            match keystroke.key.as_str() {
                "a" => {
                    self.anchor = 0;
                    self.head = self.content.len();
                    cx.notify();
                    cx.stop_propagation();
                }
                "c" => {
                    self.copy(false, cx);
                    cx.stop_propagation();
                }
                "x" => {
                    self.copy(true, cx);
                    cx.stop_propagation();
                }
                "v" => {
                    self.paste(cx);
                    cx.stop_propagation();
                }
                "z" => {
                    if let Some((text, anchor, head)) = self.undo.pop() {
                        self.redo
                            .push((self.content.clone(), self.anchor, self.head));
                        self.content = text;
                        self.anchor = anchor.min(self.content.len());
                        self.head = head.min(self.content.len());
                        self.revision = self.revision.wrapping_add(1);
                        self.scroll_to_head();
                        cx.notify();
                    }
                    cx.stop_propagation();
                }
                "y" => {
                    if let Some((text, anchor, head)) = self.redo.pop() {
                        self.undo
                            .push((self.content.clone(), self.anchor, self.head));
                        self.content = text;
                        self.anchor = anchor.min(self.content.len());
                        self.head = head.min(self.content.len());
                        self.revision = self.revision.wrapping_add(1);
                        self.scroll_to_head();
                        cx.notify();
                    }
                    cx.stop_propagation();
                }
                "left" => {
                    let offset = self.previous_word(self.head);
                    self.move_to(offset, shift, cx);
                    cx.stop_propagation();
                }
                "right" => {
                    let offset = self.next_word(self.head);
                    self.move_to(offset, shift, cx);
                    cx.stop_propagation();
                }
                "home" => {
                    self.move_to(0, shift, cx);
                    cx.stop_propagation();
                }
                "end" => {
                    self.move_to(self.content.len(), shift, cx);
                    cx.stop_propagation();
                }
                "backspace" => {
                    self.backspace(true, cx);
                    cx.stop_propagation();
                }
                "delete" => {
                    self.delete(true, cx);
                    cx.stop_propagation();
                }
                _ => {}
            }
            return;
        }

        match keystroke.key.as_str() {
            "left" => self.move_to(self.previous_char(self.head), shift, cx),
            "right" => self.move_to(self.next_char(self.head), shift, cx),
            "up" => self.move_vertical(-1, shift, cx),
            "down" => self.move_vertical(1, shift, cx),
            "home" => {
                let (line, _) = self.cursor_line_column();
                self.move_to(self.line_start(line), shift, cx);
            }
            "end" => {
                let (line, _) = self.cursor_line_column();
                self.move_to(self.line_end(line), shift, cx);
            }
            "backspace" => self.backspace(false, cx),
            "delete" => self.delete(false, cx),
            "escape" => {
                self.anchor = self.head;
                cx.notify();
            }
            "enter" => self.insert("\n", cx),
            "tab" => self.insert("    ", cx),
            "space" => self.insert(" ", cx),
            _ => {
                if let Some(text) = keystroke.key_char.as_deref() {
                    if !text.is_empty() && text.chars().all(|ch| !ch.is_control()) {
                        self.insert(text, cx);
                    }
                }
            }
        }
        cx.stop_propagation();
    }
}

impl Focusable for ScriptEditor {
    fn focus_handle(&self, _: &gpui::App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl Render for ScriptEditor {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let font = Font {
            weight: FontWeight::NORMAL,
            ..gpui::font("Consolas")
        };
        let run = TextRun {
            len: 1,
            font: font.clone(),
            color: self.foreground.into(),
            background_color: None,
            underline: None,
            strikethrough: None,
        };
        let measure: ShapedLine =
            window
                .text_system()
                .shape_line("M".into(), px(FONT_SIZE), &[run], None);
        self.char_width = f32::from(measure.width).max(1.0);

        let lines = self.content.split('\n').collect::<Vec<_>>();
        let selection = self.selection();
        let (cursor_line, _) = self.cursor_line_column();
        let mut rows = div()
            .id("script-editor-lines")
            .flex()
            .flex_col()
            .p(px(PADDING))
            .flex_1()
            .min_h(px(0.))
            .track_scroll(&self.scroll)
            .overflow_y_scroll()
            .font_family("Consolas")
            .text_size(px(FONT_SIZE));
        for (index, line) in lines.iter().enumerate() {
            let line_start = self.line_start(index);
            let line_end = self.line_end(index);
            let selected_start = selection.start.max(line_start).min(line_end);
            let selected_end = selection.end.max(line_start).min(line_end);
            let active = self.active_line == Some(index + 1);
            let error = self.error_line == Some(index + 1);
            let mut row = div()
                .id(("script-editor-line", index))
                .w_full()
                .flex()
                .items_start()
                .h(px(LINE_HEIGHT))
                .whitespace_nowrap()
                .overflow_hidden()
                .rounded_sm()
                .when(error, |row| {
                    row.bg(Hsla {
                        a: 0.22,
                        ..self.danger
                    })
                })
                .when(active && !error, |row| {
                    row.bg(Hsla {
                        a: 0.18,
                        ..self.accent
                    })
                })
                .child(
                    div()
                        .w(px(GUTTER))
                        .flex_shrink_0()
                        .text_color(if error {
                            self.danger
                        } else if active {
                            self.accent
                        } else {
                            self.muted
                        })
                        .child(if error {
                            format!("!{:>3}", index + 1)
                        } else if active {
                            format!("▶{:>3}", index + 1)
                        } else {
                            format!("{:>4}", index + 1)
                        }),
                );
            if selected_start < selected_end {
                row = row
                    .child(
                        div()
                            .text_color(self.foreground)
                            .child(self.content[line_start..selected_start].to_string()),
                    )
                    .child(
                        div()
                            .bg(Hsla {
                                a: 0.28,
                                ..self.accent
                            })
                            .child(self.content[selected_start..selected_end].to_string()),
                    )
                    .child(
                        div()
                            .text_color(self.foreground)
                            .child(self.content[selected_end..line_end].to_string()),
                    );
            } else if index == cursor_line {
                let before = &self.content[line_start..self.head.min(line_end)];
                let after = &self.content[self.head.min(line_end)..line_end];
                row = row
                    .child(div().text_color(self.foreground).child(before.to_string()))
                    .child(div().w(px(1.5)).h(px(15.)).flex_shrink_0().bg(self.accent))
                    .child(div().text_color(self.foreground).child(after.to_string()));
            } else {
                row = row.child(div().text_color(self.foreground).child((*line).to_string()));
            }
            let line_index = index;
            row = row
                .on_mouse_down(
                    MouseButton::Left,
                    cx.listener(move |this, event: &MouseDownEvent, window, cx| {
                        window.focus(&this.focus_handle);
                        this.begin_mouse_selection(line_index, event, cx);
                        cx.stop_propagation();
                    }),
                )
                .on_mouse_move(cx.listener(move |this, event: &MouseMoveEvent, _, cx| {
                    this.on_mouse_move(line_index, event, cx);
                    cx.stop_propagation();
                }))
                .on_mouse_up(
                    MouseButton::Left,
                    cx.listener(|this, _: &MouseUpEvent, _, cx| {
                        this.selecting = false;
                        cx.notify();
                    }),
                );
            rows = rows.child(row);
        }
        div()
            .size_full()
            .relative()
            .flex()
            .flex_col()
            .track_focus(&self.focus_handle)
            .cursor(CursorStyle::IBeam)
            .on_key_down(cx.listener(Self::on_key_down))
            .on_children_prepainted({
                let bounds = self.bounds.clone();
                move |children, _, _| {
                    if let Some(child) = children.first() {
                        bounds.set(Some(*child));
                    }
                }
            })
            .child(rows)
    }
}
