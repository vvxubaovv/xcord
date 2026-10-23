use std::rc::Rc;

use xcord_cordis::{Context, Plugin, PluginMeta, Result};
use xcord_plugin_api::{services, TransportRegistry, Transports};

/// Hosts the registry every transport plugin (SSH, telnet, serial, ...) fills.
#[derive(Clone)]
pub struct TransportsPlugin;

impl Plugin for TransportsPlugin {
    fn meta(&self) -> PluginMeta {
        PluginMeta::new("transports", "Transport Registry")
            .description("Registry of connection providers")
            .author("xcord")
            .capabilities(&["core", "transport"])
    }

    fn apply(&self, ctx: Context, _config: serde_json::Value) -> Result<()> {
        let registry = Rc::new(TransportRegistry::default());
        ctx.provide(services::TRANSPORTS, Transports(registry))?;
        Ok(())
    }
}
