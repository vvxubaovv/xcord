//! A compact single-line text input for dialogs, search and the compose bar.
//!
//! It follows GPUI's [`EntityInputHandler`] contract so Windows IME (and CJK
//! composition on other platforms) is routed through the platform input stack.

use std::ops::Range;
use std::sync::atomic::{AtomicU64, Ordering};

use gpui::prelude::*;
use gpui::{
    div, fill, point, px, relative, size, Bounds, ClipboardItem, Context, CursorStyle,
    ElementInputHandler, Entity, EntityInputHandler, FocusHandle, Focusable, Hsla, KeyDownEvent,
    LayoutId, MouseButton, MouseDownEvent, MouseMoveEvent, MouseUpEvent, PaintQuad, Pixels, Point,
    Render, ShapedLine, SharedString, Style, TextRun, UTF16Selection, Window,
};
use unicode_segmentation::UnicodeSegmentation;

static NEXT_TEXT_INPUT: AtomicU64 = AtomicU64::new(1);

/// Simple styled single-line input.
pub struct TextInput {
    focus_handle: FocusHandle,
    element_id: SharedString,
    content: String,
    placeholder: SharedString,
    selected_range: Range<usize>,
    selection_reversed: bool,
    marked_range: Option<Range<usize>>,
    last_layout: Option<ShapedLine>,
    last_bounds: Option<Bounds<Pixels>>,
    is_selecting: bool,
    password: bool,
    foreground: Hsla,
    placeholder_color: Hsla,
    selection_color: Hsla,
    cursor_color: Hsla,
}

impl TextInput {
    pub fn new(
        placeholder: impl Into<SharedString>,
        password: bool,
        foreground: Hsla,
        placeholder_color: Hsla,
        selection_color: Hsla,
        cursor_color: Hsla,
        cx: &mut Context<Self>,
    ) -> Self {
        Self {
            focus_handle: cx.focus_handle(),
            element_id: format!(
                "text-input-{}",
                NEXT_TEXT_INPUT.fetch_add(1, Ordering::Relaxed)
            )
            .into(),
            content: String::new(),
            placeholder: placeholder.into(),
            selected_range: 0..0,
            selection_reversed: false,
            marked_range: None,
            last_layout: None,
            last_bounds: None,
            is_selecting: false,
            password,
            foreground,
            placeholder_color,
            selection_color,
            cursor_color,
        }
    }

    pub fn value(&self) -> &str {
        &self.content
    }

    pub fn set_value(&mut self, value: impl Into<String>, cx: &mut Context<Self>) {
        self.content = value.into();
        self.selected_range = self.content.len()..self.content.len();
        self.selection_reversed = false;
        self.marked_range = None;
        cx.notify();
    }

    pub fn clear(&mut self, cx: &mut Context<Self>) {
        self.set_value(String::new(), cx);
    }

    pub fn set_placeholder(
        &mut self,
        placeholder: impl Into<SharedString>,
        cx: &mut Context<Self>,
    ) {
        self.placeholder = placeholder.into();
        cx.notify();
    }

    pub fn focus_handle_only(&self) -> FocusHandle {
        self.focus_handle.clone()
    }

    fn cursor_offset(&self) -> usize {
        if self.selection_reversed {
            self.selected_range.start
        } else {
            self.selected_range.end
        }
    }

    fn previous_boundary(&self, offset: usize) -> usize {
        self.content
            .grapheme_indices(true)
            .rev()
            .find_map(|(index, _)| (index < offset).then_some(index))
            .unwrap_or(0)
    }

    fn next_boundary(&self, offset: usize) -> usize {
        self.content
            .grapheme_indices(true)
            .find_map(|(index, _)| (index > offset).then_some(index))
            .unwrap_or(self.content.len())
    }

    fn move_to(&mut self, offset: usize, cx: &mut Context<Self>) {
        self.selected_range = offset..offset;
        self.selection_reversed = false;
        self.marked_range = None;
        cx.notify();
    }

    fn select_to(&mut self, offset: usize, cx: &mut Context<Self>) {
        if self.selection_reversed {
            self.selected_range.start = offset;
        } else {
            self.selected_range.end = offset;
        }
        if self.selected_range.end < self.selected_range.start {
            self.selection_reversed = !self.selection_reversed;
            self.selected_range = self.selected_range.end..self.selected_range.start;
        }
        cx.notify();
    }

    fn select_all(&mut self, cx: &mut Context<Self>) {
        self.selected_range = 0..self.content.len();
        self.selection_reversed = false;
        cx.notify();
    }

    fn backspace(&mut self, word: bool, cx: &mut Context<Self>) {
        if self.selected_range.is_empty() {
            let start = if word {
                self.word_start(self.cursor_offset())
            } else {
                self.previous_boundary(self.cursor_offset())
            };
            if start == self.cursor_offset() {
                return;
            }
            self.select_to(start, cx);
        }
        self.delete_selection(cx);
    }

    fn delete(&mut self, word: bool, cx: &mut Context<Self>) {
        if self.selected_range.is_empty() {
            let end = if word {
                self.word_end(self.cursor_offset())
            } else {
                self.next_boundary(self.cursor_offset())
            };
            if end == self.cursor_offset() {
                return;
            }
            self.select_to(end, cx);
        }
        self.delete_selection(cx);
    }

    fn word_start(&self, offset: usize) -> usize {
        let prefix = &self.content[..offset.min(self.content.len())];
        let mut last = 0;
        for (index, grapheme) in prefix.grapheme_indices(true) {
            if grapheme.chars().all(char::is_whitespace) {
                last = index + grapheme.len();
            }
        }
        last
    }

    fn word_end(&self, offset: usize) -> usize {
        let suffix = &self.content[offset.min(self.content.len())..];
        for (index, grapheme) in suffix.grapheme_indices(true) {
            if grapheme.chars().all(char::is_whitespace) {
                return offset + index + grapheme.len();
            }
        }
        self.content.len()
    }

    fn delete_selection(&mut self, cx: &mut Context<Self>) {
        if self.selected_range.is_empty() {
            return;
        }
        let range = self.selected_range.clone();
        self.content.replace_range(range.clone(), "");
        self.selected_range = range.start..range.start;
        self.selection_reversed = false;
        cx.notify();
    }

    fn insert(&mut self, text: &str, cx: &mut Context<Self>) {
        let range = if self.selected_range.is_empty() {
            self.cursor_offset()..self.cursor_offset()
        } else {
            self.selected_range.clone()
        };
        self.content.replace_range(range.clone(), text);
        let cursor = range.start + text.len();
        self.selected_range = cursor..cursor;
        self.selection_reversed = false;
        self.marked_range = None;
        cx.notify();
    }

    fn copy(&self, cx: &mut Context<Self>) {
        if !self.selected_range.is_empty() && !self.password {
            cx.write_to_clipboard(ClipboardItem::new_string(
                self.content[self.selected_range.clone()].to_string(),
            ));
        }
    }

    fn cut(&mut self, cx: &mut Context<Self>) {
        if self.password {
            return;
        }
        if !self.selected_range.is_empty() {
            cx.write_to_clipboard(ClipboardItem::new_string(
                self.content[self.selected_range.clone()].to_string(),
            ));
            self.delete_selection(cx);
        }
    }

    fn paste(&mut self, cx: &mut Context<Self>) {
        if let Some(text) = cx.read_from_clipboard().and_then(|item| item.text()) {
            let single_line = text.replace(['\r', '\n'], " ");
            self.insert(&single_line, cx);
        }
    }

    fn on_key_down(&mut self, event: &KeyDownEvent, _window: &mut Window, cx: &mut Context<Self>) {
        let keystroke = &event.keystroke;
        let modifiers = keystroke.modifiers;
        let word = modifiers.control || modifiers.platform;
        let shift = modifiers.shift;

        if self.marked_range.is_some() && !word {
            cx.stop_propagation();
            return;
        }

        if word {
            match keystroke.key.as_str() {
                "a" => {
                    self.select_all(cx);
                    cx.stop_propagation();
                    return;
                }
                "c" => {
                    if self.selected_range.is_empty() {
                        return;
                    }
                    self.copy(cx);
                    cx.stop_propagation();
                    return;
                }
                "x" => {
                    self.cut(cx);
                    cx.stop_propagation();
                    return;
                }
                "v" => {
                    self.paste(cx);
                    cx.stop_propagation();
                    return;
                }
                _ => {}
            }
        }

        if shift && !word && keystroke.key.as_str() == "insert" {
            self.paste(cx);
            cx.stop_propagation();
            return;
        }

        match keystroke.key.as_str() {
            "backspace" => {
                self.backspace(word, cx);
                cx.stop_propagation();
            }
            "delete" => {
                self.delete(word, cx);
                cx.stop_propagation();
            }
            "left" => {
                let offset = if self.selected_range.is_empty() {
                    self.previous_boundary(self.cursor_offset())
                } else if !shift {
                    self.selected_range.start
                } else {
                    self.previous_boundary(self.selected_range.start)
                };
                if shift {
                    self.select_to(offset, cx);
                } else {
                    self.move_to(offset, cx);
                }
                cx.stop_propagation();
            }
            "right" => {
                let offset = if self.selected_range.is_empty() {
                    self.next_boundary(self.cursor_offset())
                } else if !shift {
                    self.selected_range.end
                } else {
                    self.next_boundary(self.selected_range.end)
                };
                if shift {
                    self.select_to(offset, cx);
                } else {
                    self.move_to(offset, cx);
                }
                cx.stop_propagation();
            }
            "home" => {
                self.move_to(0, cx);
                cx.stop_propagation();
            }
            "end" => {
                let end = self.content.len();
                self.move_to(end, cx);
                cx.stop_propagation();
            }
            "enter" => {}
            _ => {}
        }
    }

    fn on_mouse_down(
        &mut self,
        event: &MouseDownEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        window.focus(&self.focus_handle);
        self.is_selecting = true;
        let offset = self.index_for_mouse_position(event.position);
        if event.modifiers.shift {
            self.select_to(offset, cx);
        } else {
            self.move_to(offset, cx);
        }
    }

    fn on_mouse_up(&mut self, _: &MouseUpEvent, window: &mut Window, _: &mut Context<Self>) {
        // Focus on mouse-up as well as mouse-down. Some GPUI/Windows paths
        // deliver the up event when the window is activated but not the down
        // event, which made every field except the initially-focused one
        // impossible to click into.
        window.focus(&self.focus_handle);
        self.is_selecting = false;
    }

    fn on_mouse_up_out(&mut self, _: &MouseUpEvent, _: &mut Window, _: &mut Context<Self>) {
        self.is_selecting = false;
    }

    fn on_mouse_move(&mut self, event: &MouseMoveEvent, _: &mut Window, cx: &mut Context<Self>) {
        if self.is_selecting {
            let offset = self.index_for_mouse_position(event.position);
            self.select_to(offset, cx);
        }
    }

    fn index_for_mouse_position(&self, position: Point<Pixels>) -> usize {
        let (Some(bounds), Some(line)) = (self.last_bounds.as_ref(), self.last_layout.as_ref())
        else {
            return 0;
        };
        if position.y < bounds.top() {
            return 0;
        }
        if position.y > bounds.bottom() {
            return self.content.len();
        }
        let x = (position.x - bounds.left()).max(px(0.0));
        let display_index = line.closest_index_for_x(x);
        self.display_to_original(display_index)
    }

    fn char_index(&self, byte: usize) -> usize {
        self.content[..byte.min(self.content.len())].chars().count()
    }

    fn original_to_display(&self, byte: usize) -> usize {
        if self.password {
            self.char_index(byte) * '•'.len_utf8()
        } else {
            byte
        }
    }

    fn display_to_original(&self, display: usize) -> usize {
        if !self.password {
            return display.min(self.content.len());
        }
        let chars = display / '•'.len_utf8();
        self.content
            .char_indices()
            .nth(chars)
            .map(|(index, _)| index)
            .unwrap_or(self.content.len())
    }

    fn display_text(&self) -> (String, bool) {
        if self.password {
            ("•".repeat(self.content.chars().count()), true)
        } else {
            (self.content.clone(), false)
        }
    }
}

impl Render for TextInput {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        div()
            .id(self.element_id.clone())
            .w_full()
            .h(px(34.))
            .px(px(10.))
            .py(px(7.))
            .line_height(px(20.))
            .text_size(px(13.))
            .text_color(self.foreground)
            .track_focus(&self.focus_handle)
            .cursor(CursorStyle::IBeam)
            .on_key_down(cx.listener(Self::on_key_down))
            .on_mouse_down(MouseButton::Left, cx.listener(Self::on_mouse_down))
            .on_mouse_up(MouseButton::Left, cx.listener(Self::on_mouse_up))
            .on_mouse_up_out(MouseButton::Left, cx.listener(Self::on_mouse_up_out))
            .on_mouse_move(cx.listener(Self::on_mouse_move))
            .child(TextElement { input: cx.entity() })
    }
}

impl Focusable for TextInput {
    fn focus_handle(&self, _: &gpui::App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

struct TextElement {
    input: Entity<TextInput>,
}

struct TextPrepaint {
    line: Option<ShapedLine>,
    cursor: Option<PaintQuad>,
    selection: Option<PaintQuad>,
}

impl IntoElement for TextElement {
    type Element = Self;

    fn into_element(self) -> Self::Element {
        self
    }
}

impl gpui::Element for TextElement {
    type RequestLayoutState = ();
    type PrepaintState = TextPrepaint;

    fn id(&self) -> Option<gpui::ElementId> {
        None
    }

    fn source_location(&self) -> Option<&'static core::panic::Location<'static>> {
        None
    }

    fn request_layout(
        &mut self,
        _id: Option<&gpui::GlobalElementId>,
        _inspector_id: Option<&gpui::InspectorElementId>,
        window: &mut Window,
        cx: &mut gpui::App,
    ) -> (LayoutId, Self::RequestLayoutState) {
        let mut style = Style::default();
        style.size.width = relative(1.0).into();
        style.size.height = window.line_height().into();
        (window.request_layout(style, [], cx), ())
    }

    fn prepaint(
        &mut self,
        _id: Option<&gpui::GlobalElementId>,
        _inspector_id: Option<&gpui::InspectorElementId>,
        bounds: Bounds<Pixels>,
        _request_layout: &mut Self::RequestLayoutState,
        window: &mut Window,
        cx: &mut gpui::App,
    ) -> Self::PrepaintState {
        let input = self.input.read(cx);
        let style = window.text_style();
        let font_size = style.font_size.to_pixels(window.rem_size());
        let (display_text, _masked) = input.display_text();

        let (text, text_color) = if display_text.is_empty() {
            (input.placeholder.to_string(), input.placeholder_color)
        } else {
            (display_text, input.foreground)
        };

        let run = TextRun {
            len: text.len(),
            font: style.font(),
            color: text_color,
            background_color: None,
            underline: None,
            strikethrough: None,
        };
        let line = window
            .text_system()
            .shape_line(text.into(), font_size, &[run], None);

        let selected_start = input.original_to_display(input.selected_range.start);
        let selected_end = input.original_to_display(input.selected_range.end);
        let cursor_display = input.original_to_display(input.cursor_offset());

        let cursor = fill(
            Bounds::new(
                point(
                    bounds.left() + line.x_for_index(cursor_display),
                    bounds.top() + px(1.),
                ),
                size(px(1.5), bounds.bottom() - bounds.top() - px(2.)),
            ),
            input.cursor_color,
        );

        let selection = if input.selected_range.is_empty() {
            None
        } else {
            Some(fill(
                Bounds::from_corners(
                    point(
                        bounds.left() + line.x_for_index(selected_start),
                        bounds.top(),
                    ),
                    point(
                        bounds.left() + line.x_for_index(selected_end),
                        bounds.bottom(),
                    ),
                ),
                input.selection_color,
            ))
        };

        TextPrepaint {
            line: Some(line),
            cursor: Some(cursor),
            selection,
        }
    }

    fn paint(
        &mut self,
        _id: Option<&gpui::GlobalElementId>,
        _inspector_id: Option<&gpui::InspectorElementId>,
        bounds: Bounds<Pixels>,
        _request_layout: &mut Self::RequestLayoutState,
        prepaint: &mut Self::PrepaintState,
        window: &mut Window,
        cx: &mut gpui::App,
    ) {
        let focus_handle = self.input.read(cx).focus_handle.clone();
        window.handle_input(
            &focus_handle,
            ElementInputHandler::new(bounds, self.input.clone()),
            cx,
        );
        if let Some(selection) = prepaint.selection.take() {
            window.paint_quad(selection);
        }
        let line = prepaint.line.take().unwrap();
        line.paint(bounds.origin, window.line_height(), window, cx)
            .ok();

        if focus_handle.is_focused(window) {
            if let Some(cursor) = prepaint.cursor.take() {
                window.paint_quad(cursor);
            }
        }

        self.input.update(cx, |input, _cx| {
            input.last_layout = Some(line);
            input.last_bounds = Some(bounds);
        });
    }
}

impl EntityInputHandler for TextInput {
    fn text_for_range(
        &mut self,
        range_utf16: Range<usize>,
        actual_range: &mut Option<Range<usize>>,
        _window: &mut Window,
        _cx: &mut Context<Self>,
    ) -> Option<String> {
        let range = self.range_from_utf16(&range_utf16);
        actual_range.replace(self.range_to_utf16(&range));
        Some(self.content[range].to_string())
    }

    fn selected_text_range(
        &mut self,
        _ignore_disabled_input: bool,
        _window: &mut Window,
        _cx: &mut Context<Self>,
    ) -> Option<UTF16Selection> {
        Some(UTF16Selection {
            range: self.range_to_utf16(&self.selected_range),
            reversed: self.selection_reversed,
        })
    }

    fn marked_text_range(
        &self,
        _window: &mut Window,
        _cx: &mut Context<Self>,
    ) -> Option<Range<usize>> {
        self.marked_range
            .as_ref()
            .map(|range| self.range_to_utf16(range))
    }

    fn unmark_text(&mut self, _window: &mut Window, _cx: &mut Context<Self>) {
        self.marked_range = None;
    }

    fn replace_text_in_range(
        &mut self,
        range_utf16: Option<Range<usize>>,
        new_text: &str,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let range = range_utf16
            .as_ref()
            .map(|range| self.range_from_utf16(range))
            .or_else(|| self.marked_range.clone())
            .unwrap_or_else(|| self.selected_range.clone());
        self.content =
            (self.content[0..range.start].to_owned() + new_text + &self.content[range.end..])
                .into();
        self.selected_range = range.start + new_text.len()..range.start + new_text.len();
        self.marked_range = None;
        cx.notify();
    }

    fn replace_and_mark_text_in_range(
        &mut self,
        range_utf16: Option<Range<usize>>,
        new_text: &str,
        new_selected_range_utf16: Option<Range<usize>>,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let range = range_utf16
            .as_ref()
            .map(|range| self.range_from_utf16(range))
            .or_else(|| self.marked_range.clone())
            .unwrap_or_else(|| self.selected_range.clone());
        self.content =
            (self.content[0..range.start].to_owned() + new_text + &self.content[range.end..])
                .into();
        self.marked_range =
            (!new_text.is_empty()).then_some(range.start..range.start + new_text.len());
        self.selected_range = new_selected_range_utf16
            .as_ref()
            .map(|selection| self.range_from_utf16(selection))
            .map(|selection| range.start + selection.start..range.start + selection.end)
            .unwrap_or_else(|| range.start + new_text.len()..range.start + new_text.len());
        cx.notify();
    }

    fn bounds_for_range(
        &mut self,
        range_utf16: Range<usize>,
        bounds: Bounds<Pixels>,
        _window: &mut Window,
        _cx: &mut Context<Self>,
    ) -> Option<Bounds<Pixels>> {
        let line = self.last_layout.as_ref()?;
        let range = self.range_from_utf16(&range_utf16);
        let start = self.original_to_display(range.start);
        let end = self.original_to_display(range.end);
        Some(Bounds::from_corners(
            point(bounds.left() + line.x_for_index(start), bounds.top()),
            point(bounds.left() + line.x_for_index(end), bounds.bottom()),
        ))
    }

    fn character_index_for_point(
        &mut self,
        point: Point<Pixels>,
        _window: &mut Window,
        _cx: &mut Context<Self>,
    ) -> Option<usize> {
        let bounds = self.last_bounds?;
        let line = self.last_layout.as_ref()?;
        let local = bounds.localize(&point)?;
        let display = line.index_for_x(local.x - bounds.left())?;
        let original = self.display_to_original(display);
        Some(self.range_to_utf16(&(original..original)).start)
    }
}

impl TextInput {
    fn offset_from_utf16(&self, offset: usize) -> usize {
        let mut utf8_offset = 0;
        let mut utf16_count = 0;
        for ch in self.content.chars() {
            if utf16_count >= offset {
                break;
            }
            utf16_count += ch.len_utf16();
            utf8_offset += ch.len_utf8();
        }
        utf8_offset
    }

    fn offset_to_utf16(&self, offset: usize) -> usize {
        let mut utf16_offset = 0;
        let mut utf8_count = 0;
        for ch in self.content.chars() {
            if utf8_count >= offset {
                break;
            }
            utf8_count += ch.len_utf8();
            utf16_offset += ch.len_utf16();
        }
        utf16_offset
    }

    fn range_to_utf16(&self, range: &Range<usize>) -> Range<usize> {
        self.offset_to_utf16(range.start)..self.offset_to_utf16(range.end)
    }

    fn range_from_utf16(&self, range: &Range<usize>) -> Range<usize> {
        self.offset_from_utf16(range.start)..self.offset_from_utf16(range.end)
    }
}
