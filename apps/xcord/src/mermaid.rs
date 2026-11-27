//! Minimal native Mermaid flowchart renderer.
//!
//! Mermaid is normally rendered in a browser. For terminal/AI output we only
//! need the common `graph TD/LR` flowchart subset, so render those as normal
//! GPUI boxes and orthogonal connectors. Other Mermaid diagram types fall back
//! to the source block in the Markdown renderer.

use std::collections::HashMap;

use gpui::prelude::*;
use gpui::{canvas, div, fill, point, px, size, Bounds, Div, Hsla};
use xcord_plugin_api::Theme;

use crate::root::{rgba, rgbc};

#[derive(Clone)]
struct Node {
    id: String,
    label: String,
    x: f32,
    y: f32,
}

#[derive(Clone, Copy)]
struct Segment {
    x1: f32,
    y1: f32,
    x2: f32,
    y2: f32,
}

const BOX_W: f32 = 138.0;
const BOX_H: f32 = 42.0;
const GAP_X: f32 = 26.0;
const GAP_Y: f32 = 34.0;

pub fn render(source: &str, theme: &Theme, base_size: f32) -> Option<Div> {
    let mut direction = "TD".to_string();
    let mut labels: HashMap<String, String> = HashMap::new();
    let mut order: Vec<String> = Vec::new();
    let mut edges: Vec<(String, String, Option<String>)> = Vec::new();

    for raw in source.lines() {
        let line = raw.split("%%").next().unwrap_or("").trim();
        if line.is_empty() {
            continue;
        }
        let lower = line.to_ascii_lowercase();
        if lower.starts_with("graph ") || lower.starts_with("flowchart ") {
            if lower.contains(" lr") || lower.ends_with(" lr") {
                direction = "LR".to_string();
            } else if lower.contains(" td") || lower.contains(" tb") {
                direction = "TD".to_string();
            }
            continue;
        }
        if lower.starts_with("subgraph")
            || lower == "end"
            || lower.starts_with("classdef")
            || lower.starts_with("class ")
            || lower.starts_with("style ")
            || lower.starts_with("linkstyle")
            || lower.starts_with("direction ")
        {
            continue;
        }

        if let Some((left, label, right)) = split_edge(line) {
            let Some((from_id, from_label)) = parse_node(&left) else {
                continue;
            };
            let Some((to_id, to_label)) = parse_node(&right) else {
                continue;
            };
            add_node(&mut labels, &mut order, &from_id, &from_label);
            add_node(&mut labels, &mut order, &to_id, &to_label);
            edges.push((from_id, to_id, label));
        } else if let Some((id, label)) = parse_node(line) {
            add_node(&mut labels, &mut order, &id, &label);
        }
    }

    if order.is_empty() {
        return None;
    }

    let index: HashMap<String, usize> = order
        .iter()
        .enumerate()
        .map(|(index, id)| (id.clone(), index))
        .collect();
    let mut layers = vec![0usize; order.len()];
    for _ in 0..order.len().saturating_mul(2).max(1) {
        for (from, to, _) in &edges {
            let (Some(&a), Some(&b)) = (index.get(from), index.get(to)) else {
                continue;
            };
            if layers[b] < layers[a] + 1 {
                layers[b] = layers[a] + 1;
            }
        }
    }
    let max_layer = layers.iter().copied().max().unwrap_or(0);
    let mut per_layer = vec![0usize; max_layer + 1];
    for layer in &layers {
        per_layer[*layer] += 1;
    }
    let max_count = per_layer.iter().copied().max().unwrap_or(1);
    let total_w = if direction == "LR" {
        (max_layer + 1) as f32 * (BOX_W + GAP_X) - GAP_X
    } else {
        max_count as f32 * (BOX_W + GAP_X) - GAP_X
    };
    let total_h = if direction == "LR" {
        max_count as f32 * (BOX_H + GAP_Y) - GAP_Y
    } else {
        (max_layer + 1) as f32 * (BOX_H + GAP_Y) - GAP_Y
    };
    let mut next_in_layer = vec![0usize; max_layer + 1];
    let mut positions: HashMap<String, (f32, f32)> = HashMap::new();
    let mut nodes = Vec::new();
    for (i, id) in order.iter().enumerate() {
        let layer = layers[i];
        let slot = next_in_layer[layer];
        next_in_layer[layer] += 1;
        let (x, y) = if direction == "LR" {
            let layer_count = per_layer[layer];
            let y = (slot as f32) * (BOX_H + GAP_Y)
                + (max_count - layer_count) as f32 * (BOX_H + GAP_Y) / 2.0;
            (layer as f32 * (BOX_W + GAP_X), y)
        } else {
            let layer_count = per_layer[layer];
            let x = (slot as f32) * (BOX_W + GAP_X)
                + (max_count - layer_count) as f32 * (BOX_W + GAP_X) / 2.0;
            (x, layer as f32 * (BOX_H + GAP_Y))
        };
        positions.insert(id.clone(), (x, y));
        nodes.push(Node {
            id: id.clone(),
            label: labels.get(id).cloned().unwrap_or_else(|| id.clone()),
            x,
            y,
        });
    }

    let mut segments = Vec::new();
    let mut edge_labels: Vec<(f32, f32, String)> = Vec::new();
    for (from, to, edge_label) in &edges {
        let (Some(&(sx, sy)), Some(&(tx, ty))) = (positions.get(from), positions.get(to)) else {
            continue;
        };
        if direction == "LR" {
            let x1 = sx + BOX_W;
            let y1 = sy + BOX_H / 2.0;
            let x2 = tx;
            let y2 = ty + BOX_H / 2.0;
            let mid = (x1 + x2) / 2.0;
            segments.push(Segment {
                x1,
                y1,
                x2: mid,
                y2: y1,
            });
            segments.push(Segment {
                x1: mid,
                y1,
                x2: mid,
                y2,
            });
            segments.push(Segment {
                x1: mid,
                y1: y2,
                x2,
                y2,
            });
        } else {
            let x1 = sx + BOX_W / 2.0;
            let y1 = sy + BOX_H;
            let x2 = tx + BOX_W / 2.0;
            let y2 = ty;
            let mid = (y1 + y2) / 2.0;
            segments.push(Segment {
                x1,
                y1,
                x2: x1,
                y2: mid,
            });
            segments.push(Segment {
                x1,
                y1: mid,
                x2,
                y2: mid,
            });
            segments.push(Segment {
                x1: x2,
                y1: mid,
                x2,
                y2,
            });
        }
        if let Some(label) = edge_label {
            if !label.is_empty() {
                let (mx, my) = if direction == "LR" {
                    ((sx + BOX_W + tx) / 2.0, (sy + ty) / 2.0 + BOX_H / 2.0)
                } else {
                    ((sx + tx) / 2.0 + BOX_W / 2.0, (sy + BOX_H + ty) / 2.0)
                };
                edge_labels.push((mx, my, label.clone()));
            }
        }
    }

    let line_color: Hsla = rgbc(theme.muted).into();
    let width = total_w.max(220.0);
    let height = total_h.max(80.0);
    let mut diagram = div().relative().w(px(width)).h(px(height)).flex_shrink_0();
    let segments_for_canvas = segments.clone();
    diagram = diagram.child(
        canvas(
            move |bounds, _, _| (bounds, segments_for_canvas.clone()),
            move |bounds, (_, segments), window, _cx| {
                for segment in segments {
                    let color = line_color;
                    let start = point(
                        bounds.origin.x + px(segment.x1),
                        bounds.origin.y + px(segment.y1),
                    );
                    let end = point(
                        bounds.origin.x + px(segment.x2),
                        bounds.origin.y + px(segment.y2),
                    );
                    let left = start.x.min(end.x);
                    let top = start.y.min(end.y);
                    let rect = Bounds::new(
                        point(left, top),
                        size(
                            px(f32::from((end.x - start.x).abs()).max(1.0)),
                            px(f32::from((end.y - start.y).abs()).max(1.0)),
                        ),
                    );
                    window.paint_quad(fill(rect, color));
                }
            },
        )
        .absolute()
        .left(px(0.))
        .top(px(0.))
        .w(px(width))
        .h(px(height)),
    );
    for (x, y, label) in edge_labels {
        diagram = diagram.child(
            div()
                .absolute()
                .left(px(x - 10.0))
                .top(px(y - 7.0))
                .px_1()
                .rounded_sm()
                .bg(rgbc(theme.panel_alt))
                .text_size(px((base_size - 2.0).max(8.0)))
                .text_color(rgbc(theme.accent))
                .child(label),
        );
    }
    for node in nodes {
        let shape_round = if node.label.contains('{') {
            px(8.)
        } else {
            px(6.)
        };
        diagram = diagram.child(
            div()
                .id(gpui::SharedString::from(format!(
                    "mermaid-node-{}",
                    node.id
                )))
                .absolute()
                .left(px(node.x))
                .top(px(node.y))
                .w(px(BOX_W))
                .h(px(BOX_H))
                .px_2()
                .flex()
                .items_center()
                .justify_center()
                .rounded(shape_round)
                .bg(rgbc(theme.panel))
                .border_1()
                .border_color(rgbc(theme.accent))
                .text_size(px((base_size - 1.0).max(9.0)))
                .text_color(rgbc(theme.foreground))
                .overflow_hidden()
                .child(node.label),
        );
    }

    Some(
        div()
            .w_full()
            .min_w(px(0.))
            .rounded_md()
            .border_1()
            .border_color(rgbc(theme.border))
            .bg(rgba(theme.panel_alt, 0.7))
            .p_2()
            .flex()
            .flex_col()
            .gap_2()
            .child(
                div()
                    .text_size(px(9.))
                    .text_color(rgbc(theme.muted))
                    .child("MERMAID"),
            )
            .child(div().w_full().overflow_hidden().child(diagram)),
    )
}

fn add_node(labels: &mut HashMap<String, String>, order: &mut Vec<String>, id: &str, label: &str) {
    if !labels.contains_key(id) {
        order.push(id.to_string());
        labels.insert(id.to_string(), label.to_string());
    } else if !label.is_empty() && label != id {
        labels.insert(id.to_string(), label.to_string());
    }
}

fn parse_node(value: &str) -> Option<(String, String)> {
    let value = value.trim();
    if value.is_empty() {
        return None;
    }
    let end = value
        .find(|ch: char| !(ch.is_ascii_alphanumeric() || ch == '_' || ch == '-'))
        .unwrap_or(value.len());
    let id = value[..end].trim();
    if id.is_empty() {
        return None;
    }
    let rest = value[end..].trim();
    let label = if rest.is_empty() {
        id.to_string()
    } else {
        let inner = rest
            .trim_start_matches(['[', '(', '{', '>'])
            .trim_end_matches([']', ')', '}', '>'])
            .trim()
            .trim_matches(['"', '\''])
            .to_string();
        if inner.is_empty() {
            id.to_string()
        } else {
            inner
        }
    };
    Some((id.to_string(), label))
}

fn split_edge(line: &str) -> Option<(String, Option<String>, String)> {
    let operators = ["-->", "---", "-.->", "==>", "->"];
    let mut found: Option<(usize, &str)> = None;
    for operator in operators {
        if let Some(index) = line.find(operator) {
            if found.is_none_or(|(best, _)| index < best) {
                found = Some((index, operator));
            }
        }
    }
    let (index, operator) = found?;
    let left = line[..index].trim().to_string();
    let mut rest = line[index + operator.len()..].trim();
    let mut label = None;
    if rest.starts_with('|') {
        if let Some(end) = rest[1..].find('|') {
            label = Some(rest[1..1 + end].trim().to_string());
            rest = rest[2 + end..].trim();
        }
    }
    if rest.is_empty() {
        return None;
    }
    Some((left, label, rest.to_string()))
}
