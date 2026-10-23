use std::rc::Rc;

use xcord_cordis::{Context, Plugin, PluginMeta, Result};
use xcord_plugin_api::{services, Command, CommandRegistry, Commands};

/// Owns the command palette table. Other plugins add commands in `apply`.
#[derive(Clone)]
pub struct CommandsPlugin;

impl Plugin for CommandsPlugin {
    fn meta(&self) -> PluginMeta {
        PluginMeta::new("commands", "Command Registry")
            .description("Command palette contributions from every plugin")
            .author("xcord")
            .capabilities(&["core", "commands"])
    }

    fn apply(&self, ctx: Context, _config: serde_json::Value) -> Result<()> {
        let registry = Rc::new(CommandRegistry::new());
        ctx.provide(services::COMMANDS, Commands(registry.clone()))?;

        let handle = registry.register(Command::new(
            "app.quit",
            "Quit Xcord",
            "Application",
            |ctx| {
                if let Some(ui) = ctx.service::<xcord_plugin_api::Ui>(services::UI_HOST) {
                    ui.send(xcord_plugin_api::UiCommand::Notify {
                        level: xcord_plugin_api::NoticeLevel::Info,
                        message: "Use the window close button to quit.".into(),
                    });
                }
            },
        ));
        ctx.effect("remove builtin command", move || {
            registry.unregister(handle)
        });
        Ok(())
    }
}
