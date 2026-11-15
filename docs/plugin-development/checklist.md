# Plugin development checklist

Use this as the final gate. Do not skip compile/test steps.

## A. Decide the surface

- [ ] Is this a service/command/background feature with no custom UI?
- [ ] Or does it need a side panel/modal/settings page?
- [ ] Does it need a new transport protocol?
- [ ] Does it need an MCP tool/resource?
- [ ] Does it need user-visible i18n strings?

If custom UI, read `ui-integration.md` before editing `root.rs`.

## B. Contract

- [ ] Service name is unique and added to `services` (only if shared).
- [ ] Trait + wrapper (`pub struct X(pub Rc<dyn XService>)`) added in
      `plugin-api` and exported.
- [ ] Snapshot types are small cloned view models.
- [ ] Worker data uses `Arc<Mutex<_>>`, not `Rc<RefCell<_>>`.
- [ ] Errors are `Result<_, String>` at API boundaries.
- [ ] No GPUI types in `plugin-api`.

## C. Plugin

- [ ] `PluginMeta` id/name/version/description/author/capabilities are set.
- [ ] `inject()` lists every `ctx.require` service.
- [ ] `apply()` returns errors instead of panicking.
- [ ] Services provided before commands/UI references to them.
- [ ] Commands registered with effect cleanup.
- [ ] i18n registered with effect cleanup.
- [ ] MCP tools/resources registered with effect cleanup.
- [ ] Background tasks have a stop/cancel path.
- [ ] Broadcast senders are not leaked after unload.
- [ ] No blocking I/O in `apply`.
- [ ] `mod` + `ctx.plugin(...)` added in `crates/plugins/src/lib.rs`.

## D. UI (if applicable)

- [ ] `UiCommand` variant added.
- [ ] `root.rs` match arm added (exhaustive match).
- [ ] Service required in `XcordApp::new` (or `Option` if optional).
- [ ] Fields/snapshots initialized.
- [ ] Side panel added to body with fixed width, or modal branch added.
- [ ] Header/command entry added.
- [ ] Close button + Escape path.
- [ ] No absolute overlay for hit-testing.
- [ ] Scroll containers have IDs and bounded parents.
- [ ] Background event pump calls `cx.notify()`.
- [ ] Terminal remains usable while panel is open.
- [ ] Layout works at the default 1280×820 and larger.
- [ ] i18n source keys match exactly.

## E. Threading

- [ ] No `Rc`/`RefCell` captured in a `Send` future or `PostOffice` job.
- [ ] UI services are only touched on the UI thread.
- [ ] Worker shares only `Arc`, channels, strings, plain data.
- [ ] `broadcast::Receiver` handles `Lagged`/`Closed`.
- [ ] PostOffice closures look up services inside the closure.
- [ ] Blocking work uses `spawn_blocking`; async I/O uses `spawn`.
- [ ] Unload stops/aborts tasks and drops senders.

## F. Security

- [ ] No secrets in logs, config, MCP output, or error strings.
- [ ] Vault used for passwords/passphrases/API keys.
- [ ] External input validated.
- [ ] Filesystem paths are scoped/validated; no traversal.
- [ ] MCP read-only flag is accurate.
- [ ] Logs/buffers/queues are bounded.

## G. Tests

- [ ] Pure logic unit tests added.
- [ ] Plugin start/pending/failure test added where practical.
- [ ] Command registration/unregistration tested.
- [ ] Background state transition tested.
- [ ] `cargo fmt --all` clean.
- [ ] `cargo check --workspace` passes.
- [ ] `cargo test --workspace` passes.
- [ ] `cargo build -p xcord` passes.

## H. Manual smoke test

- [ ] App starts; no panic.
- [ ] Plugin manager shows plugin `active`.
- [ ] Command/palette entry appears and runs.
- [ ] Notice appears (if applicable).
- [ ] Panel/modal opens and closes.
- [ ] Escape does not close a terminal accidentally.
- [ ] Tabs and typing still work.
- [ ] Locale switch updates labels.
- [ ] Repeat open/close/unload does not leak listeners or duplicate commands.
- [ ] Plugin failure does not crash the app.

## I. Docs

- [ ] README or docs updated for user-visible behavior.
- [ ] New service documented if shared.
- [ ] New UI command documented.
- [ ] New dependency/config documented.
- [ ] Known limitations recorded.

## Failure triage

| Check | Action |
|---|---|
| plugin pending | provider missing or wrong `inject` name |
| plugin failed | inspect `PluginInfo.error`/diagnostics; fix apply |
| no command | inject commands? register in apply? effect removed? |
| no UI | added `UiCommand` arm? shell branch? |
| no repaint | subscribe event + `cx.notify()`? |
| freeze | blocking I/O on UI thread? |
| `Rc not Send` | replace with Arc or use PostOffice |
| duplicate service | rename service or remove provider |
| type mismatch | provide/fetch same wrapper type |
