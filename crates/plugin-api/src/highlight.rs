use std::ops::Deref;
use std::rc::Rc;

/// A styled byte range within one rendered row.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct HighlightSpan {
    pub start: usize,
    pub end: usize,
    pub foreground: [u8; 3],
    pub background: Option<[u8; 3]>,
}

/// Regex-based output highlighting. The implementation compiles rules once and
/// is queried by the renderer for each visible row.
pub trait HighlightService {
    fn spans(&self, text: &str) -> Vec<HighlightSpan>;
    fn rule_count(&self) -> usize;
    fn enabled(&self) -> bool;
}

/// Service wrapper: `ctx.require::<Highlights>(services::HIGHLIGHTS)`.
#[derive(Clone)]
pub struct Highlights(pub Rc<dyn HighlightService>);

impl Deref for Highlights {
    type Target = dyn HighlightService;
    fn deref(&self) -> &Self::Target {
        &*self.0
    }
}
