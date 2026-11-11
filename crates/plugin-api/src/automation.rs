//! Rhai automation scripts that drive a live terminal.

use std::ops::Deref;
use std::rc::Rc;

use tokio::sync::broadcast;

use crate::workspace::TerminalId;

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
    pub logs: Vec<ScriptLog>,
    pub error: Option<String>,
}

#[derive(Clone, Debug)]
pub enum AutomationEvent {
    Changed { run_id: u64 },
}

pub trait AutomationService {
    fn scripts(&self) -> Vec<ScriptInfo>;
    fn script_source(&self, id: &str) -> Option<String>;
    fn create_script(&self, name: &str) -> Result<ScriptInfo, String>;
    fn scripts_dir(&self) -> String;
    fn run(&self, script_id: &str, terminal: TerminalId) -> Result<ScriptRunSnapshot, String>;
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
