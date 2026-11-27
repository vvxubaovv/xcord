//! Small GPUI Markdown renderer for AI replies.
//!
//! It deliberately keeps the output in normal layout flow (no WebView or
//! absolute overlays) so wrapped text, selection-free scrolling and the
//! existing message rail continue to work.

use std::cell::Cell;
use std::rc::Rc;

use gpui::prelude::*;
use gpui::{div, font, px, App, Div, Entity, Font, FontStyle, FontWeight, TextRun, UnderlineStyle};
use pulldown_cmark::{CodeBlockKind, Event, HeadingLevel, Options, Parser, Tag, TagEnd};
use xcord_plugin_api::Theme;

use crate::root::{rgba, rgbc};
use crate::selectable_text::SelectableText;

#[derive(Default, Clone, Copy)]
struct Style {
    bold: usize,
    italic: usize,
    strike: usize,
    code: bool,
    link: bool,
}

struct ListState {
    ordered: bool,
    next: u64,
}

struct Table {
    rows: Vec<Vec<String>>,
    row: Vec<String>,
    cell: String,
    header: bool,
}

pub enum MarkdownBlock {
    Text {
        text: String,
        runs: Vec<TextRun>,
        size: f32,
        quote: bool,
    },
    Code {
        text: String,
        lang: String,
    },
    Table {
        rows: Vec<Vec<String>>,
        header: bool,
    },
    Mermaid {
        source: String,
    },
    Rule,
}

pub fn parse(source: &str, theme: &Theme, base_size: f32) -> Vec<MarkdownBlock> {
    let mut blocks: Vec<MarkdownBlock> = Vec::new();
    let mut inline = String::new();
    let mut runs: Vec<TextRun> = Vec::new();
    let mut style = Style::default();
    let mut heading: Option<usize> = None;
    let mut list_stack: Vec<ListState> = Vec::new();
    let mut pending_prefix: Option<String> = None;
    let mut quote_depth = 0usize;
    let mut code: Option<(String, String)> = None;
    let mut table: Option<Table> = None;
    let mut paragraph_open = false;
    let mut finished_table: Option<(Vec<Vec<String>>, bool)> = None;

    let base_font = font("Segoe UI");
    let code_font = font("Consolas");

    let push_text = |inline: &mut String,
                     runs: &mut Vec<TextRun>,
                     text: &str,
                     style: Style,
                     heading: Option<usize>,
                     base_font: &Font,
                     code_font: &Font,
                     theme: &Theme,
                     base_size: f32| {
        if text.is_empty() {
            return;
        }
        inline.push_str(text);
        let mut font = if style.code {
            code_font.clone()
        } else {
            base_font.clone()
        };
        if heading.is_some() || style.bold > 0 {
            font.weight = FontWeight::BOLD;
        }
        if style.italic > 0 {
            font.style = FontStyle::Italic;
        }
        let color = if style.code {
            rgbc(theme.accent).into()
        } else if style.link {
            rgbc(theme.accent).into()
        } else {
            rgbc(theme.foreground).into()
        };
        let background = if style.code {
            Some(rgba(theme.accent, 0.14))
        } else {
            None
        };
        let underline = if style.link {
            Some(UnderlineStyle {
                thickness: px(1.),
                color: Some(rgbc(theme.accent).into()),
                wavy: false,
            })
        } else {
            None
        };
        runs.push(TextRun {
            len: text.len(),
            font,
            color,
            background_color: background.map(Into::into),
            underline,
            strikethrough: (style.strike > 0).then(|| gpui::StrikethroughStyle {
                thickness: px(1.),
                color: Some(rgbc(theme.muted).into()),
            }),
        });
        let _ = base_size;
    };

    let flush = |blocks: &mut Vec<MarkdownBlock>,
                 inline: &mut String,
                 runs: &mut Vec<TextRun>,
                 heading: Option<usize>,
                 quote_depth: usize,
                 theme: &Theme,
                 base_size: f32| {
        let text = std::mem::take(inline);
        let mut rich = std::mem::take(runs);
        if text.trim().is_empty() {
            return;
        }
        if !rich.iter().any(|run| run.len > 0) {
            rich.push(TextRun {
                len: text.len(),
                font: font("Segoe UI"),
                color: rgbc(theme.foreground).into(),
                background_color: None,
                underline: None,
                strikethrough: None,
            });
        }
        let size = match heading {
            Some(1) => base_size + 7.0,
            Some(2) => base_size + 5.0,
            Some(3) => base_size + 3.0,
            Some(4) => base_size + 1.5,
            _ => base_size,
        };
        blocks.push(MarkdownBlock::Text {
            text,
            runs: rich,
            size,
            quote: quote_depth > 0,
        });
    };

    let _render_code = |lang: &str, code: &str, theme: &Theme, base_size: f32| -> Div {
        let mut card = div()
            .w_full()
            .min_w(px(0.))
            .rounded_md()
            .bg(rgbc(theme.panel_alt))
            .border_1()
            .border_color(rgbc(theme.border))
            .overflow_hidden()
            .flex()
            .flex_col();
        if !lang.is_empty() {
            card = card.child(
                div()
                    .px_2()
                    .pt_1()
                    .text_size(px(9.))
                    .text_color(rgbc(theme.muted))
                    .child(lang.to_uppercase()),
            );
        }
        card.child(
            div()
                .w_full()
                .min_w(px(0.))
                .px_2()
                .py_1()
                .font_family("Consolas")
                .text_size(px(base_size - 0.5))
                .line_height(px((base_size - 0.5) * 1.5))
                .text_color(rgbc(theme.foreground))
                .child(code.trim_end().to_string()),
        )
    };

    let mut parser = Parser::new_ext(
        source,
        Options::ENABLE_TABLES | Options::ENABLE_STRIKETHROUGH | Options::ENABLE_TASKLISTS,
    )
    .peekable();

    while let Some(event) = parser.next() {
        if let Some((lang, body)) = code.as_mut() {
            match event {
                Event::Text(text) => body.push_str(&text),
                Event::End(TagEnd::CodeBlock) => {
                    let lang = lang.clone();
                    let body = body.clone();
                    code = None;
                    if lang.eq_ignore_ascii_case("mermaid") {
                        blocks.push(MarkdownBlock::Mermaid { source: body });
                    } else {
                        blocks.push(MarkdownBlock::Code { text: body, lang });
                    }
                }
                _ => {}
            }
            continue;
        }

        if table.is_some() {
            let table = table.as_mut().expect("table");
            match event {
                Event::Start(Tag::TableHead) => {
                    table.header = true;
                    table.row.clear();
                }
                Event::Start(Tag::TableRow) => {
                    table.row.clear();
                }
                Event::Start(Tag::TableCell) => {
                    table.cell.clear();
                }
                Event::Text(ref text) | Event::Code(ref text) => table.cell.push_str(text),
                Event::End(TagEnd::TableCell) => {
                    table.row.push(table.cell.trim().to_string());
                }
                Event::End(TagEnd::TableHead) | Event::End(TagEnd::TableRow) => {
                    if !table.row.is_empty() {
                        table.rows.push(std::mem::take(&mut table.row));
                    }
                }
                Event::End(TagEnd::Table) => {
                    let rows = std::mem::take(&mut table.rows);
                    let header = table.header;
                    finished_table = Some((rows, header));
                }
                _ => {}
            }
            if finished_table.is_none() {
                continue;
            }
        }
        if let Some((rows, header)) = finished_table.take() {
            table = None;
            blocks.push(MarkdownBlock::Table { rows, header });
            continue;
        }

        match event {
            Event::Start(Tag::Heading { level, .. }) => {
                heading = Some(match level {
                    HeadingLevel::H1 => 1,
                    HeadingLevel::H2 => 2,
                    HeadingLevel::H3 => 3,
                    HeadingLevel::H4 => 4,
                    _ => 5,
                });
                inline.clear();
                runs.clear();
            }
            Event::End(TagEnd::Heading(_)) => {
                flush(
                    &mut blocks,
                    &mut inline,
                    &mut runs,
                    heading,
                    quote_depth,
                    theme,
                    base_size,
                );
                heading = None;
                pending_prefix = None;
            }
            Event::Start(Tag::Paragraph) => {
                paragraph_open = true;
                if inline.is_empty() {
                    if let Some(prefix) = pending_prefix.take() {
                        push_text(
                            &mut inline,
                            &mut runs,
                            &prefix,
                            style,
                            heading,
                            &base_font,
                            &code_font,
                            theme,
                            base_size,
                        );
                    }
                }
            }
            Event::End(TagEnd::Paragraph) => {
                paragraph_open = false;
                if heading.is_none() && list_stack.is_empty() {
                    flush(
                        &mut blocks,
                        &mut inline,
                        &mut runs,
                        heading,
                        quote_depth,
                        theme,
                        base_size,
                    );
                }
                pending_prefix = None;
            }
            Event::Start(Tag::BlockQuote(_)) => {
                quote_depth += 1;
            }
            Event::End(TagEnd::BlockQuote(_)) => {
                quote_depth = quote_depth.saturating_sub(1);
            }
            Event::Start(Tag::List(start)) => {
                list_stack.push(ListState {
                    ordered: start.is_some(),
                    next: start.unwrap_or(1),
                });
            }
            Event::End(TagEnd::List(_)) => {
                list_stack.pop();
                if list_stack.is_empty() && !inline.trim().is_empty() {
                    flush(
                        &mut blocks,
                        &mut inline,
                        &mut runs,
                        heading,
                        quote_depth,
                        theme,
                        base_size,
                    );
                }
            }
            Event::Start(Tag::Item) => {
                let depth = list_stack.len().saturating_sub(1);
                let marker = if let Some(list) = list_stack.last_mut() {
                    if list.ordered {
                        let value = list.next;
                        list.next += 1;
                        format!("{value}. ")
                    } else {
                        "• ".to_string()
                    }
                } else {
                    "• ".to_string()
                };
                pending_prefix = Some(format!("{}{}", "    ".repeat(depth), marker));
            }
            Event::End(TagEnd::Item) => {
                if !inline.is_empty() {
                    if !inline.ends_with('\n') {
                        inline.push('\n');
                    }
                    runs.push(TextRun {
                        len: 1,
                        font: base_font.clone(),
                        color: rgbc(theme.foreground).into(),
                        background_color: None,
                        underline: None,
                        strikethrough: None,
                    });
                }
                pending_prefix = None;
            }
            Event::Start(Tag::CodeBlock(kind)) => {
                let lang = match kind {
                    CodeBlockKind::Fenced(lang) => lang.to_string(),
                    CodeBlockKind::Indented => String::new(),
                };
                code = Some((lang, String::new()));
            }
            Event::Start(Tag::Table(_)) => {
                table = Some(Table {
                    rows: Vec::new(),
                    row: Vec::new(),
                    cell: String::new(),
                    header: false,
                });
            }
            Event::Start(Tag::Strong) => style.bold += 1,
            Event::End(TagEnd::Strong) => style.bold = style.bold.saturating_sub(1),
            Event::Start(Tag::Emphasis) => style.italic += 1,
            Event::End(TagEnd::Emphasis) => style.italic = style.italic.saturating_sub(1),
            Event::Start(Tag::Strikethrough) => style.strike += 1,
            Event::End(TagEnd::Strikethrough) => style.strike = style.strike.saturating_sub(1),
            Event::Start(Tag::Link { .. }) => style.link = true,
            Event::End(TagEnd::Link) => style.link = false,
            Event::Start(Tag::Image { .. }) => style.link = true,
            Event::End(TagEnd::Image) => style.link = false,
            Event::Text(text) => {
                if inline.is_empty() {
                    if let Some(prefix) = pending_prefix.take() {
                        push_text(
                            &mut inline,
                            &mut runs,
                            &prefix,
                            style,
                            heading,
                            &base_font,
                            &code_font,
                            theme,
                            base_size,
                        );
                    }
                }
                push_text(
                    &mut inline,
                    &mut runs,
                    &text,
                    style,
                    heading,
                    &base_font,
                    &code_font,
                    theme,
                    base_size,
                )
            }
            Event::Code(text) => {
                if inline.is_empty() {
                    if let Some(prefix) = pending_prefix.take() {
                        push_text(
                            &mut inline,
                            &mut runs,
                            &prefix,
                            style,
                            heading,
                            &base_font,
                            &code_font,
                            theme,
                            base_size,
                        );
                    }
                }
                let mut code_style = style;
                code_style.code = true;
                push_text(
                    &mut inline,
                    &mut runs,
                    &text,
                    code_style,
                    heading,
                    &base_font,
                    &code_font,
                    theme,
                    base_size,
                );
            }
            Event::SoftBreak => push_text(
                &mut inline,
                &mut runs,
                " ",
                style,
                heading,
                &base_font,
                &code_font,
                theme,
                base_size,
            ),
            Event::HardBreak => push_text(
                &mut inline,
                &mut runs,
                "\n",
                style,
                heading,
                &base_font,
                &code_font,
                theme,
                base_size,
            ),
            Event::Rule => {
                flush(
                    &mut blocks,
                    &mut inline,
                    &mut runs,
                    heading,
                    quote_depth,
                    theme,
                    base_size,
                );
                blocks.push(MarkdownBlock::Rule);
            }
            Event::TaskListMarker(checked) => push_text(
                &mut inline,
                &mut runs,
                if checked { "☑ " } else { "☐ " },
                style,
                heading,
                &base_font,
                &code_font,
                theme,
                base_size,
            ),
            Event::Html(text) | Event::InlineHtml(text) => push_text(
                &mut inline,
                &mut runs,
                &text,
                style,
                heading,
                &base_font,
                &code_font,
                theme,
                base_size,
            ),
            _ => {}
        }
        let _ = paragraph_open;
    }

    if let Some((lang, body)) = code {
        if lang.eq_ignore_ascii_case("mermaid") {
            blocks.push(MarkdownBlock::Mermaid { source: body });
        } else {
            blocks.push(MarkdownBlock::Code { text: body, lang });
        }
    }
    flush(
        &mut blocks,
        &mut inline,
        &mut runs,
        heading,
        quote_depth,
        theme,
        base_size,
    );

    // Keep consecutive prose blocks in one selectable text run so a drag can
    // cross list items and paragraph boundaries.
    let mut merged: Vec<MarkdownBlock> = Vec::new();
    for block in blocks {
        if let MarkdownBlock::Text {
            text,
            runs,
            size,
            quote,
        } = block
        {
            if let Some(MarkdownBlock::Text {
                text: previous_text,
                runs: previous_runs,
                size: previous_size,
                quote: previous_quote,
            }) = merged.last_mut()
            {
                if *previous_size == size && *previous_quote == quote {
                    let mut added = 0;
                    if !previous_text.ends_with('\n') {
                        previous_text.push('\n');
                        added += 1;
                    }
                    previous_text.push('\n');
                    added += 1;
                    previous_text.push_str(&text);
                    previous_runs.push(TextRun {
                        len: added,
                        font: font("Segoe UI"),
                        color: rgbc(theme.foreground).into(),
                        background_color: None,
                        underline: None,
                        strikethrough: None,
                    });
                    let mut runs = runs;
                    previous_runs.append(&mut runs);
                    continue;
                }
            }
            merged.push(MarkdownBlock::Text {
                text,
                runs,
                size,
                quote,
            });
        } else {
            merged.push(block);
        }
    }
    merged
}

#[allow(dead_code)]
fn render_table(rows: Vec<Vec<String>>, header: bool, theme: &Theme, base_size: f32) -> Div {
    let columns = rows.iter().map(Vec::len).max().unwrap_or(0).max(1);
    let mut table = div()
        .w_full()
        .min_w(px(0.))
        .rounded_md()
        .border_1()
        .border_color(rgbc(theme.border))
        .overflow_hidden()
        .flex()
        .flex_col();
    for (row_index, row) in rows.iter().enumerate() {
        let mut line = div()
            .w_full()
            .min_w(px(0.))
            .flex()
            .border_b_1()
            .border_color(rgbc(theme.border));
        for column in 0..columns {
            let value = row.get(column).cloned().unwrap_or_default();
            line = line.child(
                div()
                    .flex_1()
                    .min_w(px(80.))
                    .px_2()
                    .py_1()
                    .text_size(px(base_size - 0.5))
                    .text_color(if header && row_index == 0 {
                        rgbc(theme.foreground)
                    } else {
                        rgbc(theme.muted)
                    })
                    .when(header && row_index == 0, |cell| {
                        cell.bg(rgba(theme.panel_alt, 0.7))
                    })
                    .overflow_hidden()
                    .child(value),
            );
        }
        table = table.child(line);
    }
    table
}

fn render_mermaid(source: &str, theme: &Theme, base_size: f32) -> Div {
    div()
        .w_full()
        .min_w(px(0.))
        .rounded_md()
        .border_1()
        .border_color(rgbc(theme.border))
        .bg(rgbc(theme.panel_alt))
        .p_2()
        .flex()
        .flex_col()
        .gap_2()
        .child(
            div()
                .text_size(px(9.))
                .text_color(rgbc(theme.muted))
                .child("MERMAID (未识别为流程图，显示源码)"),
        )
        .child(
            div()
                .w_full()
                .overflow_hidden()
                .font_family("Consolas")
                .text_size(px(base_size - 1.))
                .text_color(rgbc(theme.muted))
                .child(source.trim().to_string()),
        )
}

pub struct MarkdownView {
    source: String,
    theme: Theme,
    base_size: f32,
    id_base: usize,
    owner: Rc<Cell<Option<usize>>>,
    blocks: Vec<MarkdownBlock>,
    entities: Vec<Option<Entity<SelectableText>>>,
    kinds: Vec<u8>,
    table_entities: Vec<Vec<Vec<Entity<SelectableText>>>>,
}

impl MarkdownView {
    pub fn new(id_base: usize, owner: Rc<Cell<Option<usize>>>, theme: Theme) -> Self {
        Self {
            source: String::new(),
            theme,
            base_size: 11.5,
            id_base,
            owner,
            blocks: Vec::new(),
            entities: Vec::new(),
            kinds: Vec::new(),
            table_entities: Vec::new(),
        }
    }

    pub fn set_content(&mut self, source: String, theme: Theme, base_size: f32, cx: &mut App) {
        let changed = self.source != source || self.base_size != base_size;
        self.source = source.clone();
        self.theme = theme;
        self.base_size = base_size;
        if !changed {
            return;
        }
        self.blocks = parse(&source, &self.theme, base_size);
        self.table_entities.clear();
        let mut table_entity_id = 0usize;
        for block in &self.blocks {
            if let MarkdownBlock::Table { rows, .. } = block {
                let mut row_entities = Vec::new();
                for row in rows {
                    let mut cell_entities = Vec::new();
                    for cell in row {
                        let owner = self.owner.clone();
                        let text = cell.clone();
                        let theme = self.theme.clone();
                        let id = self.id_base + 500_000 + table_entity_id;
                        table_entity_id += 1;
                        let size = (base_size - 0.5).max(9.0);
                        let entity = cx.new(|cx| {
                            SelectableText::new(
                                id,
                                owner,
                                text,
                                rgbc(theme.foreground),
                                size,
                                rgba(theme.accent, 0.28),
                                cx,
                            )
                        });
                        cell_entities.push(entity);
                    }
                    row_entities.push(cell_entities);
                }
                self.table_entities.push(row_entities);
            } else {
                self.table_entities.push(Vec::new());
            }
        }
        while self.entities.len() < self.blocks.len() {
            self.entities.push(None);
            self.kinds.push(0);
        }
        self.entities.truncate(self.blocks.len());
        self.kinds.truncate(self.blocks.len());

        for (index, block) in self.blocks.iter().enumerate() {
            match block {
                MarkdownBlock::Text {
                    text, runs, size, ..
                } => {
                    if self.entities[index].is_none() || self.kinds[index] != 1 {
                        let owner = self.owner.clone();
                        let text = text.clone();
                        let runs = runs.clone();
                        let size = *size;
                        let theme = self.theme.clone();
                        let entity = cx.new(|cx| {
                            let mut text_view = SelectableText::new(
                                self.id_base + index,
                                owner,
                                text,
                                rgbc(theme.foreground),
                                size,
                                rgba(theme.accent, 0.28),
                                cx,
                            );
                            text_view.set_runs(runs, cx);
                            text_view
                        });
                        self.entities[index] = Some(entity);
                        self.kinds[index] = 1;
                    } else if let Some(entity) = &self.entities[index] {
                        let text = text.clone();
                        let runs = runs.clone();
                        let size = *size;
                        let theme = self.theme.clone();
                        entity.update(cx, |text_view, cx| {
                            text_view.set_text(text, cx);
                            text_view.set_runs(runs, cx);
                            text_view.set_style(rgbc(theme.foreground), size, cx);
                            text_view.set_monospace(false, cx);
                        });
                    }
                }
                MarkdownBlock::Code { text, .. } => {
                    if self.entities[index].is_none() || self.kinds[index] != 2 {
                        let owner = self.owner.clone();
                        let text = text.clone();
                        let size = (self.base_size - 0.5).max(9.0);
                        let theme = self.theme.clone();
                        let entity = cx.new(|cx| {
                            let mut text_view = SelectableText::new(
                                self.id_base + index,
                                owner,
                                text,
                                rgbc(theme.foreground),
                                size,
                                rgba(theme.accent, 0.28),
                                cx,
                            );
                            text_view.set_monospace(true, cx);
                            text_view
                        });
                        self.entities[index] = Some(entity);
                        self.kinds[index] = 2;
                    } else if let Some(entity) = &self.entities[index] {
                        let text = text.clone();
                        let size = (self.base_size - 0.5).max(9.0);
                        let theme = self.theme.clone();
                        entity.update(cx, |text_view, cx| {
                            text_view.set_text(text, cx);
                            text_view.set_style(rgbc(theme.foreground), size, cx);
                            text_view.set_monospace(true, cx);
                        });
                    }
                }
                _ => {
                    self.entities[index] = None;
                    self.kinds[index] = 0;
                }
            }
        }
    }
}

impl MarkdownView {
    pub fn render(&self) -> Div {
        let theme = self.theme.clone();
        let mut column = div().w_full().min_w(px(0.)).flex().flex_col().gap_2();
        for (index, block) in self.blocks.iter().enumerate() {
            match block {
                MarkdownBlock::Text { quote, .. } => {
                    if let Some(entity) = &self.entities[index] {
                        let mut wrapper = div().w_full().min_w(px(0.));
                        if *quote {
                            wrapper = wrapper
                                .pl(px(10.))
                                .border_l_2()
                                .border_color(rgbc(theme.accent))
                                .bg(rgba(theme.panel_alt, 0.5));
                        }
                        column = column.child(wrapper.child(entity.clone()));
                    }
                }
                MarkdownBlock::Code { text, lang } => {
                    let mut card = div()
                        .w_full()
                        .min_w(px(0.))
                        .rounded_md()
                        .bg(rgbc(theme.panel_alt))
                        .border_1()
                        .border_color(rgbc(theme.border))
                        .overflow_hidden()
                        .flex()
                        .flex_col();
                    if !lang.is_empty() {
                        card = card.child(
                            div()
                                .px_2()
                                .pt_1()
                                .text_size(px(9.))
                                .text_color(rgbc(theme.muted))
                                .child(lang.to_uppercase()),
                        );
                    }
                    let body = self.entities[index]
                        .clone()
                        .map(|entity| div().px_2().py_1().child(entity))
                        .unwrap_or_else(|| {
                            div()
                                .px_2()
                                .py_1()
                                .font_family("Consolas")
                                .text_size(px((self.base_size - 0.5).max(9.0)))
                                .child(text.clone())
                        });
                    column = column.child(card.child(body));
                }
                MarkdownBlock::Table { rows, header } => {
                    let columns = rows.iter().map(Vec::len).max().unwrap_or(0).max(1);
                    let mut table = div()
                        .w_full()
                        .min_w(px(0.))
                        .rounded_md()
                        .border_1()
                        .border_color(rgbc(theme.border))
                        .overflow_hidden()
                        .flex()
                        .flex_col();
                    for (row_index, row) in rows.iter().enumerate() {
                        let mut line = div()
                            .w_full()
                            .min_w(px(0.))
                            .flex()
                            .border_b_1()
                            .border_color(rgbc(theme.border));
                        for column in 0..columns {
                            let mut cell = div()
                                .flex_1()
                                .min_w(px(80.))
                                .px_2()
                                .py_1()
                                .when(*header && row_index == 0, |cell| {
                                    cell.bg(rgba(theme.panel_alt, 0.7))
                                })
                                .overflow_hidden();
                            if let Some(entity) = self
                                .table_entities
                                .get(index)
                                .and_then(|rows| rows.get(row_index))
                                .and_then(|cells| cells.get(column))
                            {
                                cell = cell.child(entity.clone());
                            } else {
                                cell = cell.child(
                                    div()
                                        .text_size(px(self.base_size - 0.5))
                                        .text_color(rgbc(theme.foreground))
                                        .child(row.get(column).cloned().unwrap_or_default()),
                                );
                            }
                            line = line.child(cell);
                        }
                        table = table.child(line);
                    }
                    column = column.child(table);
                }
                MarkdownBlock::Mermaid { source } => {
                    column = column.child(
                        crate::mermaid::render(source, &theme, self.base_size)
                            .unwrap_or_else(|| render_mermaid(source, &theme, self.base_size)),
                    );
                }
                MarkdownBlock::Rule => {
                    column = column.child(div().w_full().h(px(1.)).my_1().bg(rgbc(theme.border)));
                }
            }
        }
        column
    }
}
