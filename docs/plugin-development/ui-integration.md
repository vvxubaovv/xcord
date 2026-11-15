# Plugin UI integration in the GPUI shell

Use this document when a plugin needs a side panel, modal, settings page, header
button, or any other pixels. A plugin **cannot** create GPUI widgets itself; it
sends semantic commands and exposes snapshots. The shell owns rendering.

> Read `docs/plugin-development.md` first for the kernel/service model.

## 0. Choose the smallest UI surface

| Requirement | Integration |
|---|---|
| Show a toast/notice | `UiCommand::Notify` — no shell change |
| Command-palette action | `Commands` + a handler — no shell change |
| Existing panel (AI/FTP/automation) | reuse its service/command |
| New side panel | add `UiCommand` variant + `XcordApp` field + body child |
| New modal | add `UiCommand` variant + content branch |
| New settings page | add variant + form state + render/validate/save methods |
| New header entry | `header_button(...)` in `render_header` |
| Terminal overlay | modify `TerminalView`/terminal paint (advanced) |

Prefer a side panel or modal over a new window. The app has one GPUI window.

## 1. The data flow

```text
plugin service (UI thread)
    ├─ snapshot() -> small clone -> shell render
    ├─ command methods -> mutate state / schedule work
    └─ broadcast::Sender<Event> -> shell pump -> cx.notify()

shell
    ├─ UiCommand::{ToggleMyFeature} -> show/hide UI
    ├─ on_mouse_up -> service.method(...)
    └─ render -> service.snapshot()
```

Never let the shell depend on the concrete plugin state type. Depend on the
wrapper/trait in `crates/plugin-api`.

Never let the plugin depend on `XcordApp`. If it needs a new shell action, add a
`UiCommand` variant.

## 2. Add a semantic command

`crates/plugin-api/src/ui.rs`:

```rust
#[derive(Clone, Debug)]
pub enum UiCommand {
    // ...
    ToggleMyFeature,
}
```

Then the plugin registers a command:

```rust
let ui = (*ctx.require::<Ui>(services::UI_HOST)?).clone();
let commands = (*ctx.require::<Commands>(services::COMMANDS)?).clone();

let handle = commands.register(Command::new(
    "my-feature.open",
    i18n.t("app", "My Feature"),
    "My Feature",
    move |_| ui.send(UiCommand::ToggleMyFeature),
));
ctx.effect("unregister my-feature command", move || {
    commands.unregister(handle);
});
```

`root.rs` has an exhaustive `match` in `handle_ui_command`; adding a variant
without an arm fails the build (this is desirable).

## 3. Expose a service snapshot

Example contract in `crates/plugin-api/src/my_feature.rs`:

```rust
#[derive(Clone, Debug, Default)]
pub struct MySnapshot {
    pub open: bool,
    pub items: Vec<String>,
    pub error: Option<String>,
}

pub trait MyFeatureService {
    fn snapshot(&self) -> MySnapshot;
    fn toggle(&self);
    fn refresh(&self);
  }

#[derive(Clone)]
pub struct MyFeature(pub Rc<dyn MyFeatureService>);
```

The shell only needs `MyFeature` and `MySnapshot`.

For background state, use `Arc<Mutex<_>>` + `broadcast`; see
`docs/plugin-development.md` §6.

## 4. Acquire the service in `XcordApp::new`

In `apps/xcord/src/root.rs`, after the other `ctx.require` calls:

```rust
let my_feature = (*ctx
    .require::<MyFeature>(services::MY_FEATURE)
    .expect("my-feature plugin did not start"))
.clone();
```

Add a field:

```rust
pub struct XcordApp {
    // ...
    my_feature: MyFeature,
    my_open: bool,
    my_snapshot: MySnapshot,
    my_error: Option<String>,
}
```

Initialize:

```rust
my_feature,
my_open: false,
my_snapshot: MySnapshot::default(),
my_error: None,
```

If the service is optional, do **not** use `expect`. Keep
`Option<MyFeature>` and hide the UI when it is unavailable. Only use
`expect("... did not start")` for a hard dependency that is always installed.

## 5. Handle the UI command

`handle_ui_command` in `root.rs`:

```rust
UiCommand::ToggleMyFeature => {
    self.my_open = !self.my_open;
    if self.my_open {
        self.my_snapshot = self.my_feature.snapshot();
        self.my_error = None;
    }
    cx.notify();
}
```

If the feature is optional:

```rust
UiCommand::ToggleMyFeature => {
    if let Some(service) = &self.my_feature {
        self.my_open = !self.my_open;
        self.my_snapshot = service.snapshot();
        cx.notify();
    }
}
```

## 6. Render a side panel next to the terminal

In `Render for XcordApp`, the body is built as:

```rust
let body = div()
    .flex_1()
    .min_h(px(0.))
    .flex()
    .child(self.render_sidebar(cx))
    .child(self.render_main(cx))
    .when(self.ai_open, |body| body.child(self.render_ai_panel(cx)));
```

Add your panel **after** `render_main` so the terminal keeps its `flex_1`:

```rust
.when(self.my_open, |body| {
    body.child(self.render_my_feature_panel(cx))
});
```

The panel method should return a fixed-width column:

```rust
fn render_my_feature_panel(&self, cx: &mut Context<Self>) -> Div {
    let theme = self.theme();
    div()
        .w(px(420.))
        .h_full()
        .flex()
        .flex_col()
        .bg(rgbc(theme.panel))
        .border_l_1()
        .border_color(rgbc(theme.border))
        .child(/* header */)
        .child(/* body */)
}
```

Rules:

- Give the terminal/left area `flex_1` so it shrinks rather than overflows.
- Set a fixed panel width (`w(px(...))`) and `.h_full()`.
- Add a close button that sets `my_open = false` and calls `cx.notify()`.
- Do not use an absolute overlay for the panel; hit-testing is unreliable.
- If the panel has a scroll list, give the container an `.id(...)` and
  `.overflow_y_scroll()`.

## 7. Render a modal

Add a content branch before the fallback body. The current chain in
`Render for XcordApp` is:

```rust
let content = if self.session_form.is_some() {
    modal_shell(self.render_session_dialog(cx))
} else if self.settings_form.is_some() {
    modal_shell(self.render_settings_panel(cx))
} else {
    body
};
```

Add:

```rust
} else if self.my_open {
    modal_shell(self.render_my_feature_modal(cx))
} else {
    body
}
```

Use normal-flow `modal_shell` content, not an absolute overlay:

```rust
fn render_my_feature_modal(&mut self, cx: &mut Context<Self>) -> Div {
    let theme = self.theme();
    div()
        .w(px(720.))
        .max_h(px(620.))
        .flex()
        .flex_col()
        .rounded_lg()
        .border_1()
        .border_color(rgbc(theme.border))
        .bg(rgbc(theme.panel))
        .p_4()
        // ...
}
```

### Escape and close

Find the global Escape chain in `global_key`/`render` (where
`palette`/`session_form`/`b64_upload` are checked) and add your modal **before**
the generic fallback:

```rust
} else if self.my_open {
    self.my_open = false;
    cx.notify();
    cx.stop_propagation();
}
```

If the modal is a full-screen variant, close it to the panel state instead of
hiding everything.

## 8. Pump plugin events

If the plugin has a `broadcast::Receiver<MyEvent>`:

```rust
let mut events = self.my_feature.subscribe();
cx.spawn(async move |this, cx| loop {
    match events.recv().await {
        Ok(_) => {
            if this.update(cx, |app, cx| {
                if let Some(service) = &app.my_feature {
                    app.my_snapshot = service.snapshot();
                    cx.notify();
                }
            }).is_err() {
                break;
            }
        }
        Err(RecvError::Lagged(_)) => {
            if this.update(cx, |app, cx| {
                if let Some(service) = &app.my_feature {
                    app.my_snapshot = service.snapshot();
                    cx.notify();
                }
            }).is_err() {
                break;
            }
        }
        Err(RecvError::Closed) => break,
    }
}).detach();
```

Place this in `start_pumps` (or the equivalent startup method), not in
`render`. A render function must be side-effect-free.

If the service is always present, omit `Option` and clone the service before the
spawn:

```rust
let service = self.my_feature.clone();
let mut events = service.subscribe();
```

## 9. Header button

In `render_header`, follow the existing `header_button` pattern:

```rust
.child(self.header_button(self.tr("My Feature"), cx, |this, cx| {
    this.my_open = !this.my_open;
    if this.my_open {
        this.my_snapshot = this.my_feature.snapshot();
    }
    cx.notify();
}))
```

Keep labels short; the header space is limited. Use an i18n key rather than a
hard-coded English label for user-visible UI.

## 10. Reusable widgets

Existing shell helpers are in `root.rs`. Use them instead of inventing local
styles:

- `header_button(label, cx, handler)`
- `primary_button(label, theme)`
- `secondary_button(label, theme)`
- `small_button(label, theme)`
- `field(label, value, theme)`
- `modal_shell(content)`
- `palette_shell(content)`
- `TextInput` for single-line fields
- `SelectableText` for read-only copyable text/logs
- `ScrollHandle` + tracked scroll containers

For read-only logs, create a `SelectableText` entity once and update its text on
events; do not create a new entity every frame. It supports mouse drag and
`Ctrl+C`.

For multi-line source, reuse `ScriptEditor` (automation) or add a focused
component under `apps/xcord/src/` with its own `EntityInputHandler`, selection,
keyboard and mouse handling. Do not attempt to reuse the single-line `TextInput`
for multi-line code.

## 11. Layout and GPUI pitfalls

- **Absolute overlays:** In this GPUI version, absolute children can paint but
  miss mouse hit-testing. Use normal-flow `modal_shell`/panels.
- **Flex constraints:** A scroll child needs a bounded parent. Use
  `.flex_1().min_h(px(0.))` on the scroll parent and `.overflow_y_scroll()` on
  the scrolling container.
- **IDs:** Any element with `.overflow_y_scroll()` should have an `.id(...)`.
- **Focus:** `TextInput`/editor components manage their own focus; call
  `window.focus(&handle)` on mouse down where required.
- **Repaint:** Background events do not repaint automatically. Call
  `cx.notify()` after updating snapshot fields.
- **Borrowing:** Do not hold a service `RefCell` borrow while calling a handler
  that may re-enter the same service. Clone a snapshot and drop the borrow.
- **Blocking:** No sleeps, network, or file operations in `render`, `apply`, or
  command handlers. Use `Runtime`.

## 12. i18n for a panel

In the plugin:

```rust
const ZH: &[(&str, &str)] = &[
    ("My Feature", "我的功能"),
    ("Refresh", "刷新"),
    ("Close", "关闭"),
];

let handle = i18n.register_pairs("my-feature", "zh-CN", ZH);
ctx.effect("unregister my-feature translations", move || {
    i18n.unregister(handle);
});
```

In the shell, render with `self.tr("...")` or the i18n service directly. Source
keys must match exactly.

## 13. Settings page (optional)

If the feature needs persisted settings:

1. Add non-secret fields to `xcord_store::Settings` with
   `#[serde(default)]` if backward compatibility matters.
2. Add a form state struct in `root.rs` and a `my_settings_form` field.
3. Add a `ShowMySettings` `UiCommand`.
4. Render a form using `TextInput`/buttons.
5. On Save, clone `self.settings`, update fields, call
   `self.sessions.save_settings(...)`, update `self.settings`, and notify.
6. Never store passwords/API keys in `Settings`; use the vault.

The existing settings implementation is the reference; copy its
validate/save/rollback pattern instead of writing a parallel store.

## 14. Verification for UI changes

- [ ] Add the enum variant; `cargo check` catches missing match arms.
- [ ] Start the app; plugin manager shows the plugin `active`.
- [ ] Command palette entry opens the UI.
- [ ] Header button opens the UI.
- [ ] Escape closes the modal/panel and does not also close a terminal tab.
- [ ] Terminal remains interactive while a side panel is open.
- [ ] Close button works and leaves no stale state.
- [ ] Long text/list scrolls; no row is clipped.
- [ ] Buttons are clickable (no absolute-overlay hit-testing issue).
- [ ] Locale switch updates labels.
- [ ] Background events repaint the panel.
- [ ] Service missing/unloaded path does not panic.
- [ ] `cargo test --workspace` and `cargo build -p xcord` pass.

## 15. Minimal patch checklist

For a side panel:

1. `plugin-api/src/ui.rs`: add `ToggleMyFeature`.
2. `plugin-api/src/lib.rs`: export service/snapshot and add `MY_FEATURE`.
3. `plugins/src/my_feature.rs`: implement service + command + event sender.
4. `plugins/src/lib.rs`: `mod my_feature;` + `ctx.plugin(...)`.
5. `root.rs`: require service, add fields, match variant, body `.when`, render
   method, event pump, Escape/close.
6. i18n entries.
7. tests + build + manual smoke test.

For a modal, replace step 5's body child with a `content` branch using
`modal_shell`.
