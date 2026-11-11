//! Minimal multiline editor for `.rhai` scripts.
//!
//! It intentionally keeps a single caret (no mouse range selection) so it can
//! stay dependency-free and predictable. Scripts are still selectable/copyable
//! from the terminal log and can be opened in an external editor.

use gpui::prelude::*;
use gpui::{
    div, px, Context, CursorStyle, FocusHandle, Focusable, Hsla, KeyDownEvent, MouseButton,
    MouseDownEvent, Render, ScrollHandle, Window,
};

pub struct ScriptEditor {
    focus_handle: FocusHandle,
    content: String,
    cursor: usize,
    active_line: Option<usize>,
    foreground: Hsla,
    muted: Hsla,
    accent: Hsla,
    scroll: ScrollHandle,
}

impl ScriptEditor {
    pub fn new(foreground: Hsla, muted: Hsla, accent: Hsla, cx: &mut Context<Self>) -> Self {
        Self {
            focus_handle: cx.focus_handle(),
            content: String::new(),
            cursor: 0,
            active_line: None,
            foreground,
            muted,
            accent,
            scroll: ScrollHandle::new(),
        }
    }

    pub fn value(&self) -> &str {
        &self.content
    }

    pub fn set_value(&mut self, value: impl Into<String>, cx: &mut Context<Self>) {
        self.content = value.into();
        self.cursor = self.content.len();
        self.scroll_to_cursor();
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

    fn cursor_line_column(&self) -> (usize, usize) {
        let before = &self.content[..self.cursor.min(self.content.len())];
        let line = before.matches('\n').count();
        let start = before.rfind('\n').map(|index| index + 1).unwrap_or(0);
        (line, self.cursor.saturating_sub(start))
    }

    fn scroll_to_cursor(&self) {
        let (line, _) = self.cursor_line_column();
        self.scroll.scroll_to_item(line);
    }

    fn line_bounds(&self, line: usize) -> (usize, usize) {
        let mut start = 0;
        for _ in 0..line {
            let Some(offset) = self.content[start..].find('\n') else {
                return (self.content.len(), self.content.len());
            };
            start += offset + 1;
        }
        let end = self.content[start..]
            .find('\n')
            .map(|offset| start + offset)
            .unwrap_or(self.content.len());
        (start, end)
    }

    fn move_vertical(&mut self, delta: isize, cx: &mut Context<Self>) {
        let (line, column) = self.cursor_line_column();
        let lines = self.content.split('\n').count();
        let target = (line as isize + delta).clamp(0, lines.saturating_sub(1) as isize) as usize;
        let (start, _) = self.line_bounds(target);
        let line_len = self.content[start..]
            .find('\n')
            .unwrap_or(self.content.len() - start);
        self.cursor = start + column.min(line_len);
        self.scroll_to_cursor();
        cx.notify();
    }

    fn previous_char(&self) -> usize {
        self.content[..self.cursor.min(self.content.len())]
            .char_indices()
            .next_back()
            .map(|(index, _)| index)
            .unwrap_or(0)
    }

    fn next_char(&self) -> usize {
        self.content[self.cursor.min(self.content.len())..]
            .char_indices()
            .nth(1)
            .map(|(index, _)| self.cursor + index)
            .unwrap_or(self.content.len())
    }

    fn insert(&mut self, text: &str, cx: &mut Context<Self>) {
        self.content.insert_str(self.cursor, text);
        self.cursor += text.len();
        self.scroll_to_cursor();
        cx.notify();
    }

    fn paste(&mut self, cx: &mut Context<Self>) {
        if let Some(text) = cx.read_from_clipboard().and_then(|item| item.text()) {
            let text = text.replace("\r\n", "\n").replace('\r', "\n");
            self.insert(&text, cx);
        }
    }

    fn on_key_down(&mut self, event: &KeyDownEvent, _window: &mut Window, cx: &mut Context<Self>) {
        let keystroke = &event.keystroke;
        let word = keystroke.modifiers.control || keystroke.modifiers.platform;
        if word {
            if keystroke.key == "v" {
                self.paste(cx);
                cx.stop_propagation();
            }
            return;
        }
        match keystroke.key.as_str() {
            "left" => {
                self.cursor = self.previous_char();
                self.scroll_to_cursor();
                cx.notify();
            }
            "right" => {
                self.cursor = self.next_char();
                self.scroll_to_cursor();
                cx.notify();
            }
            "up" => self.move_vertical(-1, cx),
            "down" => self.move_vertical(1, cx),
            "home" => {
                let (line, _) = self.cursor_line_column();
                self.cursor = self.line_bounds(line).0;
                self.scroll_to_cursor();
                cx.notify();
            }
            "end" => {
                let (line, _) = self.cursor_line_column();
                self.cursor = self.line_bounds(line).1;
                self.scroll_to_cursor();
                cx.notify();
            }
            "backspace" => {
                if self.cursor > 0 {
                    let start = self.previous_char();
                    self.content.replace_range(start..self.cursor, "");
                    self.cursor = start;
                    self.scroll_to_cursor();
                    cx.notify();
                }
            }
            "delete" => {
                if self.cursor < self.content.len() {
                    let end = self.next_char();
                    self.content.replace_range(self.cursor..end, "");
                    cx.notify();
                }
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
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let (cursor_line, cursor_column) = self.cursor_line_column();
        let lines = self.content.split('\n').collect::<Vec<_>>();
        let mut rows = div()
            .id("script-editor-lines")
            .flex()
            .flex_col()
            .p_2()
            .gap_0()
            .flex_1()
            .min_h(px(0.))
            .track_scroll(&self.scroll)
            .overflow_y_scroll()
            .font_family("Consolas")
            .text_size(px(11.5));
        for (index, line) in lines.iter().enumerate() {
            let active = self.active_line == Some(index + 1);
            let mut row = div()
                .w_full()
                .flex()
                .items_start()
                .h(px(18.))
                .whitespace_nowrap()
                .overflow_hidden()
                .rounded_sm()
                .when(active, |row| {
                    row.bg(Hsla {
                        a: 0.18,
                        ..self.accent
                    })
                })
                .child(
                    div()
                        .w(px(42.))
                        .flex_shrink_0()
                        .text_color(if active { self.accent } else { self.muted })
                        .child(if active {
                            format!("▶{:>3}", index + 1)
                        } else {
                            format!("{:>4}", index + 1)
                        }),
                );
            if index == cursor_line {
                let (start, _) = self.line_bounds(index);
                let before = &self.content[start..self.cursor.min(self.content.len())];
                let after = &self.content[self.cursor.min(self.content.len())..]
                    .split('\n')
                    .next()
                    .unwrap_or("");
                row = row
                    .child(div().text_color(self.foreground).child(before.to_string()))
                    .child(div().w(px(1.5)).h(px(15.)).flex_shrink_0().bg(self.accent))
                    .child(div().text_color(self.foreground).child(after.to_string()));
            } else {
                row = row.child(div().text_color(self.foreground).child((*line).to_string()));
            }
            rows = rows.child(row);
        }
        let _ = cursor_column;
        div()
            .size_full()
            .relative()
            .flex()
            .flex_col()
            .track_focus(&self.focus_handle)
            .cursor(CursorStyle::IBeam)
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(|this, _: &MouseDownEvent, window, cx| {
                    window.focus(&this.focus_handle);
                    cx.stop_propagation();
                    cx.notify();
                }),
            )
            .on_key_down(cx.listener(Self::on_key_down))
            .child(rows)
    }
}
