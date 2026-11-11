//! First-party plugins.
//!
//! The GPUI application is only a shell: it provides the `ui-host` service and
//! calls [`install`]. Every feature below is a plugin that can be disabled,
//! replaced, or hot-reloaded independently.
//!
//! Loading order matters only for service availability; the kernel keeps
//! plugins pending until their injected services appear, so registration order
//! is a declaration of intent rather than a hard requirement.

mod ai;
mod automation;
mod commands_plugin;
mod ftp;
mod highlights;
mod i18n;
mod logging;
mod mcp;
mod runtime_plugin;
mod sessions;
mod ssh_transport;
mod themes;
mod transports;
mod workspace;
mod zmodem;

pub use highlights::HighlightMatcher;
pub use logging::TerminalOutputEvent;
pub use workspace::{WorkspacePlugin, WorkspaceState};

use xcord_cordis::Context;

/// Install the standard feature set.
///
/// The shell must provide the `ui-host` service before calling this for the
/// workspace plugin to start immediately. If it is provided later, the plugin
/// starts as soon as the service appears.
pub fn install(ctx: &Context) {
    ctx.plugin(runtime_plugin::RuntimePlugin);
    ctx.plugin(commands_plugin::CommandsPlugin);
    ctx.plugin(transports::TransportsPlugin);
    ctx.plugin(ssh_transport::SshTransportPlugin);
    ctx.plugin(sessions::SessionsPlugin);
    ctx.plugin(i18n::I18nPlugin);
    ctx.plugin(themes::ThemesPlugin);
    ctx.plugin(workspace::WorkspacePlugin);
    ctx.plugin(highlights::HighlightsPlugin);
    ctx.plugin(logging::LoggingPlugin);
    ctx.plugin(mcp::McpPlugin);
    ctx.plugin(ai::AiPlugin);
    ctx.plugin(ftp::FtpPlugin);
    ctx.plugin(automation::AutomationPlugin);
}
