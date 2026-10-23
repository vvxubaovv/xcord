use std::rc::Rc;

use crate::{Context, Result};
use serde_json::Value as Config;

/// A plugin constructor. Factories are used instead of a plugin instance so the
/// kernel can create a **fresh** plugin when hot reloading.
pub type PluginFactory = Rc<dyn Fn() -> Box<dyn Plugin>>;

/// Static metadata used by the plugin manager UI and diagnostics.
#[derive(Debug, Clone)]
pub struct PluginMeta {
    /// Stable, machine-readable id, e.g. `"terminal"`.
    pub id: &'static str,
    /// Human-readable name.
    pub name: &'static str,
    pub version: &'static str,
    pub description: &'static str,
    pub author: &'static str,
    /// Free-form capability tags, e.g. `["transport", "ssh"]`.
    pub capabilities: &'static [&'static str],
}

impl PluginMeta {
    pub const fn new(id: &'static str, name: &'static str) -> Self {
        Self {
            id,
            name,
            version: env!("CARGO_PKG_VERSION"),
            description: "",
            author: "",
            capabilities: &[],
        }
    }

    pub const fn description(mut self, description: &'static str) -> Self {
        self.description = description;
        self
    }

    pub const fn author(mut self, author: &'static str) -> Self {
        self.author = author;
        self
    }

    pub const fn version(mut self, version: &'static str) -> Self {
        self.version = version;
        self
    }

    pub const fn capabilities(mut self, capabilities: &'static [&'static str]) -> Self {
        self.capabilities = capabilities;
        self
    }
}

impl Default for PluginMeta {
    fn default() -> Self {
        Self::new("unknown", "Unknown plugin")
    }
}

/// The unit of behaviour in an xcord application.
///
/// A plugin is passive: the kernel calls [`Plugin::apply`] when every service
/// named by [`Plugin::inject`] exists. Everything the plugin creates must be
/// registered through [`Context::effect`] (or helpers such as
/// [`Context::on`]/[`Context::provide`], which do it automatically), so it can
/// be torn down deterministically.
pub trait Plugin: 'static {
    /// Metadata shown in the plugin manager.
    fn meta(&self) -> PluginMeta;

    /// Names of services this plugin needs before it can start.
    fn inject(&self) -> &'static [&'static str] {
        &[]
    }

    /// Start the plugin.
    ///
    /// This is called on the UI thread. For long-running work, hand it to a
    /// runtime service and post results back to the UI (see
    /// `xcord-plugin-api::runtime`).
    fn apply(&self, ctx: Context, config: Config) -> Result<()>;
}

/// Create a [`PluginFactory`] from any `Fn() -> P`.
pub fn factory<P, F>(make: F) -> PluginFactory
where
    P: Plugin,
    F: Fn() -> P + 'static,
{
    Rc::new(move || Box::new(make()))
}
