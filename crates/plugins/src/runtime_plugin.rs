use xcord_cordis::{Context, Plugin, PluginMeta, Result};
use xcord_plugin_api::{services, Runtime, RuntimeService};

/// Owns the Tokio I/O runtime and the UI post-office.
#[derive(Clone)]
pub struct RuntimePlugin;

impl Plugin for RuntimePlugin {
    fn meta(&self) -> PluginMeta {
        PluginMeta::new("runtime", "I/O Runtime")
            .description("Tokio worker pool and UI post-office bridge")
            .author("xcord")
            .capabilities(&["core", "runtime"])
    }

    fn apply(&self, ctx: Context, config: serde_json::Value) -> Result<()> {
        let workers = config
            .get("workers")
            .and_then(|value| value.as_u64())
            .unwrap_or(2) as usize;

        let service = RuntimeService::new(ctx.clone(), workers).map_err(|error| {
            xcord_cordis::Error::PluginFailed {
                name: "runtime",
                message: format!("could not start Tokio: {error}"),
            }
        })?;

        ctx.provide(services::RUNTIME, Runtime(service.clone()))?;

        ctx.effect("shutdown io runtime", move || {
            service.shutdown();
        });

        Ok(())
    }
}
