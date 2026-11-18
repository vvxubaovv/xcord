//! Rhai automation scripts that drive a live terminal.

use std::ops::Deref;
use std::rc::Rc;

use tokio::sync::broadcast;

use crate::workspace::TerminalId;

/// Rhai API reference injected into AI copilot context and exposed by MCP tools.
pub const AUTOMATION_SYNTAX: &str = r#"Xcord automation scripts are Rhai scripts that drive a live SSH terminal.
Available functions:
- send(text): send text without pressing Enter.
- send_line(text): send text followed by Enter.
- send_key(key): send a named key such as enter, tab, escape, ctrl+c, up, down.
- expect(pattern, ms): wait for literal text, returning an object with pattern, text and before fields.
- expect_regex(pattern, ms): wait for a regular expression.
- expect_any([patterns], ms): wait for any of several patterns.
- wait_idle(idle_ms, timeout): wait until terminal output becomes quiet.
- read_screen() / read_scrollback(n): read terminal text.
- sleep(ms), log(msg), fail(msg): helpers.

Important semantics:
- Expect/read listeners must start before send/send_line to see new output.
- expect only matches output produced after the current send.
- Scripts run on the active terminal and must be valid Rhai.
Example:
let m = expect_any(["ok", "failed"], 5000);
if m.pattern == "ok" { log("done"); }
"#;

#[derive(Clone, Debug, Default)]
pub struct ScriptInfo {
    pub id: String,
    pub name: String,
    pub path: String,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RunStatus {
    Running,
    Success,
    Failed,
    Stopped,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ScriptLogLevel {
    Info,
    Warning,
    Error,
}

#[derive(Clone, Debug)]
pub struct ScriptLog {
    pub level: ScriptLogLevel,
    pub message: String,
}

#[derive(Clone, Debug)]
pub struct ScriptRunSnapshot {
    pub run_id: u64,
    pub script_id: String,
    pub script_name: String,
    pub terminal: TerminalId,
    pub status: RunStatus,
    pub current_line: usize,
    pub logs: Vec<ScriptLog>,
    pub error: Option<String>,
}

#[derive(Clone, Debug)]
pub struct ScriptDiagnostic {
    pub line: usize,
    pub column: usize,
    pub message: String,
}

#[derive(Clone, Debug)]
pub enum AutomationEvent {
    Changed { run_id: u64 },
    ScriptsChanged,
}

pub trait AutomationService {
    fn scripts(&self) -> Vec<ScriptInfo>;
    fn script_source(&self, id: &str) -> Option<String>;
    fn validate(&self, source: &str) -> Option<ScriptDiagnostic>;
    fn create_script(&self, name: &str) -> Result<ScriptInfo, String>;
    /// Overwrite an existing managed script. The UI reloads it after ScriptsChanged.
    fn write_script(&self, id: &str, source: &str) -> Result<(), String>;
    fn scripts_dir(&self) -> String;
    fn run(&self, script_id: &str, terminal: TerminalId) -> Result<ScriptRunSnapshot, String>;
    /// Run a script from an arbitrary file. The file is read at run time.
    fn run_file(&self, path: &str, terminal: TerminalId) -> Result<ScriptRunSnapshot, String>;
    fn stop(&self, run_id: u64);
    /// UI-thread pump: executes queued send commands.
    fn pump(&self);
    /// UI-thread feed with the latest terminal history.
    fn update_terminal(&self, terminal: TerminalId, text: String);
    fn active_run(&self) -> Option<ScriptRunSnapshot>;
    fn run_snapshot(&self, run_id: u64) -> Option<ScriptRunSnapshot>;
    fn subscribe(&self) -> broadcast::Receiver<AutomationEvent>;
}

#[derive(Clone)]
pub struct Automation(pub Rc<dyn AutomationService>);

impl Deref for Automation {
    type Target = dyn AutomationService;
    fn deref(&self) -> &Self::Target {
        &*self.0
    }
}
