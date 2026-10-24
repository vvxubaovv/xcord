//! AI copilot contracts shared by the `ai` plugin and the GPUI shell.

use std::ops::Deref;
use std::rc::Rc;

use crate::workspace::TerminalId;

#[derive(Clone, Debug, Default)]
pub struct AiMessageView {
    /// "You", "AI", "Tool call", "Tool result"
    pub role: String,
    pub text: String,
    /// Tool name when this entry is a tool call/result.
    pub tool: Option<String>,
}

#[derive(Clone, Debug, Default)]
pub struct AiSnapshot {
    pub messages: Vec<AiMessageView>,
    pub streaming: bool,
    pub status: String,
    pub error: Option<String>,
    pub model: String,
    pub base_url: String,
    pub has_key: bool,
    pub allow_writes: bool,
    pub include_terminal: bool,
    /// Terminal whose output is attached to the next prompt.
    pub context_terminal: Option<TerminalId>,
    pub context_title: Option<String>,
    /// True when no tab was pinned and the active tab is used.
    pub context_follows_active: bool,
}

/// The copilot facade. All methods run on the UI thread.
pub trait AiAssistant: 'static {
    fn snapshot(&self) -> AiSnapshot;
    fn send(&self, prompt: &str) -> Result<(), String>;
    fn clear(&self);
    /// Re-read key presence after the shell updates the vault.
    fn refresh_key(&self);
    /// Pin the terminal used for conversation context. `None` follows the
    /// active tab.
    fn set_context_terminal(&self, id: Option<TerminalId>);
    fn context_terminal(&self) -> Option<TerminalId>;
}

/// Service wrapper: `ctx.require::<Ai>("ai")`.
#[derive(Clone)]
pub struct Ai(pub Rc<dyn AiAssistant>);

impl Deref for Ai {
    type Target = dyn AiAssistant;
    fn deref(&self) -> &Self::Target {
        &*self.0
    }
}
