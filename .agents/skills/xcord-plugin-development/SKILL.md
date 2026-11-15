---
name: xcord-plugin-development
description: Develop, extend, test, and debug Xcord (Rust + GPUI) plugins, services, commands, MCP tools, and plugin-backed UI. Use whenever a task touches crates/cordis, crates/plugin-api, crates/plugins, or a new Xcord feature.
---

# Xcord plugin development

Use the repository's concrete contracts. Do not copy generic Cordis/JS patterns:
Xcord is Rust, statically linked, and the GPUI shell owns all widgets.

Locate the repository root by finding the `Cargo.toml` that contains the
`xcord`/`xcord-builtins` workspace members. In the default DSH workspace it is
usually `xcord/`. Resolve all `docs/...`, `crates/...`, and `apps/...` paths from
that root, and run build commands there.

## Read before coding

Read these in order, completely:

1. `docs/plugin-development.md` — kernel model, lifecycle, services, threading,
   tests, failure modes.
2. `docs/plugin-development/service-reference.md` — exact service APIs.
3. `docs/plugin-development/ui-integration.md` — only if the feature has a
   panel, modal, settings page, header entry, or command.
4. `docs/plugin-development/templates.md` — copy the closest skeleton.
5. `docs/plugin-development/checklist.md` — final gate.

Also inspect the closest working implementation under `crates/plugins/src/`
and the shell integration in `apps/xcord/src/root.rs`. Do not invent methods;
if an API is absent, add the contract deliberately.

## Non-negotiables

- Plugin code is Rust in `crates/plugins/src/<name>.rs`, registered from
  `crates/plugins/src/lib.rs::install`.
- There is no dynamic `.dll`/`.so` loading. "Hot reload" restarts the compiled
  instance; code changes need rebuild/restart.
- `Plugin::apply` runs on the UI thread. Never block it or a render/command
  handler.
- Background work uses `RuntimeHandle`; cross-thread data uses `Arc`/channels.
  Never capture `Rc`/`RefCell` in a `Send` future or `PostOffice` job.
- Plugins send `UiCommand`; they do not construct GPUI widgets or access
  `XcordApp`.
- Clean up every command, translation, MCP tool/resource, listener, task, and
  channel with `ctx.effect` (or helpers that auto-effect).
- Keep `inject()` exactly in sync with `ctx.require` calls.
- Use the platform vault for secrets; never write them to TOML/logs/MCP.
- Do not claim success until the build/test gate passes.

## Workflow

### 1. Classify the feature

| Feature | Shape |
|---|---|
| Palette action / notification | command + `UiCommand::Notify`; no shell UI |
| Headless service | contract in `plugin-api`, provider plugin |
| Background I/O | provider + runtime + snapshot + broadcast |
| MCP capability | register `McpTools`/`McpResources` |
| Side panel/modal/settings | command + `UiCommand` variant + root integration |
| New protocol | `TransportProvider` + `Transport`, workspace consumes |

Choose the smallest shape. Do not add a UI surface just to prove the plugin
works.

### 2. Define contracts

- Private plugin: keep types in the plugin module.
- Shared with shell/plugins: add `crates/plugin-api/src/<name>.rs`, export from
  `plugin-api/src/lib.rs`, add `services::<NAME>` only if globally shared.
- Service wrapper pattern:
  ```rust
  pub struct X(pub Rc<dyn XService>);
  impl Deref for X { type Target = dyn XService; ... }
  ```
- Snapshots are small clones. Worker state is not public.

### 3. Implement the plugin

- `meta()`: stable dotted id, human name, description, author, capabilities.
- `inject()`: only services actually needed.
- `apply()`:
  1. `ctx.require` services and clone wrappers,
  2. construct state,
  3. `ctx.provide` (if any),
  4. register commands/translations/MCP capabilities,
  5. register effects for cleanup,
  6. start background work only after state is fully wired.
- Return descriptive `Error::PluginFailed` or `?`; do not panic.
- Use `ctx.effect` for command unregister, i18n unregister, worker cancellation,
  and senders/closers.

### 4. Wire the shell only if needed

Read `ui-integration.md`. Minimal side-panel sequence:

1. Add `UiCommand::ToggleX`.
2. Register a command that sends it.
3. Add service field + snapshot fields to `XcordApp`.
4. Require service in `XcordApp::new` (or optional).
5. Match variant in `handle_ui_command`.
6. Add body child (panel) or content branch (modal).
7. Add close/Escape path.
8. Pump broadcast events into snapshots and `cx.notify()`.
9. Add header/command entry and i18n keys.

Do not use absolute overlays for interactive UI; this GPUI version can paint
them but miss hit-testing. Use `modal_shell` and normal-flow panels.

### 5. Test

Add unit tests for pure logic. For kernel behavior, use `Kernel::new()` and
`ctx.provide` mocks. For UI changes, manually smoke test:

- plugin manager state = active,
- command palette entry,
- panel/modal open/close,
- Escape behavior,
- terminal still interactive,
- locale switch,
- no clipping at 1280×820.

### 6. Build gate

Run, in this order, from the repository root:

```bash
cargo fmt --all
cargo check --workspace
cargo test --workspace
cargo build -p xcord
```

Fix warnings in new code. If a command fails, read the full error; do not hide it
with `unwrap`/`allow(dead_code)` unless the code is genuinely test-only.

## Required final report

Include:

- plugin id and what it provides,
- files added/changed,
- services/commands/MCP tools exposed,
- UI entry points,
- threading/cleanup model,
- tests and exact build results,
- any remaining limitation.

## Common traps

- Forgot `inject` → plugin stays pending.
- `ctx.require` wrong wrapper type → service missing/mismatch.
- Blocking I/O in `apply`/render → app freeze.
- `Rc` in a Tokio task → compile error; use `Arc` or PostOffice.
- Event emitted but no shell pump/`cx.notify` → UI never updates.
- Registered command but no effect → duplicate after reload.
- New `UiCommand` variant but missing match arm → compile error (good).
- Absolute overlay → buttons unclickable.
- `RefCell` borrow held across a callback → panic/deadlock.
- Stored secret in config/log → security bug.
- Claimed done without `cargo test`/build → unacceptable.

## Reference implementations

- `crates/plugins/src/runtime_plugin.rs` — provider/effect.
- `crates/plugins/src/commands_plugin.rs` — tiny service.
- `crates/plugins/src/ftp.rs` — async service + snapshots + broadcast + UI.
- `crates/plugins/src/automation.rs` — worker + UI pump + service + Rhai.
- `crates/plugins/src/mcp.rs` — MCP tool/resource registration.
- `apps/xcord/src/root.rs` — service consumption and all UI integration patterns.
