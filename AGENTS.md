# Agent instructions for Xcord

Xcord is a Rust/GPUI SSH workspace with a Cordis-style plugin kernel.

## For plugin work

Before changing plugin code, load the **`xcord-plugin-development`** skill and
read:

1. `docs/plugin-development.md`
2. `docs/plugin-development/service-reference.md`
3. `docs/plugin-development/ui-integration.md` (if UI changes)
4. `docs/plugin-development/checklist.md`

Use the existing service contracts in `crates/plugin-api`. Do not invent
methods or copy JavaScript Cordis patterns.

## Hard rules

- `Plugin::apply` and all GPUI/render/command handlers run on the UI thread.
  Never block them.
- Background I/O uses `RuntimeHandle`/Tokio. Never capture `Rc`/`RefCell` in a
  `Send` task or `PostOffice` job.
- Plugins send `UiCommand`; they do not construct widgets or access `XcordApp`.
- Register cleanup for commands, translations, MCP capabilities, listeners,
  tasks, and channels with `ctx.effect` or the scoped helpers.
- Use the OS keychain/vault for secrets.
- For custom UI, normal-flow panels/modals only; absolute overlays can miss
  hit-testing in this GPUI version.

## Definition of done

```bash
cargo fmt --all
cargo check --workspace
cargo test --workspace
cargo build -p xcord
```

Then smoke-test the plugin state, command/palette or header entry, panel/modal
close path, and i18n.
