//! Read-only text with mouse selection and clipboard support.
//!
//! GPUI does not ship a selectable text element, so this wraps `StyledText` and
//! paints a selection background using the same `TextLayout` that shapes the
//! text. Selection is byte-based; mouse positions map through
//! `TextLayout::index_for_position`.

use std::cell::{Cell, RefCell};
use std::rc::Rc;

use gpui::prelude::*;
use gpui::{
    div, fill, point, px, size, App, Bounds, ClipboardItem, Context, CursorStyle, Element,
    ElementId, FocusHandle, Focusable, GlobalElementId, InspectorElementId, LayoutId, MouseButton,
    MouseDownEvent, MouseMoveEvent, MouseUpEvent, Pixels, Render, Rgba, SharedString, StyledText,
    TextLayout, TextRun, Window,
};

pub struct SelectableText {
    id: usize,
    owner: Rc<Cell<Option<usize>>>,
    text: String,
    runs: Option<Vec<TextRun>>,
    color: Rgba,
    font_size: f32,
    monospace: bool,
    selection_color: Rgba,
    focus_handle: FocusHandle,
    selection: Option<(usize, usize)>,
    selecting: bool,
    layout: Rc<RefCell<Option<TextLayout>>>,
}

impl SelectableText {
    pub fn new(
        id: usize,
        owner: Rc<Cell<Option<usize>>>,
        text: String,
        color: Rgba,
        font_size: f32,
        selection_color: Rgba,
        cx: &mut Context<Self>,
    ) -> Self {
        Self {
            id,
            owner,
            text,
            runs: None,
            color,
            font_size,
            monospace: false,
            selection_color,
            focus_handle: cx.focus_handle(),
            selection: None,
            selecting: false,
            layout: Rc::new(RefCell::new(None)),
        }
    }

    pub fn set_runs(&mut self, runs: Vec<TextRun>, cx: &mut Context<Self>) {
        self.runs = Some(runs);
        cx.notify();
    }

    pub fn set_text(&mut self, text: String, cx: &mut Context<Self>) {
        if self.text == text {
            return;
        }
        self.text = text;
        self.runs = None;
        if let Some((start, end)) = self.selection {
            let len = self.text.len();
            self.selection = Some((start.min(len), end.min(len)));
        }
        cx.notify();
    }

    pub fn set_monospace(&mut self, monospace: bool, cx: &mut Context<Self>) {
        if self.monospace == monospace {
            return;
        }
        self.monospace = monospace;
        cx.notify();
    }

    pub fn set_style(&mut self, color: Rgba, font_size: f32, cx: &mut Context<Self>) {
        if self.color == color && self.font_size == font_size {
            return;
        }
        self.color = color;
        self.font_size = font_size;
        cx.notify();
    }

    pub fn selected_text(&self) -> Option<String> {
        let (a, b) = self.selection?;
        let (start, end) = (a.min(b), a.max(b));
        if start == end || !self.text.is_char_boundary(start) || !self.text.is_char_boundary(end) {
            return None;
        }
        Some(self.text[start..end].to_string())
    }

    fn clear_selection(&mut self, cx: &mut Context<Self>) {
        self.selection = None;
        self.selecting = false;
        if self.owner.get() == Some(self.id) {
            self.owner.set(None);
        }
        cx.notify();
    }

    fn copy_selection(&self, cx: &mut Context<Self>) -> bool {
        let Some(text) = self.selected_text() else {
            return false;
        };
        if text.is_empty() {
            return false;
        }
        cx.write_to_clipboard(ClipboardItem::new_string(text));
        true
    }

    fn index_for_position(&self, position: gpui::Point<Pixels>) -> Option<usize> {
        let layout = self.layout.borrow().clone()?;
        Some(
            layout
                .index_for_position(position)
                .unwrap_or_else(|index| index),
        )
    }

    fn on_mouse_down(
        &mut self,
        event: &MouseDownEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if event.button != MouseButton::Left {
            return;
        }
        let Some(index) = self.index_for_position(event.position) else {
            return;
        };
        self.owner.set(Some(self.id));
        self.selection = Some((index, index));
        self.selecting = true;
        window.focus(&self.focus_handle);
        cx.stop_propagation();
        cx.notify();
    }

    fn on_mouse_move(
        &mut self,
        event: &MouseMoveEvent,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if !self.selecting {
            return;
        }
        let Some(index) = self.index_for_position(event.position) else {
            return;
        };
        if let Some((anchor, _)) = self.selection {
            self.selection = Some((anchor, index));
            self.owner.set(Some(self.id));
            cx.notify();
        }
    }

    fn on_mouse_up(&mut self, _: &MouseUpEvent, _: &mut Window, cx: &mut Context<Self>) {
        self.selecting = false;
        if self.selection.map(|(a, b)| a == b).unwrap_or(false) {
            self.selection = None;
            if self.owner.get() == Some(self.id) {
                self.owner.set(None);
            }
        }
        cx.notify();
    }

    fn on_key_down(&mut self, event: &gpui::KeyDownEvent, _: &mut Window, cx: &mut Context<Self>) {
        let control = event.keystroke.modifiers.control || event.keystroke.modifiers.platform;
        if !control {
            return;
        }
        match event.keystroke.key.as_str() {
            "c" => {
                if self.copy_selection(cx) {
                    cx.stop_propagation();
                }
            }
            "a" => {
                self.selection = Some((0, self.text.len()));
                self.owner.set(Some(self.id));
                cx.stop_propagation();
                cx.notify();
            }
            "escape" => {
                self.clear_selection(cx);
                cx.stop_propagation();
            }
            _ => {}
        }
    }
}

impl Render for SelectableText {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let text: SharedString = self.text.clone().into();
        let selection = if self.owner.get() == Some(self.id) {
            self.selection
        } else {
            None
        };
        let inner = if let Some(runs) = self.runs.clone() {
            StyledText::new(text.clone()).with_runs(runs)
        } else {
            StyledText::new(text.clone())
        };
        let element = SelectableTextElement {
            inner,
            text,
            selection,
            selection_color: self.selection_color,
            layout_out: self.layout.clone(),
            owner: self.owner.clone(),
            id: self.id,
        };
        div()
            .w_full()
            .min_w(px(0.))
            .text_size(px(self.font_size))
            .font_family(if self.monospace {
                "Consolas"
            } else {
                "Microsoft YaHei UI"
            })
            .line_height(px(self.font_size * 1.45))
            .text_color(self.color)
            .track_focus(&self.focus_handle)
            .cursor(CursorStyle::IBeam)
            .on_key_down(cx.listener(Self::on_key_down))
            .on_mouse_down(MouseButton::Left, cx.listener(Self::on_mouse_down))
            .on_mouse_move(cx.listener(Self::on_mouse_move))
            .on_mouse_up(MouseButton::Left, cx.listener(Self::on_mouse_up))
            .on_mouse_up_out(MouseButton::Left, cx.listener(Self::on_mouse_up))
            .child(element)
    }
}

struct SelectableTextElement {
    inner: StyledText,
    text: SharedString,
    selection: Option<(usize, usize)>,
    selection_color: Rgba,
    layout_out: Rc<RefCell<Option<TextLayout>>>,
    owner: Rc<Cell<Option<usize>>>,
    id: usize,
}

impl IntoElement for SelectableTextElement {
    type Element = Self;

    fn into_element(self) -> Self::Element {
        self
    }
}

impl Element for SelectableTextElement {
    type RequestLayoutState = ();
    type PrepaintState = ();

    fn id(&self) -> Option<ElementId> {
        None
    }

    fn source_location(&self) -> Option<&'static std::panic::Location<'static>> {
        None
    }

    fn request_layout(
        &mut self,
        id: Option<&GlobalElementId>,
        inspector_id: Option<&InspectorElementId>,
        window: &mut Window,
        cx: &mut App,
    ) -> (LayoutId, Self::RequestLayoutState) {
        self.inner.request_layout(id, inspector_id, window, cx)
    }

    fn prepaint(
        &mut self,
        id: Option<&GlobalElementId>,
        inspector_id: Option<&InspectorElementId>,
        bounds: Bounds<Pixels>,
        state: &mut Self::RequestLayoutState,
        window: &mut Window,
        cx: &mut App,
    ) {
        self.inner
            .prepaint(id, inspector_id, bounds, state, window, cx);
        *self.layout_out.borrow_mut() = Some(self.inner.layout().clone());
    }

    fn paint(
        &mut self,
        id: Option<&GlobalElementId>,
        inspector_id: Option<&InspectorElementId>,
        bounds: Bounds<Pixels>,
        state: &mut Self::RequestLayoutState,
        _prepaint: &mut Self::PrepaintState,
        window: &mut Window,
        cx: &mut App,
    ) {
        if self.owner.get() == Some(self.id) {
            if let Some((start, end)) = self.selection {
                let layout = self.inner.layout().clone();
                let (start, end) = (start.min(self.text.len()), end.min(self.text.len()));
                let (start, end) = if start <= end {
                    (start, end)
                } else {
                    (end, start)
                };
                let line_height = layout.line_height();
                for (index, ch) in self.text.char_indices() {
                    if index < start || index >= end || ch == '\n' || ch == '\r' {
                        continue;
                    }
                    let Some(from) = layout.position_for_index(index) else {
                        continue;
                    };
                    let Some(to) = layout.position_for_index(index + ch.len_utf8()) else {
                        continue;
                    };
                    let left = from.x.min(to.x);
                    let width = (to.x - from.x).abs();
                    if width <= px(0.) {
                        continue;
                    }
                    window.paint_quad(fill(
                        Bounds::new(point(left, from.y), size(width, line_height)),
                        self.selection_color,
                    ));
                }
            }
        }
        self.inner
            .paint(id, inspector_id, bounds, state, _prepaint, window, cx)
    }
}

impl Focusable for SelectableText {
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}
