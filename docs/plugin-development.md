# Xcord plugin development

This is the complete guide for adding a feature to Xcord as a plugin. It is
written for both humans and coding agents. Follow it literally; the APIs below
are the ones that exist in this checkout.

> **Read this first:** `ARCHITECTURE.md` explains the overall runtime model.
> This guide focuses on the concrete steps, exact file locations, and failure
> modes of plugin development.

## 0. What "plugin" means in Xcord

Xcord is a native Rust/GPUI application. A plugin is a Rust type implementing
`xcord_cordis::Plugin`, registered in `crates/plugins/src/lib.rs`.

There are two important constraints:

1. **Plugins are statically linked.** There is no `.dll`/`.so` plugin loading.
   "Adding a plugin" means adding Rust code and rebuilding `xcord`.
2. **"Hot reload" reloads the plugin instance, not new compiled code.**
   `Kernel::reload(id)` supports unloading and re-applying the same compiled
   plugin with the same registration config. Code edits still require a rebuild
   and app restart.

The kernel, service API, and GPUI shell are separate:

```text
apps/xcord (GPUI shell, ui-host implementation)
        │
crates/plugins (first-party feature plugins)
        │
crates/plugin-api (service contracts shared by shell and plugins)
        │
crates/cordis (kernel: Context, Plugin, services, events, effects)
```

`crates/cordis` knows nothing about SSH, terminals, or windows.

## 1. File map

| Path | Purpose |
|---|---|
| `crates/cordis/src/plugin.rs` | `Plugin`, `PluginMeta`, factories |
| `crates/cordis/src/context.rs` | `Context`, `provide`, `require`, `effect`, `on`, `plugin` |
| `crates/cordis/src/kernel.rs` | registration, dependency gating, unload/reload, plugin states |
| `crates/cordis/src/event.rs` | typed `EventBus` |
| `crates/plugin-api/src/lib.rs` | service names and service wrappers |
| `crates/plugin-api/src/*.rs` | service traits and view models |
| `crates/plugins/src/lib.rs` | the canonical plugin installation order |
| `crates/plugins/src/*.rs` | reference plugins |
| `apps/xcord/src/root.rs` | GPUI shell: consumes services, renders panels, pumps events |
| `apps/xcord/src/text_input.rs` | single-line input widget |
| `apps/xcord/src/selectable_text.rs` | read-only selectable/copyable text |
| `apps/xcord/src/script_editor.rs` | multi-line editor used by automation |

Good reference plugins:

| Plugin | Demonstrates |
|---|---|
| `runtime_plugin.rs` | owning a runtime and providing a service |
| `commands_plugin.rs` | a tiny service provider |
| `themes.rs` | registry + UI-consumed service |
| `ftp.rs` | UI service + background I/O + snapshots + broadcast events |
| `automation.rs` | background worker + UI pump + service + editor |
| `ai.rs` | async streaming + PostOffice + UI events |
| `mcp.rs` | MCP tools/resources registration |
| `sessions.rs` | persistence, settings, keychain-backed credentials |

## 2. Kernel mental model

### 2.1 Lifecycle

A plugin is registered with a factory. The kernel:

1. creates a probe instance to read `meta()` and `inject()`,
2. stores the plugin in `Pending`,
3. starts it when every injected service exists,
4. calls `apply(ctx, config)` **on the UI thread**,
5. marks it `Active` if `apply` returns `Ok(())`,
6. on error/panic marks it `Failed`, disposes its scope, and removes its services,
7. on unload first unloads every plugin that depends on services it owns,
8. runs effects in reverse registration order (LIFO).

States: `Pending`, `Loading`, `Active`, `Failed`, `Unloading`, `Disposed`.

Useful kernel APIs:

```rust
let kernel = Kernel::new();
let ctx = kernel.context();

ctx.plugin(MyPlugin);                  // config = Null
ctx.plugin_with(MyPlugin, json!({ "foo": 1 }));
ctx.plugin_factory(|| MyPlugin::new(), json!({}));
kernel.plugins();                      // Vec<PluginInfo>
kernel.active_plugins();
kernel.diagnostics();                  // startup errors
kernel.reload(plugin_id);
kernel.unload(plugin_id);
kernel.shutdown();
```

`PluginInfo` includes id, name, version, description, author, capabilities,
inject list, state, and error.

### 2.2 `Plugin` trait

```rust
pub trait Plugin: 'static {
    fn meta(&self) -> PluginMeta;
    fn inject(&self) -> &'static [&'static str] { &[] }
    fn apply(&self, ctx: Context, config: serde_json::Value)
        -> xcord_cordis::Result<()>;
}
```

`meta()` is called on a probe instance; keep it cheap and deterministic.
`apply()` runs on the UI thread. Do not perform blocking network/file I/O there.
For long work, use the `runtime` service and post results back to the UI.

Minimal implementation:

```rust
use xcord_cordis::{Context, Plugin, PluginMeta, Result};

#[derive(Clone)]
pub struct HelloPlugin;

impl Plugin for HelloPlugin {
    fn meta(&self) -> PluginMeta {
        PluginMeta::new("hello", "Hello")
            .description("Example plugin")
            .author("you")
            .capabilities(&["example"])
    }

    fn apply(&self, _ctx: Context, _config: serde_json::Value) -> Result<()> {
        Ok(())
    }
}
```

### 2.3 `Context`

`Context` is cheap to clone. Clones share the same plugin scope and owner.

```rust
ctx.owner();                         // Option<PluginId>
ctx.kernel();                        // Kernel
ctx.label();                         // scope label
ctx.provide(services::X, wrapper)?;  // typed service
ctx.provide_rc(services::X, rc)?;
ctx.require::<X>(services::X)?;      // Rc<X>
ctx.service::<X>(services::X);       // Option<Rc<X>>
ctx.has_service(services::X);
ctx.service_names();
ctx.withdraw(services::X)?;          // only owner
ctx.effect("cleanup label", move || { /* LIFO */ });
ctx.on_cleanup("same", move || {});
ctx.emit(&event);
ctx.on::<Event>(|event, ctx| { /* ... */ }); // auto-unsubscribed
ctx.plugin(ChildPlugin);
```

Rules:

- `provide` stores a `T` in an `Any` slot. The type must match exactly when
  fetched. Use the provided wrapper types; do not provide the bare state type
  when the trait defines a wrapper.
- A service name is globally unique. Duplicate `provide` returns
  `Error::DuplicateService`.
- Only the owning plugin can `withdraw`.
- Unloading automatically removes services owned by that plugin; do not
  manually withdraw in normal code.
- `ctx.on` automatically registers an unsubscription effect.
- `ctx.effect` is for resources that do not have a better scoped helper:
  command unregistration, i18n unregistration, abort handles, channel senders,
  background tasks, file handles.
- Do not hold a `RefCell` borrow across a callback that can re-enter the same
  service. This is the most common Rust-side deadlock.

### 2.4 Dependency injection

`inject()` is a declaration, not a call. If even one service is missing, the
plugin remains `Pending` and **does not run** `apply`.

```rust
fn inject(&self) -> &'static [&'static str] {
    &[services::RUNTIME, services::WORKSPACE, services::COMMANDS]
}
```

Always keep `inject` exactly in sync with `ctx.require` calls. Calling
`require` for an undeclared service is a bug: it can fail at runtime instead of
waiting.

### 2.5 Effects and unload

```rust
let commands = (*ctx.require::<Commands>(services::COMMANDS)?).clone();
let handle = commands.register(command);
ctx.effect("unregister my command", move || {
    commands.unregister(handle);
});
```

When a plugin is unloaded:

1. dependents unload first,
2. `Unloading` is emitted,
3. effects run LIFO,
4. owned services are removed,
5. state becomes `Disposed`.

Effects catch panics so one bad cleanup cannot stop the rest, but do not rely on
that: log and return normally.

### 2.6 Events

Events are keyed by Rust type. Any `'static` type can be an event.

```rust
#[derive(Clone, Debug)]
pub struct MyEvent { pub id: String }

ctx.emit(&MyEvent { id: "a".into() });
ctx.on::<MyEvent>(|event, _ctx| {
    // Runs on the UI thread, synchronously.
});
```

For background-to-UI communication use one of:

1. a `tokio::sync::broadcast::Sender<T>` owned by the plugin and subscribed by
   the shell (simplest for snapshots/status),
2. a `PostOffice` job when you specifically need a `&Context` on the UI thread.

Prefer broadcast snapshots: they decouple UI rendering from worker state. Always
handle `RecvError::Lagged` in the UI pump.

## 3. Service contracts

All shared service names live in `crates/plugin-api/src/lib.rs` under
`services`. Every service has a wrapper because `Any` cannot downcast to an
unsized trait object.

```rust
pub struct Workspace(pub Rc<dyn WorkspaceService>);
impl Deref for Workspace { ... }
```

Fetching is always:

```rust
let workspace = (*ctx.require::<Workspace>(services::WORKSPACE)?).clone();
```

### 3.1 `runtime` — `Runtime`

Owns Tokio and the UI post office.

```rust
let runtime = ctx.require::<Runtime>(services::RUNTIME)?.handle();
runtime.spawn(async move { ... });       // Send future
runtime.handle.spawn_blocking(|| ...);
runtime.post(|ctx| { ... });            // UI thread, gets &Context
runtime.sleep(Duration::from_secs(1)).await;
```

`RuntimeHandle` is `Clone + Send`; `RuntimeService` itself is UI-thread-only.

`PostOffice::post` accepts a `Send` closure; the closure receives `&Context`
only when executed on the UI thread. Never capture `Rc`/`RefCell` plugin state
in it. Capture `Send` values (`Arc<Mutex<_>>`, channels, Strings) and look up
UI services inside the closure.

The shell already runs a job pump. For a new plugin, prefer a broadcast channel
that the shell subscribes to; then you do not need to add another job pump.

### 3.2 `workspace` — `Workspace`

The terminal/tab facade. All methods are synchronous UI-thread calls.

Key operations:

```rust
let tabs = workspace.tabs();
let active = workspace.active();
workspace.select(id);
workspace.open(OpenRequest { ... });     // Result<TerminalId, String>
workspace.close(id);
workspace.reconnect(id);
workspace.write(id, b"ls\r");
workspace.resize(id, cols, rows);
workspace.snapshot(id);                  // Option<xcord_vt::Snapshot>
workspace.history_text(id);              // plain text
workspace.terminal_text(id, max_lines);  // recent text
workspace.text_in_viewport(id, start, end);
workspace.scroll_by(id, delta);
workspace.scroll_to(id, offset);
workspace.scroll_to_bottom(id);
workspace.begin_output_capture(id);
workspace.take_output_capture(id);
workspace.zmodem_upload(id, paths);
workspace.zmodem_cancel(id);
workspace.subscribe();                    // broadcast::Receiver<WorkspaceEvent>
```

`WorkspaceEvent` includes `TabsChanged`, `ActiveChanged`, `Dirty`,
`StatusChanged`, `Bell`, `Closed`, `TestFinished`, `CommandSubmitted`,
`FileSaved`, `ZmodemUploadRequested`, and `Notice`.

The workspace owns VT state. Do not try to parse raw terminal bytes yourself
unless you are writing a transport/protocol plugin. Use the snapshot/history
APIs for text.

### 3.3 `sessions` — `Sessions`

Profiles, groups, snippets, highlight rules, settings, and the platform vault.

```rust
let sessions = (*ctx.require::<Sessions>(services::SESSIONS)?).clone();
let profile = sessions.get(id);
sessions.save(profile)?;
let settings = sessions.settings();
sessions.save_settings(settings)?;
let password = sessions.password(&profile);
sessions.store_password(&profile, "secret")?;
sessions.clear_password(&profile)?;
sessions.export(path)?;
sessions.import(path)?;
```

Do not persist secrets yourself. Use the vault methods. Plain `config.toml` is
for non-secret settings.

### 3.4 `commands` — `Commands`

```rust
let commands = (*ctx.require::<Commands>(services::COMMANDS)?).clone();
let handle = commands.register(
    Command::new("my.feature", "My feature", "My category", move |_ctx| {
        // Runs on the UI thread.
    }).shortcut("Ctrl+Shift+Y")
);
ctx.effect("unregister my.feature", move || commands.unregister(handle));
```

Command IDs are globally visible to the command palette and MCP
`run_command` (if the MCP plugin is active). Use a stable dotted id. Do not
block inside a command handler; start background work or show UI.

### 3.5 `ui-host` — `Ui`

This is the only sanctioned plugin-to-UI path:

```rust
ui.send(UiCommand::Notify {
    level: NoticeLevel::Info,
    message: "done".into(),
});
ui.send(UiCommand::ToggleFtp);
```

The enum currently contains session/palette/settings/AI/FTP/automation
commands. If your plugin needs a new UI surface, see
`docs/plugin-development/ui-integration.md`: you must add an enum variant and
handle it in the GPUI shell. A plugin cannot construct GPUI widgets directly.

### 3.6 `i18n` — `I18n`

```rust
let i18n = (*ctx.require::<I18n>(services::I18N)?).clone();
let handle = i18n.register_pairs("my-plugin", "zh-CN", &[
    ("My feature", "我的功能"),
]);
ctx.effect("unregister my translations", move || i18n.unregister(handle));

let title = i18n.t("my-plugin", "My feature");
```

Use source English strings as keys and register Chinese (`zh-CN`) entries when
the app defaults to Chinese. Keep domain names unique. The shell calls
translation at render time, so locale changes are immediate.

### 3.7 `themes` — `Themes`

```rust
let theme = ctx.require::<Themes>(services::THEMES)?.active();
let accent = theme.accent; // [u8; 3]
```

Respect the active theme. Use `theme.panel`, `panel_alt`, `border`, `accent`,
`muted`, `danger`, `success`, `selection`, and `palette` rather than hard-coded
colors.

### 3.8 `highlights` — `Highlights`

```rust
let spans = highlights.spans(text);
```

The terminal renderer queries this per row. Rules are persisted by the sessions
plugin. A plugin can add rules through `Sessions::save_highlight`; don't mutate
the service's internal state directly.

### 3.9 `transports` — `Transports` / `TransportProvider`

Only needed for a new connection protocol.

```rust
pub trait TransportProvider: 'static {
    fn kind(&self) -> &'static str;
    fn connect(&self, spec: ConnectSpec, events: async_channel::Sender<TransportEvent>)
        -> Result<(), String>;
}

pub trait Transport: Send + 'static {
    fn write(&self, data: &[u8]) -> Result<(), String>;
    fn resize(&self, cols: u16, rows: u16);
    fn close(&self);
    fn description(&self) -> String;
}
```

Register in the `transports` registry and unregister with an effect. `connect`
must return immediately; do work on `Runtime`. The workspace plugin owns tab
lifecycle and consumes events. `TransportEvent::Connected` carries the transport
handle; after that the UI thread only calls `write/resize/close`.

### 3.10 `mcp-tools` / `mcp-resources` — MCP capabilities

If the feature is useful to an AI client, expose it as an MCP tool/resource too.

```rust
let tools = (*ctx.require::<McpTools>(services::MCP_TOOLS)?).clone();
let handle = tools.register(
    ToolSpec {
        name: "my_feature_status".into(),
        title: "My feature status".into(),
        description: "Return current status as JSON.".into(),
        input_schema: serde_json::json!({
            "type": "object",
            "properties": {},
            "additionalProperties": false
        }),
        read_only: true,
    },
    |ctx, _args| {
        let service = ctx.require::<MyService>(services::MY)?;
        Ok(serde_json::json!({ "status": service.status() }))
    },
);
ctx.effect("unregister mcp tool", move || tools.unregister(handle));
```

Handlers run on the UI thread. Set `read_only: true` only if the operation has
no side effects. For resources, use `McpResources` and return
`ResourceContent` text/MIME.

The MCP server plugin exposes these to external clients. The in-app copilot
uses the same registry, so a good tool benefits both.

### 3.11 Optional services

These are available but not required for a new plugin:

- `ai`: `AiAssistant` snapshot/send/clear/pin context.
- `ftp`: `FtpService` snapshots/events (SFTP browser).
- `automation`: Rhai scripts with `send/expect/validate` (see `automation.rs`).
- `mcp-status`: endpoint/token/tool count for UI display.

If you inject one of these, keep it optional unless the feature genuinely cannot
work without it; otherwise injecting `ai` means your plugin stays `Pending` when
the AI plugin is disabled.

## 4. Creating a plugin from zero

### Step 1 — Decide the plugin shape

| Shape | Needs custom GPUI? | Approach |
|---|---|---|
| Background worker / service | No | provide a service + broadcast events; shell can be extended later |
| Command-palette action | No | `Commands` + `UiCommand::Notify` |
| Notification-only | No | `ui-host` |
| MCP tool/resource | No | `mcp-tools` / `mcp-resources` |
| Side panel / modal / settings page | Yes | add `UiCommand` variant + shell code |
| New transport | No (if workspace consumes it) | `Transports` + `Transport` |
| Terminal overlay | Yes | shell/terminal rendering changes |

The rest of this guide first covers the no-shell-change path, then the exact
shell integration for a custom panel.

### Step 2 — Add the API contract (if other plugins/shell need it)

For a private plugin you can keep types in `crates/plugins/src/my_plugin.rs`.
If the shell or another plugin consumes the service, add a module in
`crates/plugin-api/src/`:

```rust
// crates/plugin-api/src/my_feature.rs
use std::ops::Deref;
use std::rc::Rc;

pub struct MySnapshot {
    pub status: String,
    pub count: usize,
}

pub trait MyFeatureService {
    fn snapshot(&self) -> MySnapshot;
    fn refresh(&self);
}

pub struct MyFeature(pub Rc<dyn MyFeatureService>);

impl Deref for MyFeature {
    type Target = dyn MyFeatureService;
    fn deref(&self) -> &Self::Target { &*self.0 }
}
```

Export it from `crates/plugin-api/src/lib.rs`:

```rust
pub mod my_feature;
pub use my_feature::{MyFeature, MyFeatureService, MySnapshot};
```

Add a service name to `services`:

```rust
pub const MY_FEATURE: &str = "my-feature";
```

Do this only for genuinely shared contracts. A private plugin should not expand
the public API surface.

### Step 3 — Create the plugin module

Create `crates/plugins/src/my_feature.rs`:

```rust
use xcord_cordis::{Context, Plugin, PluginMeta, Result};
use xcord_plugin_api::{services, Commands, Command, I18n, Ui, UiCommand, NoticeLevel};
use std::rc::Rc;

#[derive(Clone)]
pub struct MyFeaturePlugin;

impl Plugin for MyFeaturePlugin {
    fn meta(&self) -> PluginMeta {
        PluginMeta::new("my-feature", "My Feature")
            .description("What it does")
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
            "my-feature.run",
            i18n.t("app", "My Feature"),
            "My Feature",
            move |_ctx| {
                ui.send(UiCommand::Notify {
                    level: NoticeLevel::Success,
                    message: "My feature is ready".into(),
                });
            },
        ));
        ctx.effect("unregister my-feature command", move || {
            commands.unregister(handle);
        });

        Ok(())
    }
}
```

### Step 4 — Register it

In `crates/plugins/src/lib.rs`:

```rust
mod my_feature;

pub fn install(ctx: &Context) {
    // ...
    ctx.plugin(my_feature::MyFeaturePlugin);
}
```

Registration order is not a hard dependency order: the kernel defers plugins
until injected services exist. Put core providers (`runtime`, `commands`,
`sessions`, etc.) first for clearer diagnostics.

### Step 5 — Build

```bash
cargo fmt --all
cargo check -p xcord-builtins
cargo test --workspace
cargo build -p xcord
```

Do not report success if `cargo check` fails. The app binary is
`target/debug/xcord.exe` (this workspace may use a shared target directory; see
`$env:CARGO_TARGET_DIR` or `cargo metadata`).

### Step 6 — Smoke test

- Start the app.
- Open the command palette and run your command.
- Check the notice appears.
- Open **Plugins** and confirm the plugin is `active`, not `pending`/`failed`.
- If failed, inspect `kernel.diagnostics()` via the plugin manager or logs.

## 5. Custom UI: side panel, modal, or settings page

The plugin API deliberately cannot create widgets. The GPUI shell owns all
pixels and input. To add a panel you will change both plugin and shell.

Read `docs/plugin-development/ui-integration.md` for the exact diff pattern.
The short version:

1. Add a semantic `UiCommand` variant (e.g. `ToggleMyFeature`).
2. Register a command that sends it.
3. Add a field to `XcordApp`.
4. Require the service in `XcordApp::new`.
5. Handle the variant in `handle_ui_command`.
6. Render a panel/modal from a snapshot.
7. Subscribe to the plugin's broadcast events and call `cx.notify()`.
8. Add an Escape/close path.
9. Add i18n keys and header/button entry.

Do not make the plugin reach into `XcordApp`. Keep the data flow:

```text
plugin service --snapshot()--> shell render
shell command --service method--> plugin
plugin worker --broadcast::Sender--> shell pump --cx.notify()--> repaint
```

### Important GPUI constraints in this app

- Overlay elements with `position: absolute` can paint but miss hit-testing.
  Prefer normal-flow modal shells and panels.
- Use the existing helpers in `root.rs`: `header_button`, `modal_shell`,
  `primary_button`, `secondary_button`, `small_button`, `field`, `TextInput`,
  `SelectableText`.
- Do not put blocking work in a render function.
- If a panel needs a scroll area, give the scroll container an `.id(...)` and
  use `.track_scroll(...)`/`.overflow_y_scroll()`.
- For selectable text/logs, create a `SelectableText` entity once and update its
  text; do not recreate it every frame.
- TextInput is single-line. For multi-line content use a dedicated editor
  component or a plain read-only selectable view.

## 6. Background work and UI bridging

### Pattern A — background state + broadcast (recommended)

```rust
use std::sync::{Arc, Mutex};
use tokio::sync::broadcast;

pub struct WorkerState {
    data: Arc<Mutex<WorkerData>>,
    events: broadcast::Sender<WorkerEvent>,
    runtime: RuntimeHandle,
}

#[derive(Clone, Debug)]
pub enum WorkerEvent { Changed }

impl WorkerState {
    pub fn start(&self) {
        let data = self.data.clone();
        let events = self.events.clone();
        self.runtime.spawn(async move {
            // do I/O
            {
                let mut data = data.lock().unwrap();
                // mutate plain Send data
            }
            let _ = events.send(WorkerEvent::Changed);
        });
    }
    pub fn snapshot(&self) -> WorkerSnapshot {
        let data = self.data.lock().unwrap();
        WorkerSnapshot { /* clone small fields */ }
    }
}
```

The shell subscribes with `state.subscribe()` and pumps:

```rust
let mut events = worker.subscribe();
cx.spawn(async move |this, cx| loop {
    match events.recv().await {
        Ok(_) => {
            if this.update(cx, |app, cx| {
                app.worker_snapshot = app.worker.snapshot();
                cx.notify();
            }).is_err() { break; }
        }
        Err(broadcast::error::RecvError::Lagged(_)) => { /* refresh snapshot */ }
        Err(broadcast::error::RecvError::Closed) => break,
    }
}).detach();
```

This avoids sending `Rc` across threads. Use `Arc<Mutex<_>>` for background data;
use `Rc<RefCell<_>>` only on the UI thread.

### Pattern B — PostOffice job

Use when the background task must call UI-thread services with a `&Context`.

```rust
runtime.post(move |ctx| {
    if let Ok(workspace) = ctx.require::<Workspace>(services::WORKSPACE) {
        workspace.write(tab, b"...");
    }
});
```

The closure is `Send`; it captures only Send data. `Context` is passed in on the
UI thread. Do not capture `Rc`, `RefCell`, or a `Context` itself.

### Pattern C — long-lived event listener

```rust
ctx.on::<SomeEvent>(move |event, ctx| {
    // UI thread; safe to use Rc services from ctx.
});
```

Subscriptions are automatically removed on unload.

### Cancellation

- Keep an `AbortHandle`/`JoinHandle` or a `tokio::sync::watch`/`CancellationToken`.
- Register a cleanup effect that aborts/stops the worker.
- Drop broadcast senders on unload so UI pumps exit.
- Never rely solely on `Drop` of something that may be leaked.

## 7. Persistence and configuration

### Plugin config

`apply` receives `serde_json::Value` from registration:

```rust
ctx.plugin_with(MyPlugin, serde_json::json!({
    "endpoint": "http://127.0.0.1:9000",
    "limit": 100
}));
```

The current `install()` uses `ctx.plugin`, so config is `Null` unless you change
the registration. There is no automatic per-plugin config file. If a plugin
needs persistent config, store it through `Sessions::settings()` (extend
`xcord_store::Settings`) or a dedicated file under `%APPDATA%\\xcord`.

### Secrets

Use `SessionService::password/passphrase/store_*` (OS keychain). Never write
secrets into TOML, logs, MCP responses, or global breadcrumbs.

### User data location

- Windows: `%APPDATA%\\xcord`
- Linux: `$XDG_CONFIG_HOME/xcord` or `~/.config/xcord`
- Config: `config.toml`
- MCP token: `mcp-token`
- Automation scripts: `scripts/*.rhai`

Use the sessions service rather than reimplementing path resolution where
possible.

## 8. i18n and theming

- Register a unique domain, e.g. `"my-feature"`.
- Keys are source English strings; add `zh-CN` translations.
- Unregister in an effect.
- Resolve strings at render time with `i18n.t(domain, "English")`.
- Use theme colors from `Themes::active()`; never hard-code dark-theme hex.
- Keep hit targets/row heights consistent with existing UI.

## 9. Tests

### Kernel/plugin unit tests

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use xcord_cordis::Kernel;

    #[test]
    fn plugin_becomes_active_after_dependency() {
        let kernel = Kernel::new();
        let ctx = kernel.context();
        ctx.plugin(MyPlugin);
        assert_eq!(kernel.active_plugins().len(), 0);
        ctx.provide("dep", 1u32).unwrap();
        assert_eq!(kernel.active_plugins().len(), 1);
    }
}
```

Useful assertions:

- plugin pending before dependency,
- active after dependency,
- failed apply produces `PluginState::Failed` with an error,
- effects run LIFO,
- unload cascades to dependents,
- command registration/unregistration.

### Service tests

Keep pure logic in plain Rust types independent of GPUI. Test parsing, state
transitions, snapshot building, and error paths directly. Mock service traits
with small structs implementing them.

### Build matrix

```bash
cargo fmt --all -- --check
cargo check --workspace
cargo test --workspace
cargo build -p xcord
```

For UI changes, manually verify:

- plugin manager shows `active`,
- command palette entry works,
- panel opens/closes with Escape,
- header button works,
- terminal remains interactive,
- i18n switches between English and Chinese,
- no layout clipping at the default 1280×820 window,
- no panic when the service is missing/unloaded.

## 10. Common failures

| Symptom | Cause | Fix |
|---|---|---|
| Plugin stuck `pending` | `inject` names a missing service | ensure provider plugin is registered/active; check exact service name |
| Plugin `failed` | `apply` returned Err or panicked | read `PluginInfo.error`/diagnostics; fix apply |
| `MissingService` | `require` for a service not injected or absent | add to `inject` and ensure provider; use `service()` for optional |
| `DuplicateService` | two plugins provide same name | rename your service or remove the other provider |
| `service is not of type ...` | provided concrete type vs wrapper type | provide/require the same wrapper |
| Command not in palette | commands service not injected or command id not registered | inject `commands`, register during apply, verify effect not removing it |
| Command handler does nothing | sent a new `UiCommand` the shell does not match | add match arm (exhaustive match will warn) |
| Panel does not repaint | background event not pumped / no `cx.notify()` | subscribe in shell and notify |
| App freezes | blocking I/O in `apply`/render/command | move to Runtime and post/broadcast |
| `Rc` is not Send | captured UI state in background | move only `Arc`/`Send` data; use PostOffice for UI context |
| UI click dead | absolute overlay hit-testing | use normal-flow modal/panel |
| Translation missing | wrong domain/key or unregistered | register pairs with unique domain; use same key |
| Hot reload does not change behavior | code changes are compiled, not dynamic | rebuild/restart app; kernel reload only restarts instance |

## 11. Security and performance

- Treat plugin code as trusted application code: it can call any service and
  access credentials.
- Validate all external input (paths, URLs, JSON) before acting.
- Bound memory: cap logs, buffers, queues, and concurrent jobs.
- Avoid unbounded `broadcast` payloads; send events, fetch snapshots.
- Don't log secrets or full terminal output by default.
- Use `read_only: true` on MCP tools only when true.
- Clean up workers and listeners on unload.
- Keep `apply` fast; lazy-start expensive work when first used.

## 11.5 AI skill

The repository ships a project skill for coding agents:

```text
.agents/skills/xcord-plugin-development/SKILL.md
```

Load it by name (`xcord-plugin-development`) before plugin work. It tells the
agent to read this guide, classify the feature, follow the service/UI patterns,
and run the build gate.

Skill discovery is workspace-root based. If the DSH workspace root is the parent
of this repository (for example `default-workspace/` containing `xcord/`),
either open `xcord/` as the workspace or copy
`.agents/skills/xcord-plugin-development/` to
`<workspace-root>/.agents/skills/`. Restart the session after installing a new
skill so it appears in the skill catalog.

Also keep `AGENTS.md` at the repository root; it is a short pointer to this
guide and to the hard rules.

## 12. Definition of done

A plugin is done only when all of these are true:

- [ ] `cargo fmt --all` clean
- [ ] `cargo check --workspace` passes
- [ ] `cargo test --workspace` passes (including new tests)
- [ ] `cargo build -p xcord` passes
- [ ] plugin appears `active` in plugin manager
- [ ] command/palette or header entry works
- [ ] close/Escape path works if it has UI
- [ ] i18n keys exist for English and Chinese
- [ ] no blocking work on the UI thread
- [ ] all listeners/commands/translations/tasks are cleaned up by effects
- [ ] docs/README updated for user-visible behavior
- [ ] no secrets in logs or config

For a copy-paste skeleton and a complete panel integration example, read:

- `docs/plugin-development/templates.md`
- `docs/plugin-development/ui-integration.md`
- `docs/plugin-development/service-reference.md`
- `docs/plugin-development/checklist.md`
