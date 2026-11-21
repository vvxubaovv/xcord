//! Stable contracts shared by the GPUI shell and first/third-party plugins.
//!
//! The kernel itself is deliberately unaware of SSH, terminals, or windows.
//! This crate defines the *services* those plugins provide:
//!
//! | service name  | wrapper        | purpose                                   |
//! |---------------|----------------|-------------------------------------------|
//! | `runtime`     | [`Runtime`]    | Tokio I/O pool + UI post-office           |
//! | `transports`  | [`Transports`] | registry of connection providers          |
//! | `sessions`    | [`Sessions`]   | profile CRUD + credential vault           |
//! | `workspace`   | [`Workspace`]  | tabs, terminals, connection lifecycle     |
//! | `commands`    | [`Commands`]   | command palette contributions             |
//! | `themes`      | [`Themes`]     | color schemes                             |
//! | `highlights`  | [`Highlights`] | regex output highlighting                 |
//! | `i18n`        | [`I18n`]       | locale catalogs for shell and plugins     |
//! | `ui-host`     | [`Ui`]         | commands from plugins into the GPUI shell |
//!
//! Wrappers exist because Rust's `Any` cannot downcast to unsized trait
//! objects. Each wrapper is `Clone` and cheap to fetch from a [`Context`].
//!
//! [`Context`]: xcord_cordis::Context

pub mod ai;
pub mod automation;
pub mod commands;
pub mod ftp;
pub mod highlight;
pub mod i18n;
pub mod mcp;
pub mod runtime;
pub mod session;
pub mod theme;
pub mod transport;
pub mod ui;
pub mod workspace;

pub use ai::{Ai, AiAssistant, AiConversationView, AiMessageView, AiSnapshot};
pub use automation::{
    Automation, AutomationEvent, AutomationService, RunStatus, ScriptDiagnostic, ScriptInfo,
    ScriptLog, ScriptLogLevel, ScriptRunSnapshot, AUTOMATION_SYNTAX,
};
pub use commands::{Command, CommandRegistry, Commands};
pub use ftp::{Ftp, FtpEntry, FtpEvent, FtpService, FtpSnapshot};
pub use highlight::{HighlightService, HighlightSpan, Highlights};
pub use i18n::{I18n, LocaleInfo, LocalizationService};
pub use mcp::{
    McpResourceRegistry, McpResources, McpServerStatus, McpStatus, McpToolRegistry, McpTools,
    ResourceContent, ResourceSpec, ToolSpec,
};
pub use runtime::{Job, PostOffice, Runtime, RuntimeHandle, RuntimeService};
pub use session::{SessionService, Sessions};
pub use theme::{Theme, ThemeService, Themes};
pub use transport::{
    AuthRequest, ConnectSpec, ConnectionInfo, HostKeySummary, Transport, TransportEvent,
    TransportProvider, TransportRegistry, Transports,
};
pub use ui::{NoticeLevel, Ui, UiCommand, UiHost};
pub use workspace::{
    ConnectionTest, OpenRequest, TabInfo, TabStatus, TerminalId, Workspace, WorkspaceEvent,
    WorkspaceService,
};

/// The conventional service names. Using constants avoids typos and lets
/// plugins be written against a documented surface.
pub mod services {
    pub const RUNTIME: &str = "runtime";
    pub const TRANSPORTS: &str = "transports";
    pub const SESSIONS: &str = "sessions";
    pub const WORKSPACE: &str = "workspace";
    pub const COMMANDS: &str = "commands";
    pub const THEMES: &str = "themes";
    pub const HIGHLIGHTS: &str = "highlights";
    pub const UI_HOST: &str = "ui-host";
    pub const I18N: &str = "i18n";
    pub const MCP_TOOLS: &str = "mcp-tools";
    pub const MCP_RESOURCES: &str = "mcp-resources";
    pub const MCP_STATUS: &str = "mcp-status";
    pub const AI: &str = "ai";
    pub const FTP: &str = "ftp";
    pub const AUTOMATION: &str = "automation";
}
