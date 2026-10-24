use std::fmt;
use std::ops::Deref;
use std::rc::Rc;

use serde::{Deserialize, Serialize};
use tokio::sync::broadcast;
use xcord_store::SessionProfile;
use xcord_vt::Snapshot;

use crate::transport::HostKeySummary;

/// Stable id of an open tab. Survives scrolls and re-renders; not a PID.
#[derive(Copy, Clone, Debug, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
pub struct TerminalId(pub u64);

impl TerminalId {
    pub fn new(raw: u64) -> Self {
        Self(raw)
    }

    pub fn raw(self) -> u64 {
        self.0
    }
}

impl fmt::Display for TerminalId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "term-{}", self.0)
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum TabStatus {
    Connecting,
    Connected,
    Closed,
    Failed(String),
}

impl TabStatus {
    pub fn label(&self) -> &str {
        match self {
            TabStatus::Connecting => "connecting",
            TabStatus::Connected => "connected",
            TabStatus::Closed => "closed",
            TabStatus::Failed(_) => "failed",
        }
    }

    pub fn is_connected(&self) -> bool {
        matches!(self, TabStatus::Connected)
    }

    pub fn is_busy(&self) -> bool {
        matches!(self, TabStatus::Connecting)
    }
}

/// View model for a tab. Cheap to clone; the terminal buffer stays in the
/// workspace plugin.
#[derive(Clone, Debug)]
pub struct TabInfo {
    pub id: TerminalId,
    pub title: String,
    pub profile_id: String,
    pub profile_name: String,
    pub host: String,
    pub user: String,
    pub port: u16,
    pub status: TabStatus,
    pub host_key: Option<HostKeySummary>,
    pub bell: bool,
}

/// Everything needed to start a connection. Credentials are resolved by the
/// UI (keychain or a prompt) and only live in memory.
#[derive(Clone, Debug)]
pub struct OpenRequest {
    pub profile: SessionProfile,
    pub credential: Option<String>,
    pub cols: u16,
    pub rows: u16,
}

/// Result of a connection test that does not create a tab.
#[derive(Clone, Debug)]
pub struct ConnectionTest {
    pub id: u64,
    pub ok: bool,
    pub message: String,
}

/// Events broadcast to any number of views (tabs, overview grid, logger).
#[derive(Clone, Debug)]
pub enum WorkspaceEvent {
    TabsChanged,
    ActiveChanged(Option<TerminalId>),
    /// The terminal buffer changed; repaint the tab.
    Dirty(TerminalId),
    StatusChanged(TerminalId),
    Bell(TerminalId),
    Closed(TerminalId),
    TestFinished(ConnectionTest),
    Notice {
        level: crate::ui::NoticeLevel,
        message: String,
    },
}

/// The terminal/tab facade owned by the `workspace` plugin.
///
/// All methods are synchronous and cheap; they are called on the UI thread.
pub trait WorkspaceService {
    fn tabs(&self) -> Vec<TabInfo>;
    fn active(&self) -> Option<TerminalId>;
    fn select(&self, id: TerminalId);
    fn select_next(&self);
    fn select_previous(&self);

    fn open(&self, request: OpenRequest) -> Result<TerminalId, String>;
    fn close(&self, id: TerminalId);
    fn reconnect(&self, id: TerminalId) -> Result<TerminalId, String>;
    /// Verify a profile without opening a tab. The result arrives as
    /// [`WorkspaceEvent::TestFinished`] with the returned id.
    fn test_connection(&self, request: OpenRequest) -> Result<u64, String>;

    fn write(&self, id: TerminalId, data: &[u8]);
    fn resize(&self, id: TerminalId, cols: u16, rows: u16);

    fn snapshot(&self, id: TerminalId) -> Option<Snapshot>;
    /// Current grid size without cloning the whole viewport.
    fn dimensions(&self, id: TerminalId) -> Option<(u16, u16)>;
    fn scroll_by(&self, id: TerminalId, delta: i32);
    fn scroll_to(&self, id: TerminalId, offset: usize);
    fn scroll_to_bottom(&self, id: TerminalId);
    fn text_in_viewport(
        &self,
        id: TerminalId,
        start: (usize, usize),
        end: (usize, usize),
    ) -> String;

    /// Recent terminal output (scrollback + screen) for AI/MCP consumers.
    fn terminal_text(&self, id: TerminalId, max_lines: usize) -> Option<String>;

    fn subscribe(&self) -> broadcast::Receiver<WorkspaceEvent>;
}

/// Service wrapper: `ctx.require::<Workspace>(services::WORKSPACE)`.
#[derive(Clone)]
pub struct Workspace(pub Rc<dyn WorkspaceService>);

impl Deref for Workspace {
    type Target = dyn WorkspaceService;
    fn deref(&self) -> &Self::Target {
        &*self.0
    }
}
