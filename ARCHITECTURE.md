# Xcord architecture

This document explains how Xcord applies Cordis' "everything is a plugin" idea
to a native SSH client, and how data moves between the GPUI UI thread and the
Tokio I/O world.

## 1. Principles

1. **The shell is replaceable.** `apps/xcord` owns pixels and input only. It
   provides `ui-host` and consumes service traits. A CLI or web shell could
   provide the same `ui-host` and reuse every plugin.
2. **Features are plugins.** SSH, the terminal, sessions, themes, highlighting,
   logging and commands are all plugins. None are compiled into the kernel.
3. **Resources are effects.** A plugin never needs a `Drop`/dispose path of its
   own: every listener, task, file and service registration is an effect that
   runs (LIFO) when the plugin unloads.
4. **Dependencies are declarative.** A plugin names the services it needs; the
   kernel starts it when they exist and unloads it when they disappear.
5. **I/O is isolated.** Nothing blocks the UI thread. Tokio workers talk back
   through a post-office of `Send` closures executed on the UI thread.
6. **The workspace is portable.** Configuration is plain TOML; secrets are in
   the OS keychain; no proprietary database.

## 2. Cordis → Rust

Cordis (the Koishi meta-framework) gives JavaScript plugins a context, service
container, events, effects, and dependency-aware fibers. Xcord implements the
same semantics with Rust's type system in `crates/cordis`.

| Cordis | Xcord |
|---|---|
| `ctx.plugin(plugin, config)` | `ctx.plugin(plugin)` / `ctx.plugin_with(plugin, config)` |
| `ctx.provide(name, value)` / `ctx.get(name)` | typed `provide` / `require::<T>(name)` |
| `inject: ['database']` | `fn inject(&self) -> &'static [&'static str]` |
| `ctx.on('event', cb)` | typed `ctx.on::<E>(cb)` |
| `ctx.emit(...)` | `ctx.emit(&event)` |
| `ctx.effect(dispose)` | `ctx.effect(label, dispose)` |
| fiber states | `PluginState::{Pending, Loading, Active, Failed, Unloading, Disposed}` |
| service change → relink fibers | pending wake-up + cascade unload |
| hot reload | `Kernel::reload(id)` / `reload_all()` |

### 2.1 Scopes and effects

Every plugin gets a `Context` with its own `Scope`. Effects are closures stored
on that scope. Unloading runs them in reverse registration order, catching
panics so one bad cleanup cannot stop the rest:

```rust
impl Plugin for Logger {
    fn meta(&self) -> PluginMeta { PluginMeta::new("logger", "Logger") }

    fn apply(&self, ctx: Context, _config: Config) -> Result<()> {
        let file = open_log()?;
        ctx.effect("close log", move || drop(file));
        ctx.on::<Output>(|event, _ctx| write(event));
        Ok(())
    }
}
```

`ctx.on` automatically registers an effect that unsubscribes. `ctx.provide`
records the owner plugin, so unloading removes only the services that plugin
owns.

### 2.2 Dependency graph

`KernelInner` keeps service owners and plugin `inject` lists. Two operations are
central:

- **Wake:** after a service is provided, pending plugins whose dependencies are
  now satisfied are started. A re-entrancy guard batches nested starts.
- **Cascade:** before a plugin unloads, every plugin that injects a service it
  owns is unloaded first (recursively). This mirrors Cordis fibers and prevents
  dangling consumers.

Hot reload computes the transitive dependent closure, unloads dependents first,
then restarts everything in registration order with the same config. A fresh
plugin instance is built from its factory, so stateful plugins do not leak state
across reloads.

### 2.3 Events

Events are keyed by Rust type (`TypeId`), giving compile-time checked payloads
without a global string registry:

```rust
#[derive(Clone)]
struct Output { tab: TerminalId, bytes: Bytes }

ctx.emit(&Output { tab, bytes });
ctx.on::<Output>(|event, _ctx| { /* ... */ });
```

The bus snapshots listeners before dispatch, so handlers may subscribe/unsubscribe
during emission.

### 2.4 Failure handling

`apply` runs inside `catch_unwind`. On error or panic:

1. the state becomes `Failed` with the error text,
2. the plugin scope is disposed (LIFO),
3. services it owned are removed,
4. the kernel stays alive and records a diagnostic.

The plugin manager surfaces these diagnostics. A failed plugin does not take
down the application.

## 3. Threading model

GPUI is single-threaded and `!Send`; Tokio is multi-threaded and `Send`. Xcord
bridges them explicitly.

```
┌──────────────────────────── UI thread (GPUI, kernel, Context) ───────────────────────────┐
│  XcordApp                                                                                │
│    ├── RuntimeService  (owns tokio::Runtime, async_channel<Job> receiver)                │
│    ├── pump task: recv Job → execute(&Context) → notify                                   │
│    ├── workspace event pump: broadcast::Receiver<WorkspaceEvent> → notify                  │
│    └── ui command pump: async_channel<UiCommand> → dialogs/palette                        │
└──────────────────────────────────────────────────────────────────────────────────────────┘
                 ▲ Job: Box<dyn FnOnce(&Context) + Send>        │ PostOffice.post(closure)
                 │                                              ▼
┌──────────────────────────── Tokio workers (russh, files) ────────────────────────────────┐
│  SshProvider → SshTransport task (read/write/resize) → async_channel<TransportEvent>      │
│  forwarder task → PostOffice.post(|ctx| ctx.service::<Rc<WorkspaceState>>()...)           │
└──────────────────────────────────────────────────────────────────────────────────────────┘
```

A `Job` is `Send` but takes `&Context` only when executed on the UI thread. It
therefore never captures `Rc`/`RefCell` state from a worker. The workspace is
found by service name inside the closure.

`TransportEvent::Connected` owns the `Box<dyn Transport>`; the UI takes ownership
of the handle and writes to it through a Tokio `mpsc` channel. No future is ever
awaited on the UI thread.

The GPUI job pump has one subtle requirement: the job that wakes `recv()` must
also be executed. `RuntimeService::receiver()` clones compete for messages, so
the pump executes the received job before draining `try_next_job()`.

## 4. Service contracts

`crates/plugin-api` defines data types and traits; wrappers exist because Rust's
`Any` cannot downcast to unsized trait objects.

```rust
pub struct Sessions(pub Rc<dyn SessionService>);
pub struct Workspace(pub Rc<dyn WorkspaceService>);
```

Plugins provide wrappers (`Sessions(...)`), consumers require them
(`ctx.require::<Sessions>("sessions")`), and the shell can ignore the concrete
implementation.

Notable contracts:

- `SessionService` — profile CRUD, settings, snippets, highlight rules, keychain
  password/passphrase access, export/import.
- `TransportProvider` — `connect(spec, events) -> Result<()>`; must return
  immediately and report progress through the channel.
- `Transport` — `Send` handle with `write`, `resize`, `close`.
- `WorkspaceService` — tabs, open/close/reconnect, write/resize, snapshot,
  scroll, selection text, broadcast events.
- `UiHost` — semantic commands (`NewSession`, `OpenPalette`, `Notify`, ...).
  Plugins never construct widgets.
- `CommandRegistry` — command handlers receive `&Context`; registration effects
  remove them on unload.
- `HighlightService` — byte spans with colors, queried by the renderer per row.

## 5. Session-open sequence

```
UI: click session
  └─ SessionService.password()/passphrase()  (OS keychain)
  └─ Workspace::open(OpenRequest)
       ├─ build ConnectSpec (auth resolved in memory)
       ├─ create Terminal + tab, status Connecting
       ├─ spawn forwarder task (async_channel -> PostOffice)
       └─ TransportRegistry.get("ssh").connect(spec, events)
             └─ ssh plugin spawns russh connect on Tokio
                   ├─ TOFU/strict host-key check
                   ├─ auth (password or public key)
                   ├─ request PTY + shell
                   ├─ send TransportEvent::Connected { transport, info }
                   └─ run_session: mux ChannelMsg <-> write/resize controls
UI: Job(Connected) -> tab.transport = Some, status Connected, host key shown
UI: Job(Output)    -> Terminal::feed, emit TerminalOutputEvent (logging),
                      broadcast Dirty(tab), repaint
UI: keystroke      -> encode xterm bytes -> Transport::write -> SSH channel
```

Closing a tab drops `Box<dyn Transport>`; `SshTransport::drop` closes the channel
and aborts the session task. The workspace emits `Closed`, and the tab is
removed.

## 6. Terminal

`crates/vt` is UI-independent:

- `vte::Parser` drives a `Terminal` implementing `Perform`.
- Screen + `VecDeque` scrollback; alternate screen; scroll regions; tabs.
- Cells carry `Attrs` (fg/bg/flags) and width; wide characters reserve a tail
  cell; combining marks attach to the previous cell.
- `snapshot(scroll_offset)` produces viewport rows with text for highlighting.
- Responses (DSR, DA) and OSC 52 clipboard payloads are queued for the transport
  / system clipboard.
- The emulator deliberately does not reflow on resize yet.

Rendering in `terminal_view.rs`:

1. measure `MMMM` to derive cell width; compute grid `cols × rows`;
2. snapshot the terminal, resize the PTY if the grid changed;
3. build one `ShapedLine` per row from styled cell runs, applying highlight
   spans and ANSI attributes;
4. paint background runs, selection quads, text, cursor and scrollbar in a GPUI
   `canvas`, offset by the canvas bounds.

**GPUI gotcha discovered during development:** passing a `force_width` to
`TextSystem::shape_line` produced glyph positions scaled by hundreds for longer
rows, so only the first glyph was visible. Xcord shapes without `force_width`
and clips via the canvas. Keep this in mind if you refactor the renderer.

## 7. Writing a plugin

A plugin crate only needs `xcord-cordis` and `xcord-plugin-api`.

```rust
use xcord_cordis::{Context, Plugin, PluginMeta, Result, Config};
use xcord_plugin_api::{services, Sessions, Ui, UiCommand};

#[derive(Clone)]
struct NotesPlugin;

impl Plugin for NotesPlugin {
    fn meta(&self) -> PluginMeta {
        PluginMeta::new("notes", "Session Notes")
            .description("Attach notes to the active session")
            .capabilities(&["ui"])
    }

    fn inject(&self) -> &'static [&'static str] {
        &[services::SESSIONS, services::UI_HOST]
    }

    fn apply(&self, ctx: Context, _config: Config) -> Result<()> {
        let sessions = ctx.require::<Sessions>(services::SESSIONS)?;
        let ui = ctx.require::<Ui>(services::UI_HOST)?;

        // Register a command; it disappears automatically on unload.
        if let Some(commands) = ctx.service::<xcord_plugin_api::Commands>(services::COMMANDS) {
            let handle = commands.register(xcord_plugin_api::Command::new(
                "notes.show", "Show session notes", "Notes",
                move |_ctx| {
                    ui.send(UiCommand::Notify {
                        level: xcord_plugin_api::NoticeLevel::Info,
                        message: format!("{} sessions", sessions.list().len()),
                    });
                },
            ));
            ctx.effect("unregister notes command", move || commands.unregister(handle));
        }
        Ok(())
    }
}
```

Register it in `xcord_builtins::install` (or from your own binary) with
`ctx.plugin(NotesPlugin)`. To replace a built-in, do not register it and provide
the same service name from your plugin. To extend the shell, contribute a
`Command` or send a new `UiCommand`; no GPUI dependency is required.

## 8. Testing strategy

- **Kernel:** deterministic single-threaded tests for ordering, cascade unload,
  effects, failures, events and reload.
- **Terminal:** feed byte streams and assert snapshots; no window needed.
- **Store:** temp-directory round-trips and corruption handling.
- **Plugins:** highlighter unit tests; the I/O bridge is exercised manually with
  a real SSH server and by the `xcord-ssh` probe.
- **UI:** verified with live sessions and `PrintWindow` captures. A future step
  is GPUI's test context for pixel/behavior tests.

## 9. Trade-offs and future work

- The kernel is single-threaded by design. A multi-window shell would need one
  kernel per UI thread, or a `Send` variant.
- The terminal avoids GPUI's `force_width` shaping (see §6). Reflow on resize is
  not implemented.
- IME for the terminal grid is not wired yet; dialog and compose inputs use the
  full `EntityInputHandler` path.
- SSH agent auth, jump hosts, SFTP and port forwarding are plugin-sized next
  steps; `Transport` and `SessionProfile` already reserve space for them.
- Hot reload is exposed for the highlighter. Reloading core plugins is supported
  by the kernel but would close active sessions, so the UI intentionally limits
  it.
