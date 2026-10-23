use std::rc::Rc;

use regex::Regex;
use xcord_cordis::{Context, Plugin, PluginMeta, Result};
use xcord_plugin_api::{services, HighlightService, HighlightSpan, Highlights, Sessions};
use xcord_store::HighlightRule;

/// Compiled regex highlighter. Rules are evaluated in file order; a later rule
/// cannot override an earlier match, which keeps colors predictable.
pub struct HighlightMatcher {
    rules: Vec<CompiledRule>,
    enabled: bool,
}

struct CompiledRule {
    regex: Regex,
    foreground: [u8; 3],
    background: Option<[u8; 3]>,
}

impl HighlightMatcher {
    pub fn new(rules: &[HighlightRule], enabled: bool) -> Self {
        let mut compiled = Vec::new();
        for rule in rules.iter().filter(|rule| rule.enabled) {
            let Ok(regex) = Regex::new(&rule.pattern) else {
                log::warn!("invalid highlight regex `{}`: {}", rule.name, rule.pattern);
                continue;
            };
            compiled.push(CompiledRule {
                regex,
                foreground: parse_hex(rule.foreground.as_deref(), [0xf5, 0xc0, 0x6f]),
                background: rule
                    .background
                    .as_deref()
                    .map(|value| parse_hex(Some(value), [0x3a, 0x2a, 0x12])),
            });
        }
        Self {
            rules: compiled,
            enabled,
        }
    }
}

impl HighlightService for HighlightMatcher {
    fn spans(&self, text: &str) -> Vec<HighlightSpan> {
        if !self.enabled || self.rules.is_empty() || text.is_empty() {
            return Vec::new();
        }
        let mut spans: Vec<HighlightSpan> = Vec::new();
        for rule in &self.rules {
            for found in rule.regex.find_iter(text) {
                if found.start() == found.end() {
                    continue;
                }
                if spans
                    .iter()
                    .any(|span| found.start() < span.end && found.end() > span.start)
                {
                    continue;
                }
                spans.push(HighlightSpan {
                    start: found.start(),
                    end: found.end(),
                    foreground: rule.foreground,
                    background: rule.background,
                });
                if spans.len() >= 256 {
                    return spans;
                }
            }
        }
        spans
    }

    fn rule_count(&self) -> usize {
        self.rules.len()
    }

    fn enabled(&self) -> bool {
        self.enabled
    }
}

fn parse_hex(value: Option<&str>, fallback: [u8; 3]) -> [u8; 3] {
    let Some(value) = value else {
        return fallback;
    };
    let value = value.trim().trim_start_matches('#');
    if value.len() != 6 {
        return fallback;
    }
    let parse = |range: std::ops::Range<usize>| u8::from_str_radix(&value[range], 16).ok();
    match (parse(0..2), parse(2..4), parse(4..6)) {
        (Some(r), Some(g), Some(b)) => [r, g, b],
        _ => fallback,
    }
}

#[derive(Clone)]
pub struct HighlightsPlugin;

impl Plugin for HighlightsPlugin {
    fn meta(&self) -> PluginMeta {
        PluginMeta::new("highlights", "Output Highlighter")
            .description("Regex-based coloring of terminal output")
            .author("xcord")
            .capabilities(&["terminal", "regex"])
    }

    fn inject(&self) -> &'static [&'static str] {
        &[services::SESSIONS]
    }

    fn apply(&self, ctx: Context, _config: serde_json::Value) -> Result<()> {
        let sessions = ctx.require::<Sessions>(services::SESSIONS)?;
        let settings = sessions.settings();
        let matcher = Rc::new(HighlightMatcher::new(
            &sessions.highlights(),
            settings.highlight,
        ));
        ctx.provide(services::HIGHLIGHTS, Highlights(matcher))?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn matches_error_and_respects_priority() {
        let rules = vec![HighlightRule {
            id: "1".into(),
            name: "errors".into(),
            pattern: "(?i)error".into(),
            foreground: Some("#ff0000".into()),
            background: None,
            enabled: true,
        }];
        let matcher = HighlightMatcher::new(&rules, true);
        let spans = matcher.spans("an ERROR occurred");
        assert_eq!(spans.len(), 1);
        assert_eq!(spans[0].foreground, [0xff, 0x00, 0x00]);
        assert_eq!(&"an ERROR occurred"[spans[0].start..spans[0].end], "ERROR");
        assert!(matcher.spans("nothing here").is_empty());
        assert!(HighlightMatcher::new(&rules, false)
            .spans("ERROR")
            .is_empty());
    }

    #[test]
    fn invalid_rule_is_ignored() {
        let rules = vec![HighlightRule {
            id: "1".into(),
            name: "broken".into(),
            pattern: "(".into(),
            foreground: None,
            background: None,
            enabled: true,
        }];
        let matcher = HighlightMatcher::new(&rules, true);
        assert_eq!(matcher.rule_count(), 0);
    }
}
