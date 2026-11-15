# Service reference

All contracts are in `crates/plugin-api`. Every service is fetched from a
`Context` by name and wrapper type. Wrappers are cheap `Clone` values; clone the
inner trait object to keep it beyond the current scope:

```rust
let workspace = (*ctx.require::<Workspace>(services::WORKSPACE)?).clone();
```

Service names are constants in `crates/plugin-api/src/lib.rs::services`.

| Name | Wrapper | Provided by | Thread |
|---|---|---|---|
| `runtime` | `Runtime` | runtime plugin | UI-thread facade; handle Send |
| `transports` | `Transports` | transports plugin | UI |
| `sessions` | `Sessions` | sessions plugin | UI |
| `workspace` | `Workspace` | workspace plugin | UI |
| `commands` | `Commands` | commands plugin | UI |
| `themes` | `Themes` | themes plugin | UI |
| `highlights` | `Highlights` | highlights plugin | UI |
| `ui-host` | `Ui` | GPUI shell | UI |
| `i18n` | `I18n` | i18n plugin | UI |
| `mcp-tools` | `McpTools` | mcp plugin | UI handlers |
| `mcp-resources` | `McpResources` | mcp plugin | UI readers |
| `mcp-status` | `McpStatus` | mcp plugin | UI |
| `ai` | `Ai` | AI copilot plugin | UI |
| `ftp` | `Ftp` | SFTP plugin | UI facade |
| `automation` | `Automation` | automation plugin | UI facade |

## Kernel and `Context`

See `docs/plugin-development.md` §2. Summary:

```rust
ctx.provide(name, wrapper)?;
ctx.provide_rc(name, rc)?;
ctx.require::<T>(name)?;          // Rc<T>
ctx.service::<T>(name);           // Option<Rc<T>>
ctx.has_service(name);
ctx.service_names();
ctx.effect(label, dispose);
ctx.on::<E>(handler);
ctx.emit(&event);
ctx.plugin(Child);
ctx.plugin_with(Child, json!({...}));
```

## `Runtime` / `RuntimeHandle`

File: `crates/plugin-api/src/runtime.rs`.

```rust
pub struct RuntimeHandle {
    pub handle: tokio::runtime::Handle,
    pub office: PostOffice,
}

impl RuntimeHandle {
    pub fn spawn<F>(&self, future: F) where F: Future<Output = ()> + Send + 'static;
    pub fn sleep(&self, duration: Duration) -> tokio::time::Sleep;
    pub fn post(&self, job: impl FnOnce(&Context) + Send + 'static) -> bool;
}
```

`Runtime` derefs to `RuntimeService`:

- `handle() -> RuntimeHandle`
- `receiver() -> async_channel::Receiver<Job>`
- `try_next_job()`
- `execute(job)`
- `spawn_blocking(task)`
- `shutdown()`

`PostOffice`:

- `post(job) -> bool`
- `is_open()`

Thread rules:

- `RuntimeHandle` is `Send`; keep it in background closures.
- `RuntimeHandle::post` closure runs on UI and receives `&Context`.
- `Job = Box<dyn FnOnce(&Context) + Send + 'static>`.
- Never capture `Rc`/`RefCell` in a job. Capture `Arc<Mutex<_>>`, channels,
  Strings, and plain data.
- `spawn` futures must be `Send`.
- The shell already drains the job receiver. If your plugin only emits
  broadcasts, the shell still needs a pump; see UI integration.

## `Workspace` / `WorkspaceService`

File: `crates/plugin-api/src/workspace.rs`.

Data:

- `TerminalId(pub u64)` — stable tab/target id.
- `TabInfo` — id, title, profile id/name, host, user, port, `TabStatus`, host key, bell.
- `TabStatus::{Connecting, Connected, Closed, Failed(String)}`.
- `OpenRequest { profile, credential, cols, rows }`.
- `ConnectionTest { id, ok, message }`.
- `Snapshot` from `xcord_vt` with viewport rows, cursor, modes, scroll state.

Methods:

| Method | Notes |
|---|---|
| `tabs() -> Vec<TabInfo>` | cheap view models |
| `active() -> Option<TerminalId>` | currently selected tab |
| `select(id)` | UI state |
| `select_next/select_previous()` | cycles |
| `move_tab(id, target, after)` | reorder |
| `open(OpenRequest) -> Result<TerminalId, String>` | starts connection |
| `close(id)` | closes and drops transport |
| `reconnect(id) -> Result<TerminalId, String>` | |
| `test_connection(OpenRequest) -> Result<u64, String>` | result via event |
| `write(id, &[u8])` | raw terminal input |
| `resize(id, cols, rows)` | |
| `begin_output_capture(id)` | one-shot raw-output capture |
| `take_output_capture(id) -> Option<String>` | clears capture |
| `zmodem_upload(id, paths)` | remote `rz` waiting |
| `zmodem_cancel(id)` | |
| `snapshot(id) -> Option<Snapshot>` | viewport |
| `history_text(id) -> Option<String>` | scrollback + screen |
| `dimensions(id) -> Option<(u16,u16)>` | |
| `scroll_by(id, delta)` / `scroll_to(id, offset)` / `scroll_to_bottom(id)` | |
| `text_in_viewport(id, start, end)` | selection text |
| `terminal_text(id, max_lines)` | recent plain text |
| `subscribe() -> broadcast::Receiver<WorkspaceEvent>` | |

Events (`WorkspaceEvent`):

- `TabsChanged`
- `ActiveChanged(Option<TerminalId>)`
- `Dirty(TerminalId)` — repaint terminal; often used by automation/observers
- `StatusChanged(TerminalId)`
- `Bell(TerminalId)`
- `Closed(TerminalId)`
- `TestFinished(ConnectionTest)`
- `CommandSubmitted(TerminalId)`
- `FileSaved { terminal, path }`
- `ZmodemUploadRequested { terminal }`
- `Notice { level, message }`

Do not parse raw output for ordinary terminal text; use `snapshot`,
`history_text`, or `terminal_text`. `Dirty` means "some state changed"; fetch a
snapshot rather than caching bytes.

## `Sessions` / `SessionService`

File: `crates/plugin-api/src/session.rs`.

Profiles:

- `list() -> Vec<SessionProfile>`
- `get(id) -> Option<SessionProfile>`
- `save(profile) -> Result<(), String>`
- `delete(id)`
- `groups()`, `create_group`, `rename_group`, `move_group`, `move_session`

Snippets/highlights/settings:

- `snippets()`, `save_snippet`, `delete_snippet`
- `highlights()`, `save_highlight`, `delete_highlight`
- `settings() -> Settings`, `save_settings(Settings)`

Vault:

- `password(&profile) -> Option<String>`
- `store_password`, `clear_password`
- `passphrase`, `store_passphrase`, `clear_passphrase`
- `ai_api_key`, `store_ai_api_key`, `clear_ai_api_key`

Import/export:

- `export(path)`, `import(path)`

`SessionProfile` and `Settings` come from `xcord-store`. Extend `Settings` with
`#[serde(default)]` when adding backward-compatible fields, and update its
`Default`.

## `Commands` / `CommandRegistry`

File: `crates/plugin-api/src/commands.rs`.

```rust
pub struct Command {
    pub id: &'static str,
    pub title: String,
    pub category: &'static str,
    pub shortcut: Option<&'static str>,
    pub handler: Rc<dyn Fn(&Context)>,
}

commands.register(Command::new("id", "Title", "Category", handler));
commands.unregister(handle);
commands.list();
commands.get("id");
commands.run("id", &ctx);
```

Rules:

- `id` must be stable and globally unique (`my-feature.action`).
- Handler runs on the UI thread; no blocking.
- Register in `apply`, unregister in an effect.
- The command palette and MCP `run_command` see registered commands.
- `shortcut` is display metadata; actual key handling is shell-owned unless a
  global shortcut is added there.

## `Ui` / `UiHost`

File: `crates/plugin-api/src/ui.rs`.

```rust
pub trait UiHost: 'static {
    fn send(&self, command: UiCommand);
}
```

`UiCommand` variants currently include session/palette/plugins/settings/focus,
notice, tab actions, copy/paste, AI, FTP, automation. Adding a variant requires
a `root.rs` match arm (exhaustive). `NoticeLevel` is
`Info/Warning/Error/Success`.

`UiCommand::Notify` is the safe way to show a toast. A plugin must not access
`XcordApp` or GPUI.

## `I18n` / `LocalizationService`

File: `crates/plugin-api/src/i18n.rs`.

```rust
i18n.register_pairs(domain, "zh-CN", &[("Key", "译文")]) -> handle;
i18n.unregister(handle);
i18n.t(domain, "Key");
i18n.t_or(domain, "Key", "Fallback");
i18n.t_args(domain, "Key", &[("name", value)]);
i18n.current_locale(); // via locale()
```

- Use a unique domain per plugin.
- Source English key is the lookup key.
- Register Chinese for the default `zh-CN` UI.
- Unregister in an effect.
- The shell renders labels at render time, so switching locale is immediate.

## `Themes` / `ThemeService`

File: `crates/plugin-api/src/theme.rs`.

```rust
let themes = ctx.require::<Themes>(services::THEMES)?;
let theme = themes.active();
theme.background/foreground/panel/panel_alt/border/accent/muted/
theme.danger/success/cursor/selection/palette;
themes.themes();
themes.set_active("id");
themes.cycle();
```

Use colors from the active theme; never hard-code a theme-specific hex value.
`Theme::resolve(Color)` maps terminal colors.

## `Highlights` / `HighlightService`

File: `crates/plugin-api/src/highlight.rs`.

```rust
let spans = highlights.spans(text);
let enabled = highlights.enabled();
let count = highlights.rule_count();
```

`HighlightSpan { start, end, foreground, background }` uses byte offsets in a
rendered row. Rules are persisted by the sessions service; add rules through
`Sessions::save_highlight`.

## `Transports` / `TransportProvider`

File: `crates/plugin-api/src/transport.rs`.

```rust
pub trait TransportProvider: 'static {
    fn kind(&self) -> &'static str;
    fn connect(
        &self,
        spec: ConnectSpec,
        events: async_channel::Sender<TransportEvent>,
    ) -> Result<(), String>;
}
```

`Transport` is `Send + 'static` and has `write`, `resize`, `close`,
`description`. `TransportEvent`:

- `Connected { transport: Box<dyn Transport>, info: ConnectionInfo }`
- `Output(Bytes)`
- `Error(String)`
- `Closed { reason, exit_status }`

`ConnectSpec` includes host/port/user, `AuthRequest`, term, command, timeouts,
keepalive, host-key policy, known-hosts path, cols/rows.

Registration:

```rust
let transports = (*ctx.require::<Transports>(services::TRANSPORTS)?).clone();
transports.register(Rc::new(MyProvider))?;
ctx.effect("unregister provider", move || transports.unregister("my-kind"));
```

`connect` must return quickly; spawn I/O on `Runtime`. The workspace plugin owns
tab lifecycle and consumes the events.

## `McpTools` / `McpResources`

File: `crates/plugin-api/src/mcp.rs`.

```rust
pub struct ToolSpec {
    pub name: String,
    pub title: String,
    pub description: String,
    pub input_schema: serde_json::Value,
    pub read_only: bool,
}
```

```rust
tools.register(spec, |ctx, args| -> Result<Value, String> { ... }) -> handle;
tools.unregister(handle);
tools.list();
tools.is_read_only(name);
tools.call(ctx, name, args);
```

Resources:

```rust
ResourceSpec { uri, name, description, mime_type }
ResourceContent { uri, mime_type, text }
resources.register(spec, |ctx, uri| Ok(vec![...])) -> handle;
```

- Tool/resource handlers run on the UI thread.
- `read_only: true` means no side effects; the copilot can call it without the
  write toggle.
- Register with an effect and unregister on unload.
- Return JSON-serializable values; errors are user-visible strings.

## `Ai` / `AiAssistant`

File: `crates/plugin-api/src/ai.rs`.

```rust
ai.snapshot() -> AiSnapshot;
ai.send(prompt) -> Result<(), String>;
ai.clear();
ai.refresh_key();
ai.set_context_terminal(Option<TerminalId>);
ai.context_terminal();
```

`AiSnapshot` includes messages, streaming, status, error, model, base URL, key
presence, write toggle, terminal context, pinned terminal/title.

## `Ftp` / `FtpService`

File: `crates/plugin-api/src/ftp.rs`.

```rust
ftp.connect(profile_id)?;
ftp.disconnect(profile_id);
ftp.navigate_remote(profile_id, path);
ftp.navigate_local(path);
ftp.refresh(profile_id);
ftp.upload(profile_id, local_paths)?;
ftp.download(profile_id, remote_paths)?;
ftp.create_dir(profile_id, name)?;
ftp.delete_remote(profile_id, paths)?;
ftp.snapshot(profile_id) -> Option<FtpSnapshot>;
ftp.subscribe() -> broadcast::Receiver<FtpEvent>;
```

`FtpSnapshot` contains profile/host/connected/loading/busy/error, remote/local
paths and entries. `FtpEvent::Changed`/`Notice`.

## `Automation` / `AutomationService`

File: `crates/plugin-api/src/automation.rs`.

```rust
automation.scripts() -> Vec<ScriptInfo>;
automation.script_source(id) -> Option<String>;
automation.validate(source) -> Option<ScriptDiagnostic>;
automation.create_script(name) -> Result<ScriptInfo, String>;
automation.scripts_dir() -> String;
automation.run(script_id, terminal) -> Result<ScriptRunSnapshot, String>;
automation.stop(run_id);
automation.pump();                    // UI-thread command queue
automation.update_terminal(term, text);
automation.active_run() -> Option<ScriptRunSnapshot>;
automation.run_snapshot(run_id);
automation.subscribe() -> broadcast::Receiver<AutomationEvent>;
```

This is also a reference for a service that has a background worker and a UI
pump. Do not call `pump` from a background thread.

## Service design checklist

- [ ] Add trait + wrapper in `plugin-api`, or keep private if no external consumer.
- [ ] Add unique `services::NAME`.
- [ ] Include `#[derive(Clone)]` on wrapper.
- [ ] Use `Rc<dyn Trait>` for UI-thread services.
- [ ] Use `Arc<Mutex<_>>` for worker data.
- [ ] Provide small snapshots instead of exposing internal state.
- [ ] Broadcast a `Changed` event after mutations.
- [ ] Handle `Lagged` at consumers.
- [ ] Return `Result<_, String>` at API boundaries; don't panic.
