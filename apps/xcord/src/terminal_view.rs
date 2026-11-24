//! GPU-rendered terminal grid with selection, scrollback and xterm key encoding.

use std::cell::Cell;
use std::ops::Range;
use std::rc::Rc;
use std::time::{Duration, Instant};

use gpui::prelude::*;
use gpui::{
    canvas, div, fill, point, px, rgb, rgba, size, App, Bounds, Context, CursorStyle,
    ElementInputHandler, EntityInputHandler, FocusHandle, Focusable, Font, FontWeight,
    KeyDownEvent, Keystroke, MouseButton, MouseDownEvent, MouseMoveEvent, MouseUpEvent, Pixels,
    Point, Render, ScrollDelta, ScrollWheelEvent, ShapedLine, TextRun, UTF16Selection,
    UnderlineStyle, Window,
};
use xcord_plugin_api::{HighlightSpan, Highlights, TerminalId, Themes, Ui, UiCommand, Workspace};
use xcord_store::Settings;
use xcord_vt::{default_palette, Color, Flags};

#[derive(Clone, Copy, Debug, Default)]
struct GridMetrics {
    bounds: Bounds<Pixels>,
    cell_width: f32,
    cell_height: f32,
    cols: u16,
    rows: u16,
}

impl GridMetrics {
    fn cell_at(&self, position: Point<Pixels>) -> (usize, usize) {
        if self.cell_width <= 0.0 || self.cell_height <= 0.0 {
            return (0, 0);
        }
        let local = position - self.bounds.origin;
        let col = (f32::from(local.x) / self.cell_width).floor().max(0.0) as usize;
        let row = (f32::from(local.y) / self.cell_height).floor().max(0.0) as usize;
        (
            row.min(self.rows.saturating_sub(1) as usize),
            col.min(self.cols.saturating_sub(1) as usize),
        )
    }
}

#[derive(Clone, Copy, Debug)]
struct Selection {
    anchor: (usize, usize),
    head: (usize, usize),
}

impl Selection {
    fn normalized(&self) -> ((usize, usize), (usize, usize)) {
        if self.anchor.0 < self.head.0
            || (self.anchor.0 == self.head.0 && self.anchor.1 <= self.head.1)
        {
            (self.anchor, self.head)
        } else {
            (self.head, self.anchor)
        }
    }

    fn is_empty(&self) -> bool {
        self.anchor == self.head
    }
}

struct ThemeColors {
    background: [u8; 3],
    foreground: [u8; 3],
    cursor: [u8; 3],
    selection: [u8; 3],
    palette: [[u8; 3]; 16],
}

pub struct TerminalView {
    focus_handle: FocusHandle,
    workspace: Workspace,
    themes: Themes,
    highlights: Highlights,
    ui: Ui,
    settings: Settings,
    active: Option<TerminalId>,
    metrics: Rc<Cell<GridMetrics>>,
    selection: Option<Selection>,
    selecting: bool,
    scrollbar_drag: Option<f32>,
    scroll_accumulator: f32,
    ime_text: String,
    suppress_ctrl_c_until: Option<Instant>,
}

impl TerminalView {
    pub fn new(
        workspace: Workspace,
        themes: Themes,
        highlights: Highlights,
        ui: Ui,
        settings: Settings,
        cx: &mut Context<Self>,
    ) -> Self {
        Self {
            focus_handle: cx.focus_handle(),
            workspace,
            themes,
            highlights,
            ui,
            settings,
            active: None,
            metrics: Rc::new(Cell::new(GridMetrics::default())),
            selection: None,
            selecting: false,
            scrollbar_drag: None,
            scroll_accumulator: 0.0,
            ime_text: String::new(),
            suppress_ctrl_c_until: None,
        }
    }

    pub fn set_active(&mut self, id: Option<TerminalId>, cx: &mut Context<Self>) {
        if self.active != id {
            self.active = id;
            self.selection = None;
            self.selecting = false;
            cx.notify();
        }
    }

    pub fn set_settings(&mut self, settings: Settings, cx: &mut Context<Self>) {
        self.settings = settings;
        cx.notify();
    }

    pub fn set_highlights(&mut self, highlights: Highlights, cx: &mut Context<Self>) {
        self.highlights = highlights;
        cx.notify();
    }

    pub fn focus_handle(&self) -> FocusHandle {
        self.focus_handle.clone()
    }

    fn copy_selection(&self, cx: &mut Context<Self>) {
        let Some(id) = self.active else {
            return;
        };
        let Some(selection) = self.selection else {
            return;
        };
        if selection.is_empty() {
            return;
        }
        let (start, end) = selection.normalized();
        let text = self.workspace.text_in_viewport(
            id,
            (start.0, start.1),
            (
                end.0,
                (end.1 + 1).min(self.metrics.get().cols.max(1) as usize),
            ),
        );
        if !text.is_empty() {
            cx.write_to_clipboard(gpui::ClipboardItem::new_string(text));
        }
    }

    fn paste(&mut self, cx: &mut Context<Self>) {
        let Some(id) = self.active else {
            return;
        };
        let Some(text) = cx.read_from_clipboard().and_then(|item| item.text()) else {
            return;
        };
        if text.is_empty() {
            return;
        }
        let normalized = text.replace("\r\n", "\r").replace('\n', "\r");
        let bridged = self
            .workspace
            .snapshot(id)
            .map(|snapshot| snapshot.modes.bracketed_paste)
            .unwrap_or(false);
        if bridged {
            self.workspace.write(id, b"\x1b[200~");
            self.workspace.write(id, normalized.as_bytes());
            self.workspace.write(id, b"\x1b[201~");
        } else {
            self.workspace.write(id, normalized.as_bytes());
        }
        self.workspace.scroll_to_bottom(id);
    }

    fn on_key_down(&mut self, event: &KeyDownEvent, _window: &mut Window, cx: &mut Context<Self>) {
        let keystroke = &event.keystroke;
        let control = keystroke.modifiers.control || keystroke.modifiers.platform;
        let shift = keystroke.modifiers.shift;
        let key = keystroke.key.as_str();

        if !control && !keystroke.modifiers.alt && !self.ime_text.is_empty() {
            // Never consume keys while the IME owns composition; returning
            // lets GPUI translate them back to the IME.
            return;
        }

        // Do not handle printable keys here. Let the platform translate the
        // key into WM_CHAR/WM_IME messages; the input handler below receives
        // both normal text and IME commits. Stopping propagation here would
        // prevent translation and therefore also prevent IME activation.
        if !control && !keystroke.modifiers.alt {
            let printable = keystroke
                .key_char
                .as_ref()
                .is_some_and(|text| text.chars().any(|ch| !ch.is_control()))
                || keystroke.key.chars().count() == 1
                || keystroke.key == "space";
            if printable {
                return;
            }
        }

        if control && !shift && key == "v" {
            self.paste(cx);
            cx.stop_propagation();
            return;
        }
        if shift && !control && key == "insert" {
            self.paste(cx);
            cx.stop_propagation();
            return;
        }

        if control && !shift && key == "c" {
            let has_selection = self
                .selection
                .as_ref()
                .map(|selection| !selection.is_empty())
                .unwrap_or(false);
            let after_double_click = self
                .suppress_ctrl_c_until
                .is_some_and(|until| Instant::now() < until);
            if after_double_click {
                self.suppress_ctrl_c_until = None;
                if has_selection {
                    self.copy_selection(cx);
                }
                cx.stop_propagation();
                return;
            }
            if has_selection {
                self.copy_selection(cx);
                cx.stop_propagation();
                return;
            }
        }

        if control && shift {
            match key {
                "t" => {
                    self.ui.send(UiCommand::NewSession);
                    cx.stop_propagation();
                    return;
                }
                "w" => {
                    self.ui.send(UiCommand::CloseActiveTab);
                    cx.stop_propagation();
                    return;
                }
                "p" => {
                    self.ui.send(UiCommand::OpenPalette);
                    cx.stop_propagation();
                    return;
                }
                "e" => {
                    self.ui.send(UiCommand::FocusCompose);
                    cx.stop_propagation();
                    return;
                }
                "i" => {
                    self.ui.send(UiCommand::ToggleAi);
                    cx.stop_propagation();
                    return;
                }
                "c" => {
                    self.copy_selection(cx);
                    cx.stop_propagation();
                    return;
                }
                "v" => {
                    self.paste(cx);
                    cx.stop_propagation();
                    return;
                }
                "tab" => {
                    self.ui.send(UiCommand::SelectPrevTab);
                    cx.stop_propagation();
                    return;
                }
                _ => {}
            }
        }

        if control && key == "tab" {
            self.ui.send(UiCommand::SelectNextTab);
            cx.stop_propagation();
            return;
        }

        let Some(id) = self.active else {
            return;
        };
        let Some(snapshot) = self.workspace.snapshot(id) else {
            return;
        };
        if let Some(bytes) = encode_key(keystroke, snapshot.modes.application_cursor) {
            self.workspace.write(id, &bytes);
            self.workspace.scroll_to_bottom(id);
        }
        cx.stop_propagation();
    }

    fn select_word_at(&mut self, position: Point<Pixels>, cx: &mut Context<Self>) {
        let Some(id) = self.active else {
            return;
        };
        let Some(snapshot) = self.workspace.snapshot(id) else {
            return;
        };
        let (row, col) = self.metrics.get().cell_at(position);
        let Some(line) = snapshot.lines.get(row) else {
            return;
        };
        if line.cells.is_empty() {
            return;
        }
        let target = col.min(line.cells.len() - 1);
        if line.cells[target].ch == ' ' {
            self.selection = None;
            cx.notify();
            return;
        }
        let is_word = |ch: char| {
            ch.is_alphanumeric() || matches!(ch, '_' | '-' | '.' | '/' | '\\' | ':' | '@' | '~')
        };
        let target_is_word = is_word(line.cells[target].ch);
        let mut start = target;
        let mut end = target;
        if target_is_word {
            while start > 0 && is_word(line.cells[start - 1].ch) {
                start -= 1;
            }
            while end + 1 < line.cells.len() && is_word(line.cells[end + 1].ch) {
                end += 1;
            }
        }
        let end_cell = &line.cells[end];
        let head = end + (end_cell.width.max(1) as usize) - 1;
        self.selection = Some(Selection {
            anchor: (row, start),
            head: (row, head),
        });
        cx.notify();
    }

    fn on_mouse_down(
        &mut self,
        event: &MouseDownEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        window.focus(&self.focus_handle);
        if event.button == MouseButton::Right {
            let has_selection = self
                .selection
                .as_ref()
                .map(|selection| !selection.is_empty())
                .unwrap_or(false);
            if has_selection {
                self.copy_selection(cx);
                cx.stop_propagation();
            }
            return;
        }
        if event.button != MouseButton::Left {
            return;
        }
        if self.begin_scrollbar_drag(event.position, cx) {
            cx.notify();
            cx.stop_propagation();
            return;
        }
        if event.click_count >= 2 {
            self.selecting = false;
            self.select_word_at(event.position, cx);
            self.suppress_ctrl_c_until = Some(Instant::now() + Duration::from_millis(800));
            cx.stop_propagation();
            return;
        }
        let (row, col) = self.metrics.get().cell_at(event.position);
        self.selecting = true;
        self.selection = Some(Selection {
            anchor: (row, col),
            head: (row, col),
        });
        cx.notify();
    }

    fn on_mouse_move(
        &mut self,
        event: &MouseMoveEvent,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.scrollbar_drag.is_some() {
            self.update_scrollbar_drag(event.position, cx);
            cx.notify();
            return;
        }
        if !self.selecting {
            return;
        }
        let (row, col) = self.metrics.get().cell_at(event.position);
        if let Some(selection) = &mut self.selection {
            selection.head = (row, col);
        }
        cx.notify();
    }

    fn on_mouse_up(&mut self, event: &MouseUpEvent, _: &mut Window, cx: &mut Context<Self>) {
        if self.scrollbar_drag.take().is_some() {
            cx.notify();
            return;
        }
        if event.click_count >= 2 {
            self.selecting = false;
            if self.settings.copy_on_select {
                self.copy_selection(cx);
            }
            cx.notify();
            return;
        }
        if !self.selecting {
            return;
        }
        self.selecting = false;
        if self.settings.copy_on_select {
            self.copy_selection(cx);
        }
        cx.notify();
    }

    fn scrollbar_snapshot(&self) -> Option<(TerminalId, Bounds<Pixels>, usize, usize)> {
        let id = self.active?;
        let metrics = self.metrics.get();
        let rows = metrics.rows as usize;
        if rows == 0 || metrics.cell_height <= 0.0 {
            return None;
        }
        let snapshot = self.workspace.snapshot(id)?;
        if snapshot.scrollback_len == 0 {
            return None;
        }
        Some((id, metrics.bounds, rows, snapshot.scrollback_len))
    }

    fn scrollbar_ratio(&self, y: Pixels, bounds: Bounds<Pixels>, rows: usize) -> f32 {
        let track = rows as f32 * self.metrics.get().cell_height.max(1.0);
        if track <= 0.0 {
            return 0.0;
        }
        (f32::from(y - bounds.top()) / track).clamp(0.0, 1.0)
    }

    fn begin_scrollbar_drag(&mut self, position: Point<Pixels>, cx: &mut Context<Self>) -> bool {
        let Some((id, bounds, rows, scrollback)) = self.scrollbar_snapshot() else {
            return false;
        };
        let metrics = self.metrics.get();
        let local_x = f32::from(position.x - bounds.left());
        if local_x < (metrics.cols as f32 * metrics.cell_width - 12.0).max(0.0) {
            return false;
        }
        let ratio = self.scrollbar_ratio(position.y, bounds, rows);
        let max_top = scrollback;
        let top_lines = (ratio * max_top as f32).round() as usize;
        let scroll = scrollback.saturating_sub(top_lines);
        self.workspace.scroll_to(id, scroll);
        self.scrollbar_drag = Some(ratio);
        cx.notify();
        true
    }

    fn update_scrollbar_drag(&mut self, position: Point<Pixels>, _cx: &mut Context<Self>) {
        let Some((id, bounds, rows, scrollback)) = self.scrollbar_snapshot() else {
            return;
        };
        let ratio = self.scrollbar_ratio(position.y, bounds, rows);
        let max_top = scrollback;
        let top_lines = (ratio * max_top as f32).round() as usize;
        let scroll = scrollback.saturating_sub(top_lines);
        self.workspace.scroll_to(id, scroll);
    }

    fn on_scroll(
        &mut self,
        event: &ScrollWheelEvent,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(id) = self.active else {
            return;
        };
        let lines = match event.delta {
            ScrollDelta::Lines(delta) => f32::from(delta.y) * 3.0,
            ScrollDelta::Pixels(delta) => {
                f32::from(delta.y) / self.metrics.get().cell_height.max(1.0)
            }
        };
        self.scroll_accumulator += lines;
        let steps = self.scroll_accumulator.trunc();
        if steps.abs() >= 1.0 {
            self.scroll_accumulator -= steps;
            self.workspace.scroll_by(id, steps as i32);
        }
        cx.stop_propagation();
    }
}

impl Render for TerminalView {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = self.themes.active();
        let colors = ThemeColors {
            background: theme.background,
            foreground: theme.foreground,
            cursor: theme.cursor,
            selection: theme.selection,
            palette: theme.palette,
        };
        let id = self.active;
        let workspace = self.workspace.clone();
        let highlights = self.highlights.clone();
        let settings = self.settings.clone();
        let metrics = self.metrics.clone();
        let selection = self.selection;
        let font_family = settings.font_family.clone();
        let font_size = settings.font_size;
        let line_height = settings.line_height;
        let background = colors.background;
        let input_view = cx.entity();
        let input_focus = self.focus_handle.clone();

        div()
            .size_full()
            .relative()
            .overflow_hidden()
            .bg(rgb(color_u32(background)))
            .cursor(CursorStyle::IBeam)
            .track_focus(&self.focus_handle)
            .key_context("Terminal")
            .on_key_down(cx.listener(Self::on_key_down))
            .on_mouse_down(MouseButton::Left, cx.listener(Self::on_mouse_down))
            .on_mouse_move(cx.listener(Self::on_mouse_move))
            .on_mouse_up(MouseButton::Left, cx.listener(Self::on_mouse_up))
            .on_mouse_up_out(MouseButton::Left, cx.listener(Self::on_mouse_up))
            .on_scroll_wheel(cx.listener(Self::on_scroll))
            .child(
                canvas(
                    move |bounds, window, _cx| {
                        prepare(
                            bounds,
                            id,
                            &workspace,
                            &highlights,
                            &settings,
                            &font_family,
                            font_size,
                            line_height,
                            colors,
                            selection,
                            metrics,
                            window,
                        )
                    },
                    move |_bounds, paint, window, cx| {
                        let bounds = paint.bounds;
                        paint_terminal(paint, window, cx);
                        window.handle_input(
                            &input_focus,
                            ElementInputHandler::new(bounds, input_view.clone()),
                            cx,
                        );
                    },
                )
                .size_full(),
            )
    }
}

impl EntityInputHandler for TerminalView {
    fn text_for_range(
        &mut self,
        _range_utf16: Range<usize>,
        _actual_range: &mut Option<Range<usize>>,
        _window: &mut Window,
        _cx: &mut Context<Self>,
    ) -> Option<String> {
        Some(self.ime_text.clone())
    }

    fn selected_text_range(
        &mut self,
        _ignore_disabled_input: bool,
        _window: &mut Window,
        _cx: &mut Context<Self>,
    ) -> Option<UTF16Selection> {
        Some(UTF16Selection {
            range: 0..0,
            reversed: false,
        })
    }

    fn marked_text_range(
        &self,
        _window: &mut Window,
        _cx: &mut Context<Self>,
    ) -> Option<Range<usize>> {
        (!self.ime_text.is_empty()).then(|| 0..self.ime_text.encode_utf16().count())
    }

    fn unmark_text(&mut self, _window: &mut Window, cx: &mut Context<Self>) {
        self.ime_text.clear();
        cx.notify();
    }

    fn replace_text_in_range(
        &mut self,
        _range_utf16: Option<Range<usize>>,
        new_text: &str,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.ime_text.clear();
        let text = new_text
            .chars()
            .filter(|ch| !ch.is_control())
            .collect::<String>();
        if !text.is_empty() {
            if let Some(id) = self.active {
                self.workspace.write(id, text.as_bytes());
                self.workspace.scroll_to_bottom(id);
            }
        }
        cx.notify();
    }

    fn replace_and_mark_text_in_range(
        &mut self,
        _range_utf16: Option<Range<usize>>,
        new_text: &str,
        _new_selected_range_utf16: Option<Range<usize>>,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.ime_text = new_text.to_string();
        cx.notify();
    }

    fn bounds_for_range(
        &mut self,
        _range_utf16: Range<usize>,
        bounds: Bounds<Pixels>,
        _window: &mut Window,
        _cx: &mut Context<Self>,
    ) -> Option<Bounds<Pixels>> {
        let (row, col) = self
            .active
            .and_then(|id| self.workspace.snapshot(id))
            .and_then(|snapshot| snapshot.cursor)
            .unwrap_or((0, 0));
        let metrics = self.metrics.get();
        Some(Bounds::new(
            point(
                bounds.left() + px(col as f32 * metrics.cell_width),
                bounds.top() + px(row as f32 * metrics.cell_height),
            ),
            size(
                px(metrics.cell_width.max(1.0)),
                px(metrics.cell_height.max(1.0)),
            ),
        ))
    }

    fn character_index_for_point(
        &mut self,
        _point: Point<Pixels>,
        _window: &mut Window,
        _cx: &mut Context<Self>,
    ) -> Option<usize> {
        Some(0)
    }
}

impl Focusable for TerminalView {
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

struct RowBackground {
    start_cell: usize,
    end_cell: usize,
    color: [u8; 3],
}

struct CursorPaint {
    row: usize,
    col: usize,
    style: xcord_vt::CursorStyle,
}

struct TerminalPaint {
    lines: Vec<Option<ShapedLine>>,
    backgrounds: Vec<Vec<RowBackground>>,
    cursor: Option<CursorPaint>,
    selection: Option<Selection>,
    cols: usize,
    rows: usize,
    cell_width: f32,
    cell_height: f32,
    bounds: Bounds<Pixels>,
    background: [u8; 3],
    cursor_color: [u8; 3],
    selection_color: [u8; 3],
    scroll: usize,
    scrollback_len: usize,
}

fn prepare(
    bounds: Bounds<Pixels>,
    id: Option<TerminalId>,
    workspace: &Workspace,
    highlights: &Highlights,
    _settings: &Settings,
    font_family: &str,
    font_size: f32,
    line_height: f32,
    colors: ThemeColors,
    selection: Option<Selection>,
    metrics: Rc<Cell<GridMetrics>>,
    window: &mut Window,
) -> TerminalPaint {
    let font = Font {
        weight: FontWeight::NORMAL,
        ..gpui::font(font_family.to_string())
    };
    let font_size_px = px(font_size.max(6.0));
    let run = TextRun {
        len: 4,
        font: font.clone(),
        color: rgb(color_u32(colors.foreground)).into(),
        background_color: None,
        underline: None,
        strikethrough: None,
    };
    let measure = window
        .text_system()
        .shape_line("MMMM".into(), font_size_px, &[run], None);
    let cell_width = (f32::from(measure.width) / 4.0).max(1.0);
    let cell_height = (font_size * line_height.max(1.05)).max(font_size + 2.0);
    let cols = (f32::from(bounds.size.width) / cell_width)
        .floor()
        .max(20.0) as u16;
    let rows = (f32::from(bounds.size.height) / cell_height)
        .floor()
        .max(5.0) as u16;

    metrics.set(GridMetrics {
        bounds,
        cell_width,
        cell_height,
        cols,
        rows,
    });

    let Some(id) = id else {
        return TerminalPaint {
            lines: Vec::new(),
            backgrounds: Vec::new(),
            cursor: None,
            selection: None,
            bounds,
            cols: cols as usize,
            rows: rows as usize,
            cell_width,
            cell_height,
            background: colors.background,
            cursor_color: colors.cursor,
            selection_color: colors.selection,
            scroll: 0,
            scrollback_len: 0,
        };
    };

    let mut snapshot = match workspace.snapshot(id) {
        Some(snapshot) => snapshot,
        None => {
            return TerminalPaint {
                lines: Vec::new(),
                backgrounds: Vec::new(),
                cursor: None,
                selection: None,
                bounds,
                cols: cols as usize,
                rows: rows as usize,
                cell_width,
                cell_height,
                background: colors.background,
                cursor_color: colors.cursor,
                selection_color: colors.selection,
                scroll: 0,
                scrollback_len: 0,
            }
        }
    };

    if snapshot.cols != cols as usize || snapshot.rows != rows as usize {
        workspace.resize(id, cols, rows);
        if let Some(updated) = workspace.snapshot(id) {
            snapshot = updated;
        }
    }

    let spans_per_row: Vec<Vec<HighlightSpan>> = snapshot
        .lines
        .iter()
        .map(|row| highlights.spans(&row.text))
        .collect();

    let mut lines = Vec::with_capacity(snapshot.lines.len());
    let mut backgrounds = Vec::with_capacity(snapshot.lines.len());

    for (row_index, row) in snapshot.lines.iter().enumerate() {
        let last_cell = row
            .cells
            .iter()
            .rposition(|cell| {
                cell.ch != ' '
                    || cell.combining.is_some()
                    || cell.attrs.bg != Color::Default
                    || cell.attrs.flags.contains(Flags::REVERSE)
            })
            .map(|index| index + 1)
            .unwrap_or(0);

        if last_cell == 0 {
            lines.push(None);
            backgrounds.push(Vec::new());
            continue;
        }

        let spans = &spans_per_row[row_index];
        let mut text = String::new();
        let mut runs: Vec<TextRun> = Vec::new();
        let mut row_backgrounds: Vec<RowBackground> = Vec::new();
        let mut current_style: Option<CellStyle> = None;
        let mut current_bg: Option<(usize, usize, [u8; 3])> = None;

        for (cell_index, cell) in row.cells.iter().take(last_cell).enumerate() {
            let byte_offset = text.len();
            let mut cell_text = String::new();
            cell.write_text(&mut cell_text);
            if cell_text.is_empty() {
                continue;
            }

            let span = spans
                .iter()
                .find(|span| span.start <= byte_offset && byte_offset < span.end);
            let style = cell_style(cell, span, &colors);

            let background = style.background;
            if background != colors.background {
                match &mut current_bg {
                    Some((start, end, color)) if *color == background && *end == cell_index => {
                        *end = cell_index + 1;
                    }
                    Some(_) => {
                        if let Some((start, end, color)) = current_bg.take() {
                            row_backgrounds.push(RowBackground {
                                start_cell: start,
                                end_cell: end,
                                color,
                            });
                        }
                        current_bg = Some((cell_index, cell_index + 1, background));
                    }
                    None => current_bg = Some((cell_index, cell_index + 1, background)),
                }
            } else if let Some((start, end, color)) = current_bg.take() {
                row_backgrounds.push(RowBackground {
                    start_cell: start,
                    end_cell: end,
                    color,
                });
            }

            if current_style.as_ref() != Some(&style) {
                current_style = Some(style.clone());
                let mut cell_font = font.clone();
                if style.bold {
                    cell_font.weight = FontWeight::BOLD;
                }
                runs.push(TextRun {
                    len: 0,
                    font: cell_font,
                    color: rgba_color(style.foreground, 1.0).into(),
                    background_color: None,
                    underline: style.underline.then_some(UnderlineStyle {
                        color: Some(rgba_color(style.foreground, 1.0).into()),
                        thickness: px(1.0),
                        wavy: false,
                    }),
                    strikethrough: None,
                });
            }

            text.push_str(&cell_text);
            if let Some(last) = runs.last_mut() {
                last.len += cell_text.len();
            }
        }

        if let Some((start, end, color)) = current_bg.take() {
            row_backgrounds.push(RowBackground {
                start_cell: start,
                end_cell: end,
                color,
            });
        }

        if text.is_empty() {
            lines.push(None);
        } else {
            let line =
                window
                    .text_system()
                    .shape_line(text.clone().into(), font_size_px, &runs, None);
            lines.push(Some(line));
        }
        backgrounds.push(row_backgrounds);
    }

    let cursor = snapshot.cursor.map(|(row, col)| CursorPaint {
        row,
        col,
        style: snapshot.cursor_style,
    });

    TerminalPaint {
        lines,
        backgrounds,
        cursor,
        selection,
        bounds,
        cols: cols as usize,
        rows: rows as usize,
        cell_width,
        cell_height,
        background: colors.background,
        cursor_color: colors.cursor,
        selection_color: colors.selection,
        scroll: snapshot.scroll_offset,
        scrollback_len: snapshot.scrollback_len,
    }
}

#[derive(Clone, PartialEq, Eq)]
struct CellStyle {
    foreground: [u8; 3],
    background: [u8; 3],
    bold: bool,
    underline: bool,
}

fn cell_style(
    cell: &xcord_vt::Cell,
    highlight: Option<&HighlightSpan>,
    colors: &ThemeColors,
) -> CellStyle {
    let bold = cell.attrs.flags.contains(Flags::BOLD);
    let mut foreground_color = cell.attrs.fg;
    if bold {
        if let Color::Indexed(index) = foreground_color {
            if index < 8 {
                foreground_color = Color::Indexed(index + 8);
            }
        }
    }

    let mut foreground = resolve_color(foreground_color, colors);
    let mut background = match cell.attrs.bg {
        Color::Default => colors.background,
        color => resolve_color(color, colors),
    };

    if cell.attrs.flags.contains(Flags::REVERSE) {
        std::mem::swap(&mut foreground, &mut background);
    }
    if cell.attrs.flags.contains(Flags::HIDDEN) {
        foreground = background;
    }
    if cell.attrs.flags.contains(Flags::DIM) {
        foreground = blend(foreground, background, 0.55);
    }

    if let Some(highlight) = highlight {
        foreground = highlight.foreground;
        if let Some(background_color) = highlight.background {
            background = background_color;
        }
    }

    CellStyle {
        foreground,
        background,
        bold,
        underline: cell.attrs.flags.contains(Flags::UNDERLINE)
            || cell.attrs.flags.contains(Flags::DOUBLE_UNDERLINE)
            || cell.attrs.flags.contains(Flags::CURLY_UNDERLINE),
    }
}

fn resolve_color(color: Color, colors: &ThemeColors) -> [u8; 3] {
    match color {
        Color::Default => colors.foreground,
        Color::Rgb(r, g, b) => [r, g, b],
        Color::Indexed(index) if index < 16 => colors.palette[index as usize],
        Color::Indexed(index) => {
            let (r, g, b) = default_palette(index);
            [r, g, b]
        }
    }
}

fn blend(from: [u8; 3], to: [u8; 3], amount: f32) -> [u8; 3] {
    let mix =
        |a: u8, b: u8| (a as f32 + (b as f32 - a as f32) * amount.clamp(0.0, 1.0)).round() as u8;
    [
        mix(from[0], to[0]),
        mix(from[1], to[1]),
        mix(from[2], to[2]),
    ]
}

fn paint_terminal(paint: TerminalPaint, window: &mut Window, cx: &mut App) {
    let origin = paint.bounds.origin;
    window.paint_quad(fill(paint.bounds, rgb(color_u32(paint.background))));
    // Backgrounds first so glyphs and selection stay readable.
    for (row, backgrounds) in paint.backgrounds.iter().enumerate() {
        let y = px(row as f32 * paint.cell_height);
        for background in backgrounds {
            let x = px(background.start_cell as f32 * paint.cell_width);
            let width = px((background.end_cell - background.start_cell) as f32 * paint.cell_width);
            window.paint_quad(fill(
                Bounds::new(origin + point(x, y), size(width, px(paint.cell_height))),
                rgb(color_u32(background.color)),
            ));
        }
    }

    if let Some(selection) = paint.selection {
        let ((start_row, start_col), (end_row, end_col)) = selection.normalized();
        for row in start_row..=end_row.min(paint.rows.saturating_sub(1)) {
            let from = if row == start_row { start_col } else { 0 };
            let to = if row == end_row {
                (end_col + 1).min(paint.cols)
            } else {
                paint.cols
            };
            if to <= from {
                continue;
            }
            let x = px(from as f32 * paint.cell_width);
            let y = px(row as f32 * paint.cell_height);
            let width = px((to - from) as f32 * paint.cell_width);
            window.paint_quad(fill(
                Bounds::new(origin + point(x, y), size(width, px(paint.cell_height))),
                rgba_color(paint.selection_color, 0.35),
            ));
        }
    }

    for (row, line) in paint.lines.iter().enumerate() {
        if let Some(line) = line {
            let line_origin = origin + point(px(0.0), px(row as f32 * paint.cell_height));
            line.paint(line_origin, px(paint.cell_height), window, cx)
                .ok();
        }
    }

    if let Some(cursor) = &paint.cursor {
        let x = px(cursor.col as f32 * paint.cell_width);
        let y = px(cursor.row as f32 * paint.cell_height);
        let (width, height) = match cursor.style {
            xcord_vt::CursorStyle::Bar => (px(2.0), px(paint.cell_height)),
            xcord_vt::CursorStyle::Underline => (px(paint.cell_width), px(2.0)),
            xcord_vt::CursorStyle::Block => (px(paint.cell_width), px(paint.cell_height)),
        };
        let yy = if matches!(cursor.style, xcord_vt::CursorStyle::Underline) {
            y + px(paint.cell_height - 2.0)
        } else {
            y
        };
        window.paint_quad(fill(
            Bounds::new(origin + point(x, yy), size(width, height)),
            rgba_color(paint.cursor_color, 0.38),
        ));
        window.paint_quad(fill(
            Bounds::new(
                origin + point(x, y + px(paint.cell_height - 2.0)),
                size(px(paint.cell_width), px(2.0)),
            ),
            rgba_color(paint.cursor_color, 0.9),
        ));
    }

    let total = paint.scrollback_len + paint.rows;
    if paint.scrollback_len > 0 && total > 0 {
        let track_height = paint.rows as f32 * paint.cell_height;
        let thumb_height = (paint.rows as f32 / total as f32 * track_height).max(18.0);
        let top_lines = total.saturating_sub(paint.rows + paint.scroll).min(total);
        let thumb_top = (top_lines as f32 / total as f32) * track_height;
        let x = px(paint.cols as f32 * paint.cell_width - 5.0);
        window.paint_quad(fill(
            Bounds::new(
                origin + point(x, px(thumb_top)),
                size(px(4.0), px(thumb_height)),
            ),
            rgba_color(paint.cursor_color, 0.35),
        ));
    }
}

fn color_u32(color: [u8; 3]) -> u32 {
    ((color[0] as u32) << 16) | ((color[1] as u32) << 8) | color[2] as u32
}

fn rgba_color(color: [u8; 3], alpha: f32) -> gpui::Rgba {
    let alpha = (alpha.clamp(0.0, 1.0) * 255.0) as u32;
    rgba((color_u32(color) << 8) | alpha)
}

fn encode_key(keystroke: &Keystroke, application_cursor: bool) -> Option<Vec<u8>> {
    let modifiers = keystroke.modifiers;
    let control = modifiers.control;
    let alt = modifiers.alt;
    let shift = modifiers.shift;
    let modifier = 1 + shift as u8 + (alt as u8) * 2 + (control as u8) * 4;
    let key = keystroke.key.as_str();

    let tilde = |number: u8| {
        if modifier > 1 {
            format!("\x1b[{number};{modifier}~").into_bytes()
        } else {
            format!("\x1b[{number}~").into_bytes()
        }
    };
    let ss3 = |code: char| {
        if modifier > 1 {
            format!("\x1b[1;{modifier}{code}").into_bytes()
        } else if application_cursor {
            format!("\x1bO{code}").into_bytes()
        } else {
            format!("\x1b[{code}").into_bytes()
        }
    };

    match key {
        "enter" => Some(if alt {
            b"\x1b\r".to_vec()
        } else {
            b"\r".to_vec()
        }),
        "tab" => Some(if shift {
            b"\x1b[Z".to_vec()
        } else {
            b"\t".to_vec()
        }),
        "backspace" => Some(if control { vec![0x08] } else { vec![0x7f] }),
        "escape" => Some(b"\x1b".to_vec()),
        "up" => Some(ss3('A')),
        "down" => Some(ss3('B')),
        "right" => Some(ss3('C')),
        "left" => Some(ss3('D')),
        "home" => Some(ss3('H')),
        "end" => Some(ss3('F')),
        "insert" => Some(tilde(2)),
        "delete" => Some(tilde(3)),
        "pageup" => Some(tilde(5)),
        "pagedown" => Some(tilde(6)),
        "f1" => Some(function_key(1, modifier)),
        "f2" => Some(function_key(2, modifier)),
        "f3" => Some(function_key(3, modifier)),
        "f4" => Some(function_key(4, modifier)),
        "f5" => Some(tilde(15)),
        "f6" => Some(tilde(17)),
        "f7" => Some(tilde(18)),
        "f8" => Some(tilde(19)),
        "f9" => Some(tilde(20)),
        "f10" => Some(tilde(21)),
        "f11" => Some(tilde(23)),
        "f12" => Some(tilde(24)),
        "space" if control => Some(vec![0x00]),
        "space" => {
            if alt {
                Some(b"\x1b ".to_vec())
            } else {
                Some(b" ".to_vec())
            }
        }
        _ => {
            if control && !alt {
                let byte = key.as_bytes().first().copied()?;
                let control_byte = match byte {
                    b'a'..=b'z' => byte - b'a' + 1,
                    b'@' => 0,
                    b'[' => 27,
                    b'\\' => 28,
                    b']' => 29,
                    b'^' => 30,
                    b'_' => 31,
                    _ => return None,
                };
                return Some(vec![control_byte]);
            }
            let text = keystroke.key_char.clone().or_else(|| {
                (!key.is_empty() && key.chars().count() == 1).then(|| key.to_string())
            })?;
            if text.is_empty() {
                return None;
            }
            if alt {
                let mut bytes = vec![0x1b];
                bytes.extend_from_slice(text.as_bytes());
                Some(bytes)
            } else {
                Some(text.into_bytes())
            }
        }
    }
}

fn function_key(number: u8, modifier: u8) -> Vec<u8> {
    let code = match number {
        1 => 'P',
        2 => 'Q',
        3 => 'R',
        _ => 'S',
    };
    if modifier > 1 {
        format!("\x1b[1;{modifier}{code}").into_bytes()
    } else {
        format!("\x1bO{code}").into_bytes()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use gpui::Modifiers;

    fn key(name: &str) -> Keystroke {
        Keystroke {
            modifiers: Modifiers::default(),
            key: name.to_string(),
            key_char: None,
        }
    }

    #[test]
    fn encodes_plain_space() {
        assert_eq!(encode_key(&key("space"), false), Some(b" ".to_vec()));
    }

    #[test]
    fn encodes_alt_space() {
        let mut stroke = key("space");
        stroke.modifiers.alt = true;
        assert_eq!(encode_key(&stroke, false), Some(b"\x1b ".to_vec()));
    }
}
