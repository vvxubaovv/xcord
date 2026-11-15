# Plugin templates

Copy these into `crates/plugins/src/<your_plugin>.rs`. They intentionally avoid
GPUI; for UI see `docs/plugin-development/ui-integration.md`.

## 1. Minimal command-only plugin

Best for a palette action, notification, or small utility.

```rust
use std::rc::Rc;

use xcord_cordis::{Context, Plugin, PluginMeta, Result};
use xcord_plugin_api::{
    services, Command, Commands, I18n, NoticeLevel, Ui, UiCommand,
};

#[derive(Clone)]
pub struct MyPlugin;

impl Plugin for MyPlugin {
    fn meta(&self) -> PluginMeta {
        PluginMeta::new("my-plugin", "My Plugin")
            .description("One sentence for the plugin manager")
            .author("you")
            .capabilities(&["example"])
    }

    fn inject(&self) -> &'static [&'static str] {
        &[services::COMMANDS, services::UI_HOST, services::I18N]
    }

    fn apply(&self, ctx: Context, _config: serde_json::Value) -> Result<()> {
        let commands = (*ctx.require::<Commands>(services::COMMANDS)?).clone();
        let ui = (*ctx.require::<Ui>(services::UI_HOST)?).clone();
        let i18n = (*ctx.require::<I18n>(services::I18N)?).clone();

        let handle = commands.register(Command::new(
            "my-plugin.run",
            i18n.t("app", "Run my plugin"),
            "My Plugin",
            move |_| {
                ui.send(UiCommand::Notify {
                    level: NoticeLevel::Success,
                    message: "My plugin ran".into(),
                });
            },
        ));
        ctx.effect("unregister my-plugin.run", move || {
            commands.unregister(handle);
        });

        Ok(())
    }
}
```

Register in `crates/plugins/src/lib.rs`:

```rust
mod my_plugin;
// inside install()
ctx.plugin(my_plugin::MyPlugin);
```

## 2. Service + snapshot + background worker

This is the recommended shape for a feature with state and a UI panel.

### 2.1 Public contract (`crates/plugin-api/src/my_feature.rs`)

```rust
use std::ops::Deref;
use std::rc::Rc;

#[derive(Clone, Debug, Default)]
pub struct MySnapshot {
    pub status: String,
    pub items: Vec<String>,
    pub error: Option<String>,
}

#[derive(Clone, Debug)]
pub enum MyEvent {
    Changed,
}

pub trait MyFeatureService {
    fn snapshot(&self) -> MySnapshot;
    fn refresh(&self);
    fn subscribe(&self) -> tokio::sync::broadcast::Receiver<MyEvent>;
}

#[derive(Clone)]
pub struct MyFeature(pub Rc<dyn MyFeatureService>);

impl Deref for MyFeature {
    type Target = dyn MyFeatureService;
    fn deref(&self) -> &Self::Target {
        &*self.0
    }
}
```

Export in `plugin-api/src/lib.rs` and add:

```rust
pub const MY_FEATURE: &str = "my-feature";
```

### 2.2 Plugin implementation

```rust
use std::rc::Rc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use tokio::sync::broadcast;
use xcord_cordis::{Context, Plugin, PluginMeta, Result};
use xcord_plugin_api::{
    services, Command, Commands, I18n, MyEvent, MyFeature, MyFeatureService, MySnapshot,
    NoticeLevel, Runtime, RuntimeHandle, Ui, UiCommand,
};

#[derive(Clone)]
pub struct MyFeaturePlugin;

#[derive(Default)]
struct Data {
    status: String,
    items: Vec<String>,
    error: Option<String>,
}

struct Shared {
    runtime: RuntimeHandle,
    data: Mutex<Data>,
    events: broadcast::Sender<MyEvent>,
    stopped: AtomicBool,
}

struct UiState {
    shared: Arc<Shared>,
}

impl MyFeatureService for UiState {
    fn snapshot(&self) -> MySnapshot {
        let data = self.shared.data.lock().unwrap();
        MySnapshot {
            status: data.status.clone(),
            items: data.items.clone(),
            error: data.error.clone(),
        }
    }

    fn refresh(&self) {
        if self.shared.stopped.load(Ordering::Relaxed) {
            return;
        }
        let shared = self.shared.clone();
        self.shared.runtime.spawn(async move {
            let result = refresh_remote().await; // your async I/O
            {
                let mut data = shared.data.lock().unwrap();
                match result {
                    Ok(items) => {
                        data.items = items;
                        data.error = None;
                        data.status = "ready".into();
                    }
                    Err(error) => {
                        data.error = Some(error);
                        data.status = "failed".into();
                    }
                }
            }
            let _ = shared.events.send(MyEvent::Changed);
        });
    }

    fn subscribe(&self) -> broadcast::Receiver<MyEvent> {
        self.shared.events.subscribe()
    }
}

async fn refresh_remote() -> std::result::Result<Vec<String>, String> {
    // Do not put blocking network I/O here. Use async reqwest / tokio I/O.
    Ok(vec!["item".to_string()])
}

impl Plugin for MyFeaturePlugin {
    fn meta(&self) -> PluginMeta {
        PluginMeta::new("my-feature", "My Feature")
            .description("Background example")
            .author("you")
            .capabilities(&["example"])
    }

    fn inject(&self) -> &'static [&'static str] {
        &[
            services::RUNTIME,
            services::COMMANDS,
            services::UI_HOST,
            services::I18N,
        ]
    }

    fn apply(&self, ctx: Context, _config: serde_json::Value) -> Result<()> {
        let runtime = ctx.require::<Runtime>(services::RUNTIME)?.handle();
        let commands = (*ctx.require::<Commands>(services::COMMANDS)?).clone();
        let ui = (*ctx.require::<Ui>(services::UI_HOST)?).clone();
        let i18n = (*ctx.require::<I18n>(services::I18N)?).clone();

        let (events, _) = broadcast::channel(64);
        let shared = Arc::new(Shared {
            runtime,
            data: Mutex::new(Data::default()),
            events,
            stopped: AtomicBool::new(false),
        });

        let feature_state = Rc::new(UiState {
            shared: shared.clone(),
        });
        ctx.provide(services::MY_FEATURE, MyFeature(feature_state.clone()))?;

        let handle = commands.register(Command::new(
            "my-feature.refresh",
            i18n.t("app", "Refresh my feature"),
            "My Feature",
            {
                let feature_state = feature_state.clone();
                let ui = ui.clone();
                move |_| {
                    // Fire-and-forget: spawn a background refresh.
                    feature_state.refresh();
                    ui.send(UiCommand::Notify {
                        level: NoticeLevel::Info,
                        message: "Refresh started".into(),
                    });
                }
            },
        ));
        ctx.effect("unregister my-feature command", move || {
            commands.unregister(handle);
        });
        ctx.effect("stop my-feature worker", move || {
            shared.stopped.store(true, Ordering::Relaxed);
        });

        Ok(())
    }
}
```

`feature_state` is the UI-side service and is cloned into the command closure.
The background work shares only `Arc<Shared>`; it never captures `Rc`.

## 3. Optional MCP tool for the same service

Add this in `apply` after providing the service:

```rust
let tools = (*ctx.require::<xcord_plugin_api::McpTools>(
    services::MCP_TOOLS,
)?)
.clone();

let handle = tools.register(
    xcord_plugin_api::ToolSpec {
        name: "my_feature_snapshot".into(),
        title: "My feature snapshot".into(),
        description: "Return the current My Feature snapshot as JSON.".into(),
        input_schema: serde_json::json!({
            "type": "object",
            "properties": {},
            "additionalProperties": false
        }),
        read_only: true,
    },
    |ctx, _args| {
        let service = ctx.require::<MyFeature>(services::MY_FEATURE)?;
        let snapshot = service.snapshot();
        Ok(serde_json::json!({
            "status": snapshot.status,
            "items": snapshot.items,
            "error": snapshot.error,
        }))
    },
);
ctx.effect("unregister my-feature mcp tool", move || {
    tools.unregister(handle)
});
```

Remember to inject `services::MCP_TOOLS` (or use `ctx.service` and skip
registration when absent).

## 4. Shell UI template

For a side panel, see `docs/plugin-development/ui-integration.md`. The essential
shell diff is:

```rust
// root.rs field
my_open: bool,
my_snapshot: MySnapshot,
my_feature: MyFeature,

// handle_ui_command
UiCommand::ToggleMyFeature => {
    self.my_open = !self.my_open;
    self.my_snapshot = self.my_feature.snapshot();
    cx.notify();
}

// Render body
.when(self.my_open, |body| body.child(self.render_my_feature_panel(cx)))
```

## 5. Cargo dependencies

Typical plugin module needs no new crate. Add to
`crates/plugins/Cargo.toml` only when using new libraries:

```toml
my-crate = "1"
```

Use workspace dependencies where possible (`tokio.workspace = true`,
`serde.workspace = true`, etc.). Do not add a second async runtime.

## 6. Tests template

Test pure logic directly. For kernel behavior, use a tiny test plugin so the
test has no unrelated dependencies:

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use xcord_cordis::Kernel;

    #[derive(Clone)]
    struct NeedsDep;

    impl Plugin for NeedsDep {
        fn meta(&self) -> PluginMeta { PluginMeta::new("needs-dep", "Needs Dep") }
        fn inject(&self) -> &'static [&'static str] { &["test-dep"] }
        fn apply(&self, _ctx: Context, _config: serde_json::Value) -> Result<()> {
            Ok(())
        }
    }

    #[test]
    fn plugin_waits_for_dependency() {
        let kernel = Kernel::new();
        let ctx = kernel.context();
        ctx.plugin(NeedsDep);

        let pending = kernel.active_plugins().is_empty();
        assert!(pending);

        ctx.provide("test-dep", 1u32).unwrap();
        assert!(kernel.active_plugins().iter().any(|p| p.name == "Needs Dep"));
    }
}
```

For a plugin with real services, provide the exact wrappers it injects
(for example `Commands(Rc::new(CommandRegistry::new()))`) or reduce the
`inject` list in a test-only variant. Do not assert "any plugin exists" when
you mean "my plugin is active".

## 7. Build sequence

```bash
cargo fmt --all
cargo check --workspace
cargo test --workspace
cargo build -p xcord
```

Fix all warnings in new code. Do not commit with a failed build.
