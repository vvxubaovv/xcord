//! The GPUI shell.
//!
//! The shell owns no protocol or terminal logic. It provides the `ui-host`
//! service, renders the services plugins publish, and forwards user intent as
//! commands. Plugins can be reloaded without touching this file.

use std::collections::BTreeMap;
use std::rc::Rc;
use std::time::{Duration, Instant};

use async_channel::Receiver;
use gpui::prelude::*;
use gpui::{
    div, px, rgb, Context, Div, Entity, Font, Hsla, KeyDownEvent, MouseButton, PathPromptOptions,
    Render, Rgba, SharedString, Stateful, Window,
};
use tokio::sync::broadcast::error::RecvError;
use xcord_cordis::{Context as PluginContext, Kernel, PluginInfo, PluginState};
use xcord_plugin_api::{
    services, Ai, AiMessageView, AiSnapshot, Command, Commands, ConnectionTest, Highlights, I18n,
    McpStatus, NoticeLevel, OpenRequest, Runtime, Sessions, TabInfo, TabStatus, TerminalId, Theme,
    Themes, Ui, UiCommand, UiHost, Workspace, WorkspaceEvent,
};
use xcord_store::{AuthKind, HostKeyPolicy, SessionProfile, Settings};

use crate::terminal_view::TerminalView;
use crate::text_input::TextInput;

#[derive(Clone, Copy, PartialEq, Eq)]
enum AuthMode {
    Password,
    PublicKey,
}

struct SessionForm {
    editing: Option<String>,
    name: Entity<TextInput>,
    group: Entity<TextInput>,
    host: Entity<TextInput>,
    port: Entity<TextInput>,
    user: Entity<TextInput>,
    secret: Entity<TextInput>,
    key_path: Entity<TextInput>,
    auth: AuthMode,
    save_secret: bool,
    logging: bool,
    error: Option<String>,
}

struct SettingsForm {
    theme_id: String,
    language: String,
    font_size: f32,
    scrollback: usize,
    copy_on_select: bool,
    compose_bar: bool,
    logging: bool,
    highlight: bool,
    strict_host_keys: bool,
    error: Option<String>,
}

struct PaletteState {
    input: Entity<TextInput>,
    selected: usize,
}

struct AiConfigForm {
    base_url: Entity<TextInput>,
    model: Entity<TextInput>,
    api_key: Entity<TextInput>,
    allow_writes: bool,
    include_terminal: bool,
    error: Option<String>,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum PendingFocus {
    Terminal,
    DialogName,
    DialogSecret,
    Compose,
    Palette,
    AiInput,
    AiBase,
}

struct NoticeState {
    level: NoticeLevel,
    message: String,
    at: Instant,
}

struct UiHostImpl {
    sender: async_channel::Sender<UiCommand>,
}

impl UiHost for UiHostImpl {
    fn send(&self, command: UiCommand) {
        let _ = self.sender.try_send(command);
    }
}

pub struct XcordApp {
    kernel: Kernel,
    ctx: PluginContext,
    runtime: Runtime,
    workspace: Workspace,
    sessions: Sessions,
    commands: Commands,
    themes: Themes,
    highlights: Highlights,
    i18n: I18n,
    ai: Ai,
    mcp: McpStatus,

    ui_rx: Receiver<UiCommand>,
    terminal: Entity<TerminalView>,
    search: Entity<TextInput>,
    compose: Entity<TextInput>,
    ai_input: Entity<TextInput>,

    session_form: Option<SessionForm>,
    settings_form: Option<SettingsForm>,
    ai_config: Option<AiConfigForm>,
    test_id: Option<u64>,
    test_result: Option<ConnectionTest>,
    test_pending: bool,
    palette: Option<PaletteState>,
    plugins_panel: bool,
    ai_open: bool,
    plugin_infos: Vec<PluginInfo>,

    tabs: Vec<TabInfo>,
    active: Option<TerminalId>,
    terminal_active: Option<TerminalId>,
    theme: Rc<Theme>,
    settings: Settings,

    notice: Option<NoticeState>,
    pending_focus: Option<PendingFocus>,
}

impl XcordApp {
    pub fn new(window: &mut Window, cx: &mut Context<Self>) -> Self {
        let kernel = Kernel::new();
        let ctx = kernel.context();

        let (ui_tx, ui_rx) = async_channel::unbounded();
        let host: Rc<dyn UiHost> = Rc::new(UiHostImpl {
            sender: ui_tx.clone(),
        });
        ctx.provide(services::UI_HOST, Ui(host))
            .expect("ui-host service");

        xcord_builtins::install(&ctx);

        let runtime = (*ctx
            .require::<Runtime>(services::RUNTIME)
            .expect("runtime plugin did not start"))
        .clone();
        let workspace = (*ctx
            .require::<Workspace>(services::WORKSPACE)
            .expect("workspace plugin did not start"))
        .clone();
        let sessions = (*ctx
            .require::<Sessions>(services::SESSIONS)
            .expect("sessions plugin did not start"))
        .clone();
        let commands = (*ctx
            .require::<Commands>(services::COMMANDS)
            .expect("commands plugin did not start"))
        .clone();
        let themes = (*ctx
            .require::<Themes>(services::THEMES)
            .expect("themes plugin did not start"))
        .clone();
        let highlights = (*ctx
            .require::<Highlights>(services::HIGHLIGHTS)
            .expect("highlights plugin did not start"))
        .clone();
        let ui = (*ctx
            .require::<Ui>(services::UI_HOST)
            .expect("ui-host service missing"))
        .clone();
        let ai = (*ctx
            .require::<Ai>(services::AI)
            .expect("ai plugin did not start"))
        .clone();
        let mcp = (*ctx
            .require::<McpStatus>(services::MCP_STATUS)
            .expect("mcp plugin did not start"))
        .clone();
        let i18n = (*ctx
            .require::<I18n>(services::I18N)
            .expect("localization plugin did not start"))
        .clone();

        let settings = sessions.settings();
        let theme = themes.active();
        let window_title = i18n.t("app", "Xcord — plugin-first SSH");
        let search = Self::make_input(&theme, &i18n.t("app", "Search sessions"), false, cx);
        let compose = Self::make_input(
            &theme,
            &i18n.t("app", "Send a command to the active session…"),
            false,
            cx,
        );
        let ai_input =
            Self::make_input(&theme, &i18n.t("app", "Ask about this session…"), false, cx);
        let terminal = cx.new(|cx| {
            TerminalView::new(
                workspace.clone(),
                themes.clone(),
                highlights.clone(),
                ui.clone(),
                settings.clone(),
                cx,
            )
        });

        let mut app = Self {
            kernel,
            ctx,
            runtime,
            workspace,
            sessions,
            commands,
            themes,
            highlights,
            i18n,
            ai,
            mcp,
            ui_rx,
            terminal,
            search,
            compose,
            ai_input,
            session_form: None,
            settings_form: None,
            ai_config: None,
            test_id: None,
            test_result: None,
            test_pending: false,
            palette: None,
            plugins_panel: false,
            ai_open: false,
            plugin_infos: Vec::new(),
            tabs: Vec::new(),
            active: None,
            terminal_active: None,
            theme,
            settings,
            notice: None,
            pending_focus: Some(PendingFocus::Terminal),
        };
        app.sync(cx);
        app.start_pumps(cx);
        window.set_window_title(&window_title);
        if let Ok(session_id) = std::env::var("XCORD_AUTOCONNECT") {
            if !session_id.trim().is_empty() {
                app.connect_profile(session_id, window, cx);
            }
        }
        if let Ok(prompt) = std::env::var("XCORD_AI_PROMPT") {
            if !prompt.trim().is_empty() {
                app.ai_open = true;
                app.pending_focus = Some(PendingFocus::AiInput);
                let _ = app.ai.send(&prompt);
            }
        }
        app
    }

    fn make_input(
        theme: &Theme,
        placeholder: &str,
        password: bool,
        cx: &mut Context<Self>,
    ) -> Entity<TextInput> {
        let (fg, placeholder_color, selection, cursor) = input_colors(theme);
        let placeholder = placeholder.to_string();
        cx.new(move |cx| {
            TextInput::new(
                placeholder,
                password,
                fg,
                placeholder_color,
                selection,
                cursor,
                cx,
            )
        })
    }

    fn tr(&self, key: &str) -> String {
        self.i18n.t("app", key)
    }

    fn tr_args(&self, key: &str, args: &[(&str, String)]) -> String {
        self.i18n.t_args("app", key, args)
    }

    fn command_title(&self, command: &Command) -> String {
        self.i18n.t_or("commands", &command.id, &command.title)
    }

    fn command_category(&self, command: &Command) -> String {
        self.i18n
            .t_or("categories", &command.category, &command.category)
    }

    fn plugin_name(&self, id: &str, fallback: &str) -> String {
        self.i18n.t_or("plugin", &format!("{id}.name"), fallback)
    }

    fn plugin_description(&self, id: &str, fallback: &str) -> String {
        self.i18n
            .t_or("plugin", &format!("{id}.description"), fallback)
    }

    fn status_label(&self, label: &str) -> String {
        self.i18n.t_or("status", label, label)
    }

    fn ai_status(&self, status: &str) -> String {
        if status.is_empty() || status == "Thinking…" {
            return self.i18n.t("ai", "Thinking…");
        }
        if let Some(tool) = status
            .strip_prefix("Running ")
            .and_then(|rest| rest.strip_suffix('…'))
        {
            return self
                .i18n
                .t_args("ai", "Running {tool}…", &[("tool", tool.to_string())]);
        }
        status.to_string()
    }

    fn start_pumps(&self, cx: &mut Context<Self>) {
        // I/O jobs: drain bursts, then repaint once.
        let runtime = self.runtime.clone();
        let jobs = runtime.receiver();
        cx.spawn(async move |this, cx| loop {
            let first = match jobs.recv().await {
                Ok(job) => job,
                Err(_) => break,
            };
            runtime.execute(first);
            while let Some(job) = runtime.try_next_job() {
                runtime.execute(job);
            }
            if this
                .update(cx, |app, cx| {
                    app.sync(cx);
                    cx.notify();
                })
                .is_err()
            {
                break;
            }
        })
        .detach();

        // Workspace notifications.
        let mut events = self.workspace.subscribe();
        cx.spawn(async move |this, cx| loop {
            match events.recv().await {
                Ok(event) => {
                    if this
                        .update(cx, |app, cx| {
                            app.on_workspace_event(event, cx);
                        })
                        .is_err()
                    {
                        break;
                    }
                }
                Err(RecvError::Lagged(_)) => {
                    if this
                        .update(cx, |app, cx| {
                            app.sync(cx);
                            cx.notify();
                        })
                        .is_err()
                    {
                        break;
                    }
                }
                Err(RecvError::Closed) => break,
            }
        })
        .detach();

        // Commands sent by plugins.
        let ui_rx = self.ui_rx.clone();
        cx.spawn(async move |this, cx| {
            while let Ok(command) = ui_rx.recv().await {
                if this
                    .update(cx, |app, cx| {
                        app.handle_ui_command(command, cx);
                    })
                    .is_err()
                {
                    break;
                }
            }
        })
        .detach();
    }

    fn sync(&mut self, cx: &mut Context<Self>) {
        self.tabs = self.workspace.tabs();
        self.active = self.workspace.active();
        self.theme = self.themes.active();
        if self.terminal_active != self.active {
            let active = self.active;
            self.terminal
                .update(cx, |view, cx| view.set_active(active, cx));
            self.terminal_active = active;
        }
    }

    fn on_workspace_event(&mut self, event: WorkspaceEvent, cx: &mut Context<Self>) {
        self.sync(cx);
        if let Some(id) = self.ai.context_terminal() {
            if !self.tabs.iter().any(|tab| tab.id == id) {
                self.ai.set_context_terminal(None);
            }
        }
        if let Some(tab) = self
            .active
            .and_then(|id| self.tabs.iter().find(|tab| tab.id == id))
        {
            if tab.status.is_connected()
                && self
                    .notice
                    .as_ref()
                    .map(|notice| notice.message.starts_with("Connecting"))
                    .unwrap_or(false)
            {
                self.notice = None;
            }
        }
        match event {
            WorkspaceEvent::Notice { level, message } => {
                self.set_notice(level, message);
            }
            WorkspaceEvent::Dirty(id) => {
                if self.active == Some(id) {
                    self.terminal.update(cx, |_, cx| cx.notify());
                }
            }
            WorkspaceEvent::StatusChanged(id) | WorkspaceEvent::Bell(id) => {
                if self.active == Some(id) {
                    self.terminal.update(cx, |_, cx| cx.notify());
                }
            }
            WorkspaceEvent::TestFinished(result) => {
                if self.test_id == Some(result.id) {
                    self.test_pending = false;
                    self.test_result = Some(result);
                }
            }
            WorkspaceEvent::TabsChanged
            | WorkspaceEvent::ActiveChanged(_)
            | WorkspaceEvent::Closed(_) => {}
        }
        cx.notify();
    }

    fn handle_ui_command(&mut self, command: UiCommand, cx: &mut Context<Self>) {
        match command {
            UiCommand::NewSession => self.open_session_dialog(None, cx),
            UiCommand::EditSession(id) => self.open_session_dialog(Some(id), cx),
            UiCommand::OpenPalette => self.open_palette(cx),
            UiCommand::ShowPlugins => {
                self.plugins_panel = true;
                self.refresh_plugins();
            }
            UiCommand::ShowSettings => self.open_settings(cx),
            UiCommand::FocusCompose => {
                self.settings.compose_bar = true;
                self.pending_focus = Some(PendingFocus::Compose);
            }
            UiCommand::Notify { level, message } => self.set_notice(level, message),
            UiCommand::CloseActiveTab => {
                if let Some(id) = self.active {
                    self.workspace.close(id);
                    self.sync(cx);
                }
            }
            UiCommand::ReconnectActive => {
                if let Some(id) = self.active {
                    if let Err(error) = self.workspace.reconnect(id) {
                        self.set_notice(NoticeLevel::Error, error);
                    }
                    self.sync(cx);
                }
            }
            UiCommand::SelectNextTab => {
                self.workspace.select_next();
                self.sync(cx);
            }
            UiCommand::SelectPrevTab => {
                self.workspace.select_previous();
                self.sync(cx);
            }
            UiCommand::CopySelection | UiCommand::Paste => {}
            UiCommand::ToggleAi => {
                self.ai_open = !self.ai_open;
                if self.ai_open {
                    self.pending_focus = Some(PendingFocus::AiInput);
                }
            }
            UiCommand::OpenAiSettings => self.open_ai_config(cx),
        }
        cx.notify();
    }

    fn set_notice(&mut self, level: NoticeLevel, message: impl Into<String>) {
        self.notice = Some(NoticeState {
            level,
            message: message.into(),
            at: Instant::now(),
        });
    }

    fn open_session_dialog(&mut self, profile_id: Option<String>, cx: &mut Context<Self>) {
        let existing = profile_id.as_ref().and_then(|id| self.sessions.get(id));
        let is_new = existing.is_none();
        let profile = existing.clone().unwrap_or_else(|| SessionProfile::new(""));
        let theme = self.theme.clone();
        let name = Self::make_input(&theme, &self.tr("Production web"), false, cx);
        let host = Self::make_input(&theme, "example.com", false, cx);
        let port = Self::make_input(&theme, "22", false, cx);
        let user = Self::make_input(&theme, "root", false, cx);
        let secret = Self::make_input(
            &theme,
            &self.tr("Leave blank to use the keychain"),
            true,
            cx,
        );
        let key_path = Self::make_input(&theme, "~/.ssh/id_ed25519", false, cx);
        let group = Self::make_input(&theme, "Sessions", false, cx);

        name.update(cx, |input, cx| input.set_value(profile.name.clone(), cx));
        let group_value = if is_new {
            self.tr("Sessions")
        } else {
            profile.group.clone()
        };
        group.update(cx, |input, cx| input.set_value(group_value, cx));
        host.update(cx, |input, cx| input.set_value(profile.host.clone(), cx));
        port.update(cx, |input, cx| {
            input.set_value(profile.port.to_string(), cx)
        });
        user.update(cx, |input, cx| input.set_value(profile.user.clone(), cx));

        let (auth, save_secret, key) = match &profile.auth {
            AuthKind::Password { save, .. } => (AuthMode::Password, *save, String::new()),
            AuthKind::PublicKey {
                path,
                save_passphrase,
                ..
            } => (AuthMode::PublicKey, *save_passphrase, path.clone()),
            AuthKind::Agent | AuthKind::None => (AuthMode::Password, false, String::new()),
        };
        key_path.update(cx, |input, cx| input.set_value(key, cx));

        self.session_form = Some(SessionForm {
            editing: profile_id,
            name,
            group,
            host,
            port,
            user,
            secret,
            key_path,
            auth,
            save_secret,
            logging: profile.logging,
            error: None,
        });
        self.test_id = None;
        self.test_result = None;
        self.test_pending = false;
        self.pending_focus = Some(PendingFocus::DialogName);
        cx.notify();
    }

    fn close_session_dialog(&mut self, cx: &mut Context<Self>) {
        self.session_form = None;
        self.test_id = None;
        self.test_result = None;
        self.test_pending = false;
        self.pending_focus = Some(PendingFocus::Terminal);
        cx.notify();
    }

    fn pick_private_key(&mut self, _window: &mut Window, cx: &mut Context<Self>) {
        let receiver = cx.prompt_for_paths(PathPromptOptions {
            files: true,
            directories: false,
            multiple: false,
            prompt: Some("Select a private key".into()),
        });
        cx.spawn(async move |this, cx| {
            let Ok(Ok(Some(paths))) = receiver.await else {
                return;
            };
            let Some(path) = paths.into_iter().next() else {
                return;
            };
            let path = path.to_string_lossy().into_owned();
            let _ = this.update(cx, |this, cx| {
                if let Some(form) = &this.session_form {
                    form.key_path
                        .update(cx, |input, cx| input.set_value(path, cx));
                }
                cx.notify();
            });
        })
        .detach();
    }

    fn focus_session_next(&mut self, reverse: bool, window: &mut Window, cx: &mut Context<Self>) {
        let Some(form) = self.session_form.as_ref() else {
            return;
        };
        let mut handles = vec![
            form.name.read(cx).focus_handle_only(),
            form.group.read(cx).focus_handle_only(),
            form.host.read(cx).focus_handle_only(),
            form.port.read(cx).focus_handle_only(),
            form.user.read(cx).focus_handle_only(),
        ];
        if form.auth == AuthMode::Password {
            handles.push(form.secret.read(cx).focus_handle_only());
        } else {
            handles.push(form.key_path.read(cx).focus_handle_only());
        }
        let current = handles.iter().position(|handle| handle.is_focused(window));
        let next = match (current, reverse) {
            (Some(index), false) => (index + 1) % handles.len(),
            (Some(index), true) => (index + handles.len() - 1) % handles.len(),
            (None, false) => 0,
            (None, true) => handles.len() - 1,
        };
        window.focus(&handles[next]);
        cx.notify();
    }

    fn submit_session(&mut self, _window: &mut Window, cx: &mut Context<Self>) {
        let Some(form) = self.session_form.as_ref() else {
            return;
        };
        let name = form.name.read(cx).value().trim().to_string();
        let host = form.host.read(cx).value().trim().to_string();
        let port_text = form.port.read(cx).value().trim().to_string();
        let user = form.user.read(cx).value().trim().to_string();
        let secret = form.secret.read(cx).value().to_string();
        let key_path = form.key_path.read(cx).value().trim().to_string();
        let auth = form.auth;
        let save_secret = form.save_secret;
        let logging = form.logging;
        let group = form.group.read(cx).value().trim().to_string();
        let editing = form.editing.clone();

        if host.is_empty() {
            let message = self.tr("Host is required.");
            if let Some(form) = &mut self.session_form {
                form.error = Some(message);
            }
            return;
        }
        let port = match port_text.parse::<u16>() {
            Ok(port) if port > 0 => port,
            _ => {
                let message = self.tr("Port must be a number between 1 and 65535.");
                if let Some(form) = &mut self.session_form {
                    form.error = Some(message);
                }
                return;
            }
        };
        if auth == AuthMode::PublicKey && key_path.is_empty() {
            let message = self.tr("Choose a private key file.");
            if let Some(form) = &mut self.session_form {
                form.error = Some(message);
            }
            return;
        }

        let existing = editing.as_ref().and_then(|id| self.sessions.get(id));
        let mut profile = existing.clone().unwrap_or_else(|| SessionProfile::new(""));
        profile.name = if name.is_empty() { host.clone() } else { name };
        profile.host = host;
        profile.port = port;
        profile.user = user;
        profile.group = if group.is_empty() {
            "Sessions".into()
        } else {
            group
        };
        profile.logging = logging;
        profile.auth = match auth {
            AuthMode::Password => AuthKind::Password {
                save: save_secret,
                secret_id: existing.as_ref().and_then(|profile| match &profile.auth {
                    AuthKind::Password { secret_id, .. } => secret_id.clone(),
                    _ => None,
                }),
            },
            AuthMode::PublicKey => AuthKind::PublicKey {
                path: key_path,
                save_passphrase: save_secret,
                secret_id: existing.as_ref().and_then(|profile| match &profile.auth {
                    AuthKind::PublicKey { secret_id, .. } => secret_id.clone(),
                    _ => None,
                }),
            },
        };

        if let Err(error) = self.sessions.save(profile.clone()) {
            if let Some(form) = &mut self.session_form {
                form.error = Some(error);
            }
            return;
        }

        if save_secret && !secret.is_empty() {
            let result = match auth {
                AuthMode::Password => self.sessions.store_password(&profile, &secret),
                AuthMode::PublicKey => self.sessions.store_passphrase(&profile, &secret),
            };
            if let Err(error) = result {
                self.set_notice(
                    NoticeLevel::Warning,
                    self.tr_args("Secret was not saved: {error}", &[("error", error)]),
                );
            }
        }

        let credential = if !secret.is_empty() {
            Some(secret)
        } else {
            match auth {
                AuthMode::Password => self.sessions.password(&profile),
                AuthMode::PublicKey => self.sessions.passphrase(&profile),
            }
        };
        if auth == AuthMode::Password && credential.is_none() {
            let message = self.tr("This session needs a password.");
            if let Some(form) = &mut self.session_form {
                form.error = Some(message);
            }
            return;
        }

        match self.workspace.open(OpenRequest {
            profile: profile.clone(),
            credential,
            cols: 80,
            rows: 24,
        }) {
            Ok(id) => {
                self.session_form = None;
                self.pending_focus = Some(PendingFocus::Terminal);
                self.sync(cx);
                self.set_notice(
                    NoticeLevel::Info,
                    self.tr_args(
                        "Connecting to {endpoint}…",
                        &[("endpoint", profile.endpoint())],
                    ),
                );
                let _ = id;
            }
            Err(error) => {
                if let Some(form) = &mut self.session_form {
                    form.error = Some(error.clone());
                }
                self.set_notice(NoticeLevel::Error, error);
            }
        }
        cx.notify();
    }

    fn session_test_request(&self, cx: &Context<Self>) -> std::result::Result<OpenRequest, String> {
        let Some(form) = self.session_form.as_ref() else {
            return Err("No session form is open.".into());
        };
        let name = form.name.read(cx).value().trim().to_string();
        let host = form.host.read(cx).value().trim().to_string();
        let port_text = form.port.read(cx).value().trim().to_string();
        let user = form.user.read(cx).value().trim().to_string();
        let secret = form.secret.read(cx).value().to_string();
        let key_path = form.key_path.read(cx).value().trim().to_string();
        let auth = form.auth;
        let save_secret = form.save_secret;
        let logging = form.logging;
        let group = form.group.read(cx).value().trim().to_string();
        let editing = form.editing.clone();

        if host.is_empty() {
            return Err("Host is required.".into());
        }
        let port = port_text
            .parse::<u16>()
            .ok()
            .filter(|port| *port > 0)
            .ok_or_else(|| "Port must be a number between 1 and 65535.".to_string())?;
        if auth == AuthMode::PublicKey && key_path.is_empty() {
            return Err("Choose a private key file.".into());
        }

        let existing = editing.as_ref().and_then(|id| self.sessions.get(id));
        let mut profile = existing
            .clone()
            .unwrap_or_else(|| SessionProfile::new(if name.is_empty() { &host } else { &name }));
        profile.name = if name.is_empty() { host.clone() } else { name };
        profile.host = host;
        profile.port = port;
        profile.user = user;
        profile.group = if group.is_empty() {
            "Sessions".into()
        } else {
            group
        };
        profile.logging = logging;
        profile.auth = match auth {
            AuthMode::Password => AuthKind::Password {
                save: save_secret,
                secret_id: existing
                    .as_ref()
                    .and_then(|profile| match &profile.auth {
                        AuthKind::Password { secret_id, .. } => secret_id.clone(),
                        _ => None,
                    })
                    .or_else(|| Some(format!("session:{}:password", profile.id))),
            },
            AuthMode::PublicKey => AuthKind::PublicKey {
                path: key_path,
                save_passphrase: save_secret,
                secret_id: existing.as_ref().and_then(|profile| match &profile.auth {
                    AuthKind::PublicKey { secret_id, .. } => secret_id.clone(),
                    _ => None,
                }),
            },
        };
        let credential = if secret.is_empty() {
            match auth {
                AuthMode::Password => self.sessions.password(&profile),
                AuthMode::PublicKey => self.sessions.passphrase(&profile),
            }
        } else {
            Some(secret)
        };
        if auth == AuthMode::Password && credential.is_none() {
            return Err("Enter a password to test this session.".into());
        }
        Ok(OpenRequest {
            profile,
            credential,
            cols: 80,
            rows: 24,
        })
    }

    fn test_session(&mut self, cx: &mut Context<Self>) {
        if self.test_pending {
            return;
        }
        let request = match self.session_test_request(cx) {
            Ok(request) => request,
            Err(error) => {
                let message = self.tr(&error);
                if let Some(form) = &mut self.session_form {
                    form.error = Some(message);
                }
                cx.notify();
                return;
            }
        };
        match self.workspace.test_connection(request) {
            Ok(id) => {
                self.test_id = Some(id);
                self.test_result = None;
                self.test_pending = true;
                if let Some(form) = &mut self.session_form {
                    form.error = None;
                }
            }
            Err(error) => {
                let message = self.tr(&error);
                if let Some(form) = &mut self.session_form {
                    form.error = Some(message);
                }
            }
        }
        cx.notify();
    }

    fn connect_profile(&mut self, profile_id: String, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(tab) = self.tabs.iter().find(|tab| tab.profile_id == profile_id) {
            self.workspace.select(tab.id);
            self.pending_focus = Some(PendingFocus::Terminal);
            self.sync(cx);
            cx.notify();
            return;
        }
        let Some(profile) = self.sessions.get(&profile_id) else {
            self.set_notice(NoticeLevel::Error, self.tr("Session no longer exists."));
            return;
        };
        let credential = match &profile.auth {
            AuthKind::Password { .. } => self.sessions.password(&profile),
            AuthKind::PublicKey { .. } => self.sessions.passphrase(&profile),
            _ => None,
        };
        if matches!(profile.auth, AuthKind::Password { .. }) && credential.is_none() {
            self.open_session_dialog(Some(profile_id), cx);
            let message = self.tr("Enter the password for this session.");
            if let Some(form) = &mut self.session_form {
                form.error = Some(message);
            }
            self.pending_focus = Some(PendingFocus::DialogSecret);
            cx.notify();
            return;
        }
        match self.workspace.open(OpenRequest {
            profile,
            credential,
            cols: 80,
            rows: 24,
        }) {
            Ok(_) => {
                self.pending_focus = Some(PendingFocus::Terminal);
                self.sync(cx);
                self.set_notice(NoticeLevel::Info, self.tr("Connecting…"));
            }
            Err(error) => {
                self.set_notice(NoticeLevel::Error, error);
            }
        }
        let _ = window;
        cx.notify();
    }

    fn filtered_sessions(&self, cx: &Context<Self>) -> Vec<SessionProfile> {
        let query = self.search.read(cx).value().trim().to_lowercase();
        let mut sessions = self.sessions.list();
        if !query.is_empty() {
            sessions.retain(|profile| {
                profile.name.to_lowercase().contains(&query)
                    || profile.host.to_lowercase().contains(&query)
                    || profile.user.to_lowercase().contains(&query)
                    || profile
                        .tags
                        .iter()
                        .any(|tag| tag.to_lowercase().contains(&query))
            });
        }
        sessions.sort_by(|a, b| a.name.to_lowercase().cmp(&b.name.to_lowercase()));
        sessions
    }

    fn connect_first_filtered(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(profile) = self.filtered_sessions(cx).first() {
            let id = profile.id.clone();
            self.connect_profile(id, window, cx);
        }
    }

    fn send_compose(&mut self, _window: &mut Window, cx: &mut Context<Self>) {
        let Some(id) = self.active else {
            return;
        };
        let text = self.compose.read(cx).value().to_string();
        if text.trim().is_empty() {
            return;
        }
        self.workspace.write(id, text.as_bytes());
        self.workspace.write(id, b"\r");
        self.compose.update(cx, |input, cx| input.clear(cx));
        self.pending_focus = Some(PendingFocus::Compose);
        cx.notify();
    }

    fn send_snippet(&mut self, command: &str, send_enter: bool, cx: &mut Context<Self>) {
        let Some(id) = self.active else {
            return;
        };
        self.workspace.write(id, command.as_bytes());
        if send_enter {
            self.workspace.write(id, b"\r");
        }
        self.pending_focus = Some(PendingFocus::Terminal);
        cx.notify();
    }

    fn open_settings(&mut self, cx: &mut Context<Self>) {
        self.settings_form = Some(SettingsForm {
            theme_id: self.themes.active_id().to_string(),
            language: self.settings.language.clone(),
            font_size: self.settings.font_size,
            scrollback: self.settings.scrollback,
            copy_on_select: self.settings.copy_on_select,
            compose_bar: self.settings.compose_bar,
            logging: self.settings.logging,
            highlight: self.settings.highlight,
            strict_host_keys: self.settings.host_key_policy == HostKeyPolicy::Strict,
            error: None,
        });
        cx.notify();
    }

    fn save_settings(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(form) = self.settings_form.as_ref() else {
            return;
        };
        let mut settings = self.settings.clone();
        settings.theme = form.theme_id.clone();
        settings.language = form.language.clone();
        settings.font_size = form.font_size.clamp(9.0, 28.0);
        settings.scrollback = form.scrollback.clamp(500, 200_000);
        settings.copy_on_select = form.copy_on_select;
        settings.compose_bar = form.compose_bar;
        settings.logging = form.logging;
        settings.highlight = form.highlight;
        settings.host_key_policy = if form.strict_host_keys {
            HostKeyPolicy::Strict
        } else {
            HostKeyPolicy::AcceptNew
        };
        if let Err(error) = self.sessions.save_settings(settings.clone()) {
            if let Some(form) = &mut self.settings_form {
                form.error = Some(error);
            }
            return;
        }
        self.settings = settings.clone();
        self.themes.set_active(&settings.theme);
        self.theme = self.themes.active();
        if let Err(error) = self.i18n.set_locale(&settings.language) {
            if let Some(form) = &mut self.settings_form {
                form.error = Some(error);
            }
        }
        let window_title = self.tr("Xcord — plugin-first SSH");
        window.set_window_title(&window_title);
        let search_placeholder = self.tr("Search sessions");
        self.search.update(cx, |input, cx| {
            input.set_placeholder(search_placeholder, cx)
        });
        let compose_placeholder = self.tr("Send a command to the active session…");
        self.compose.update(cx, |input, cx| {
            input.set_placeholder(compose_placeholder, cx)
        });
        let ai_placeholder = self.tr("Ask about this session…");
        self.ai_input
            .update(cx, |input, cx| input.set_placeholder(ai_placeholder, cx));
        self.terminal
            .update(cx, |view, cx| view.set_settings(settings, cx));
        self.settings_form = None;
        self.set_notice(
            NoticeLevel::Success,
            self.tr("Settings saved. Reload plugins to apply logging/highlighter changes."),
        );
        cx.notify();
    }

    fn open_ai_config(&mut self, cx: &mut Context<Self>) {
        let settings = self.sessions.settings();
        let theme = self.theme.clone();
        let base_url = Self::make_input(&theme, "https://api.deepseek.com/v1", false, cx);
        let model = Self::make_input(&theme, "deepseek-chat", false, cx);
        let api_key = Self::make_input(
            &theme,
            &self.tr("Leave blank to keep the stored key"),
            true,
            cx,
        );
        base_url.update(cx, |input, cx| {
            input.set_value(settings.ai_base_url.clone(), cx)
        });
        model.update(cx, |input, cx| {
            input.set_value(settings.ai_model.clone(), cx)
        });
        self.ai_config = Some(AiConfigForm {
            base_url,
            model,
            api_key,
            allow_writes: settings.ai_allow_writes,
            include_terminal: settings.ai_include_terminal,
            error: None,
        });
        self.pending_focus = Some(PendingFocus::AiBase);
        cx.notify();
    }

    fn save_ai_config(&mut self, cx: &mut Context<Self>) {
        let Some(form) = self.ai_config.as_ref() else {
            return;
        };
        let base_url = form.base_url.read(cx).value().trim().to_string();
        let model = form.model.read(cx).value().trim().to_string();
        let api_key = form.api_key.read(cx).value().to_string();
        let allow_writes = form.allow_writes;
        let include_terminal = form.include_terminal;
        if !base_url.starts_with("http://") && !base_url.starts_with("https://") {
            let message = self.tr("Base URL must start with http:// or https://");
            if let Some(form) = &mut self.ai_config {
                form.error = Some(message);
            }
            return;
        }
        if model.is_empty() {
            let message = self.tr("Model is required.");
            if let Some(form) = &mut self.ai_config {
                form.error = Some(message);
            }
            return;
        }
        let mut settings = self.sessions.settings();
        settings.ai_base_url = base_url;
        settings.ai_model = model;
        settings.ai_allow_writes = allow_writes;
        settings.ai_include_terminal = include_terminal;
        if let Err(error) = self.sessions.save_settings(settings.clone()) {
            if let Some(form) = &mut self.ai_config {
                form.error = Some(error);
            }
            return;
        }
        self.settings = settings;
        if !api_key.trim().is_empty() {
            if let Err(error) = self.sessions.store_ai_api_key(api_key.trim()) {
                if let Some(form) = &mut self.ai_config {
                    form.error = Some(error);
                }
                return;
            }
        }
        self.ai.refresh_key();
        self.ai_config = None;
        self.pending_focus = Some(PendingFocus::AiInput);
        self.set_notice(NoticeLevel::Success, self.tr("AI settings saved."));
        cx.notify();
    }

    fn clear_ai_key(&mut self, cx: &mut Context<Self>) {
        match self.sessions.clear_ai_api_key() {
            Ok(()) => {
                self.ai.refresh_key();
                self.set_notice(NoticeLevel::Success, self.tr("Stored AI API key removed."));
            }
            Err(error) => self.set_notice(NoticeLevel::Error, error),
        }
        cx.notify();
    }

    fn send_ai(&mut self, cx: &mut Context<Self>) {
        let prompt = self.ai_input.read(cx).value().trim().to_string();
        if prompt.is_empty() {
            return;
        }
        match self.ai.send(&prompt) {
            Ok(()) => {
                self.ai_input.update(cx, |input, cx| input.clear(cx));
                self.pending_focus = Some(PendingFocus::AiInput);
            }
            Err(error) => self.set_notice(NoticeLevel::Error, error),
        }
        cx.notify();
    }

    fn send_ai_prompt(&mut self, prompt: &str, cx: &mut Context<Self>) {
        if let Err(error) = self.ai.send(prompt) {
            self.set_notice(NoticeLevel::Error, error);
        }
        self.pending_focus = Some(PendingFocus::AiInput);
        cx.notify();
    }

    fn toggle_ai_include_terminal(&mut self, cx: &mut Context<Self>) {
        let mut settings = self.sessions.settings();
        settings.ai_include_terminal = !settings.ai_include_terminal;
        if let Err(error) = self.sessions.save_settings(settings.clone()) {
            self.set_notice(NoticeLevel::Error, error);
        } else {
            self.settings = settings;
        }
        cx.notify();
    }

    fn toggle_ai_allow_writes(&mut self, cx: &mut Context<Self>) {
        let mut settings = self.sessions.settings();
        settings.ai_allow_writes = !settings.ai_allow_writes;
        if let Err(error) = self.sessions.save_settings(settings.clone()) {
            self.set_notice(NoticeLevel::Error, error);
        } else {
            self.settings = settings;
        }
        cx.notify();
    }

    fn render_ai_panel(&self, cx: &mut Context<Self>) -> Div {
        let theme = self.theme();
        let snapshot: AiSnapshot = self.ai.snapshot();
        let mut messages = div()
            .id("ai-messages")
            .flex()
            .flex_col()
            .gap_3()
            .flex_1()
            .overflow_y_scroll()
            .px_3()
            .py_3();

        if snapshot.messages.is_empty() {
            messages = messages
                .child(
                    div()
                        .text_size(px(12.))
                        .text_color(rgbc(theme.muted))
                        .child(self.tr("Ask the copilot to inspect a session, explain output, or run a fix with tools.")),
                )
                .child(
                    secondary_button(self.tr("Explain this terminal"), theme).on_mouse_up(
                        MouseButton::Left,
                        cx.listener(|this, _, _, cx| {
                            this.send_ai_prompt(
                                "Explain what the active terminal is showing and what I should do next.",
                                cx,
                            )
                        }),
                    ),
                )
                .child(
                    secondary_button(self.tr("Why did the last command fail?"), theme).on_mouse_up(
                        MouseButton::Left,
                        cx.listener(|this, _, _, cx| {
                            this.send_ai_prompt(
                                "Look at the recent terminal output and explain why the last command failed.",
                                cx,
                            )
                        }),
                    ),
                )
                .child(
                    secondary_button(self.tr("Summarize open tabs"), theme).on_mouse_up(
                        MouseButton::Left,
                        cx.listener(|this, _, _, cx| {
                            this.send_ai_prompt(
                                "Summarize the open tabs and their connection status.",
                                cx,
                            )
                        }),
                    ),
                );
        } else {
            for message in &snapshot.messages {
                messages = messages.child(render_ai_message(message, theme));
            }
        }

        if snapshot.streaming {
            messages = messages.child(
                div()
                    .text_size(px(10.5))
                    .text_color(rgbc(theme.accent))
                    .child(if snapshot.status.is_empty() {
                        self.tr("Thinking…")
                    } else {
                        self.ai_status(&snapshot.status)
                    }),
            );
        }
        if let Some(error) = &snapshot.error {
            messages = messages.child(
                div()
                    .p_2()
                    .rounded_md()
                    .bg(rgba(theme.danger, 0.12))
                    .border_1()
                    .border_color(rgba(theme.danger, 0.4))
                    .text_size(px(10.5))
                    .text_color(rgbc(theme.danger))
                    .child(error.clone()),
            );
        }

        let mut footer = div()
            .border_t_1()
            .border_color(rgbc(theme.border))
            .p_2()
            .flex()
            .flex_col()
            .gap_2();
        footer = footer
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap_3()
                    .child(
                        checkbox(
                            self.tr("Terminal context"),
                            snapshot.include_terminal,
                            theme,
                        )
                        .on_mouse_up(
                            MouseButton::Left,
                            cx.listener(|this, _, _, cx| this.toggle_ai_include_terminal(cx)),
                        ),
                    )
                    .child(
                        checkbox(
                            self.tr(if snapshot.allow_writes {
                                "Writes allowed"
                            } else {
                                "Writes blocked"
                            }),
                            snapshot.allow_writes,
                            theme,
                        )
                        .on_mouse_up(
                            MouseButton::Left,
                            cx.listener(|this, _, _, cx| this.toggle_ai_allow_writes(cx)),
                        ),
                    )
                    .child(
                        div()
                            .text_size(px(9.5))
                            .text_color(rgbc(theme.muted))
                            .child(snapshot.model.clone()),
                    ),
            )
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap_2()
                    .child(
                        div()
                            .flex_1()
                            .h(px(34.))
                            .bg(rgbc(theme.background))
                            .border_1()
                            .border_color(rgbc(theme.border))
                            .rounded_md()
                            .child(self.ai_input.clone()),
                    )
                    .child(primary_button(self.tr("Send"), theme).on_mouse_up(
                        MouseButton::Left,
                        cx.listener(|this, _, _, cx| this.send_ai(cx)),
                    )),
            )
            .on_key_down(cx.listener(|this, event: &KeyDownEvent, _, cx| {
                if event.keystroke.key == "enter" {
                    this.send_ai(cx);
                    cx.stop_propagation();
                }
            }));

        div()
            .w(px(400.))
            .h_full()
            .flex()
            .flex_col()
            .bg(rgbc(theme.panel))
            .border_l_1()
            .border_color(rgbc(theme.border))
            .child(
                div()
                    .h(px(46.))
                    .px_3()
                    .flex()
                    .items_center()
                    .justify_between()
                    .border_b_1()
                    .border_color(rgbc(theme.border))
                    .child(
                        div()
                            .flex()
                            .flex_col()
                            .child(
                                div()
                                    .text_size(px(13.))
                                    .font_weight(gpui::FontWeight::BOLD)
                                    .text_color(rgbc(theme.foreground))
                                    .child(self.tr("AI Copilot")),
                            )
                            .child(
                                div()
                                    .text_size(px(9.5))
                                    .text_color(if snapshot.has_key {
                                        rgbc(theme.muted)
                                    } else {
                                        rgbc(theme.danger)
                                    })
                                    .child(if snapshot.has_key {
                                        self.tr_args(
                                            "{model} · MCP tools",
                                            &[("model", snapshot.model.clone())],
                                        )
                                    } else {
                                        self.tr("No API key — open settings")
                                    }),
                            ),
                    )
                    .child(
                        div()
                            .flex()
                            .items_center()
                            .gap_2()
                            .child(small_button(self.tr("Settings"), theme).on_mouse_up(
                                MouseButton::Left,
                                cx.listener(|this, _, _, cx| this.open_ai_config(cx)),
                            ))
                            .child(
                                div()
                                    .id("ai-close")
                                    .px_2()
                                    .rounded_sm()
                                    .text_size(px(16.))
                                    .text_color(rgbc(theme.muted))
                                    .cursor_pointer()
                                    .on_mouse_up(
                                        MouseButton::Left,
                                        cx.listener(|this, _, _, cx| {
                                            this.ai_open = false;
                                            cx.notify();
                                        }),
                                    )
                                    .child("×"),
                            ),
                    ),
            )
            .child(self.render_ai_context_bar(&snapshot, cx))
            .child(messages)
            .child(footer)
    }

    fn render_ai_context_bar(&self, snapshot: &AiSnapshot, cx: &mut Context<Self>) -> Div {
        let theme = self.theme();
        let title = match &snapshot.context_title {
            Some(title) => self.tr_args(
                "Conversation terminal · {title}",
                &[("title", title.clone())],
            ),
            None => self.tr("Conversation terminal"),
        };
        let mut tabs = div()
            .id("ai-context-tabs")
            .flex()
            .gap_1()
            .overflow_x_scroll()
            .pb_1();
        tabs = tabs.child(
            div()
                .id("ai-context-follow")
                .px_2()
                .py_1()
                .rounded_md()
                .border_1()
                .border_color(if snapshot.context_follows_active {
                    rgbc(theme.accent)
                } else {
                    rgbc(theme.border)
                })
                .bg(if snapshot.context_follows_active {
                    rgba(theme.accent, 0.18)
                } else {
                    rgba(theme.panel_alt, 0.0)
                })
                .text_size(px(10.))
                .text_color(if snapshot.context_follows_active {
                    rgbc(theme.accent)
                } else {
                    rgbc(theme.muted)
                })
                .cursor_pointer()
                .on_mouse_up(
                    MouseButton::Left,
                    cx.listener(|this, _, _, cx| {
                        this.ai.set_context_terminal(None);
                        cx.notify();
                    }),
                )
                .child(self.tr("Follow active tab")),
        );
        for tab in &self.tabs {
            let id = tab.id;
            let selected =
                !snapshot.context_follows_active && snapshot.context_terminal == Some(id);
            let label = if tab.title.is_empty() {
                format!("#{}", id.raw())
            } else {
                tab.title.clone()
            };
            tabs = tabs.child(
                div()
                    .id(SharedString::from(format!("ai-context-{}", id.raw())))
                    .px_2()
                    .py_1()
                    .max_w(px(150.))
                    .overflow_hidden()
                    .text_ellipsis()
                    .rounded_md()
                    .border_1()
                    .border_color(if selected {
                        rgbc(theme.accent)
                    } else {
                        rgbc(theme.border)
                    })
                    .bg(if selected {
                        rgba(theme.accent, 0.18)
                    } else {
                        rgba(theme.panel_alt, 0.0)
                    })
                    .text_size(px(10.))
                    .text_color(if selected {
                        rgbc(theme.accent)
                    } else {
                        rgbc(theme.muted)
                    })
                    .cursor_pointer()
                    .on_mouse_up(
                        MouseButton::Left,
                        cx.listener(move |this, _, _, cx| {
                            this.ai.set_context_terminal(Some(id));
                            this.workspace.select(id);
                            this.sync(cx);
                            this.pending_focus = Some(PendingFocus::AiInput);
                            cx.notify();
                        }),
                    )
                    .child(label),
            );
        }
        if self.tabs.is_empty() {
            tabs = tabs.child(
                div()
                    .px_2()
                    .py_1()
                    .text_size(px(10.))
                    .text_color(rgbc(theme.muted))
                    .child(self.tr("No open terminals")),
            );
        }
        div()
            .px_3()
            .py_2()
            .border_b_1()
            .border_color(rgbc(theme.border))
            .flex()
            .flex_col()
            .gap_1()
            .child(
                div()
                    .text_size(px(9.5))
                    .text_color(rgbc(theme.muted))
                    .child(title),
            )
            .child(tabs)
    }

    fn render_ai_config(&self, cx: &mut Context<Self>) -> Div {
        let theme = self.theme();
        let Some(form) = self.ai_config.as_ref() else {
            return div();
        };
        let has_key = self.ai.snapshot().has_key;
        let mcp = self.mcp.snapshot();
        let bridge = std::env::current_exe()
            .ok()
            .and_then(|path| path.parent().map(|dir| dir.to_path_buf()))
            .map(|dir| {
                dir.join(if cfg!(windows) {
                    "xcord-mcp.exe"
                } else {
                    "xcord-mcp"
                })
            })
            .unwrap_or_else(|| "xcord-mcp".into());
        let mcp_config =
            serde_json::to_string_pretty(&serde_json::json!({            "mcpServers": {
                    "xcord": {
                        "command": bridge.to_string_lossy()
                    }
                }
            }))
            .unwrap_or_default();
        let error = form.error.clone();
        let mcp_hint =
            self.tr("MCP speaks JSON-RPC on 127.0.0.1; the bridge reads mcp-token from the config directory.");
        let endpoint = if mcp.running {
            self.tr_args(
                "Endpoint: {url} · token {token}",
                &[("url", mcp.url.clone()), ("token", mask_token(&mcp.token))],
            )
        } else {
            self.tr_args("Endpoint: {url}", &[("url", mcp.url.clone())])
        };

        div()
            .w(px(680.))
            .max_h(px(720.))
            .bg(rgbc(theme.panel_alt))
            .border_1()
            .border_color(rgbc(theme.border))
            .rounded_lg()
            .shadow_lg()
            .p(px(22.))
            .flex()
            .flex_col()
            .gap_3()
            .on_key_down(cx.listener(|this, event: &KeyDownEvent, _, cx| {
                if event.keystroke.key == "escape" {
                    this.ai_config = None;
                    cx.notify();
                } else if event.keystroke.key == "enter" {
                    this.save_ai_config(cx);
                    cx.stop_propagation();
                }
            }))
            .child(
                div()
                    .flex()
                    .items_center()
                    .justify_between()
                    .child(
                        div()
                            .text_size(px(18.))
                            .font_weight(gpui::FontWeight::BOLD)
                            .text_color(rgbc(theme.foreground))
                            .child(self.tr("AI & MCP")),
                    )
                    .child(
                        div()
                            .id("ai-config-close")
                            .text_size(px(18.))
                            .text_color(rgbc(theme.muted))
                            .cursor_pointer()
                            .on_mouse_up(MouseButton::Left, cx.listener(|this, _, _, cx| {
                                this.ai_config = None;
                                cx.notify();
                            }))
                            .child("×"),
                    ),
            )
            .child(
                div()
                    .text_size(px(10.5))
                    .text_color(rgbc(theme.muted))
                    .child(self.tr("Any OpenAI-compatible endpoint works. The key is stored in Windows Credential Manager.")),
            )
            .child(
                div()
                    .flex()
                    .gap_2()
                    .child(
                        secondary_button("DeepSeek", theme).on_mouse_up(
                            MouseButton::Left,
                            cx.listener(|this, _, _, cx| {
                                if let Some(form) = &this.ai_config {
                                    form.base_url.update(cx, |input, cx| {
                                        input.set_value("https://api.deepseek.com/v1", cx)
                                    });
                                    form.model.update(cx, |input, cx| {
                                        input.set_value("deepseek-chat", cx)
                                    });
                                }
                                cx.notify();
                            }),
                        ),
                    )
                    .child(
                        secondary_button("OpenAI", theme).on_mouse_up(
                            MouseButton::Left,
                            cx.listener(|this, _, _, cx| {
                                if let Some(form) = &this.ai_config {
                                    form.base_url.update(cx, |input, cx| {
                                        input.set_value("https://api.openai.com/v1", cx)
                                    });
                                    form.model.update(cx, |input, cx| {
                                        input.set_value("gpt-4o-mini", cx)
                                    });
                                }
                                cx.notify();
                            }),
                        ),
                    )
                    .child(
                        secondary_button("Ollama", theme).on_mouse_up(
                            MouseButton::Left,
                            cx.listener(|this, _, _, cx| {
                                if let Some(form) = &this.ai_config {
                                    form.base_url.update(cx, |input, cx| {
                                        input.set_value("http://127.0.0.1:11434/v1", cx)
                                    });
                                    form.model.update(cx, |input, cx| {
                                        input.set_value("qwen2.5-coder:7b", cx)
                                    });
                                }
                                cx.notify();
                            }),
                        ),
                    ),
            )
            .child(field(self.tr("Base URL"), form.base_url.clone(), theme))
            .child(field(self.tr("Model"), form.model.clone(), theme))
            .child(field(
                self.tr(if has_key {
                    "API key (a key is already stored)"
                } else {
                    "API key"
                }),
                form.api_key.clone(),
                theme,
            ))
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap_4()
                    .child(
                        checkbox(self.tr("Send terminal context"), form.include_terminal, theme).on_mouse_up(
                            MouseButton::Left,
                            cx.listener(|this, _, _, cx| {
                                if let Some(form) = &mut this.ai_config {
                                    form.include_terminal = !form.include_terminal;
                                }
                                cx.notify();
                            }),
                        ),
                    )
                    .child(
                        checkbox(self.tr("Allow terminal writes"), form.allow_writes, theme).on_mouse_up(
                            MouseButton::Left,
                            cx.listener(|this, _, _, cx| {
                                if let Some(form) = &mut this.ai_config {
                                    form.allow_writes = !form.allow_writes;
                                }
                                cx.notify();
                            }),
                        ),
                    )
                    .child(
                        small_button(self.tr("Clear stored key"), theme).on_mouse_up(
                            MouseButton::Left,
                            cx.listener(|this, _, _, cx| this.clear_ai_key(cx)),
                        ),
                    ),
            )
            .child(
                div()
                    .p_3()
                    .rounded_md()
                    .bg(rgbc(theme.background))
                    .border_1()
                    .border_color(rgbc(theme.border))
                    .flex()
                    .flex_col()
                    .gap_2()
                    .child(
                        div()
                            .flex()
                            .items_center()
                            .justify_between()
                            .child(
                                div()
                                    .text_size(px(10.5))
                                    .text_color(if mcp.running {
                                        rgbc(theme.success)
                                    } else {
                                        rgbc(theme.danger)
                                    })
                                    .child(if mcp.running {
                                        self.tr_args(
                                            "MCP server running · {tools} tools",
                                            &[("tools", mcp.tools.to_string())],
                                        )
                                    } else {
                                        self.tr_args(
                                            "MCP server unavailable: {error}",
                                            &[(
                                                "error",
                                                mcp.error
                                                    .as_deref()
                                                    .unwrap_or("disabled")
                                                    .to_string(),
                                            )],
                                        )
                                    }),
                            )
                            .child(
                                small_button(self.tr("Copy Claude/Cursor config"), theme).on_mouse_up(
                                    MouseButton::Left,
                                    cx.listener({
                                        let config = mcp_config.clone();
                                        move |_, _, _, cx| {
                                            cx.write_to_clipboard(
                                                gpui::ClipboardItem::new_string(config.clone()),
                                            );
                                        }
                                    }),
                                ),
                            ),
                    )
                    .child(
                        div()
                            .text_size(px(9.5))
                            .text_color(rgbc(theme.muted))
                            .child(endpoint),
                    )
                    .child(
                        div()
                            .font(gpui::font("Consolas"))
                            .text_size(px(9.5))
                            .text_color(rgbc(theme.muted))
                            .child(mcp_config.replace('\n', " ")),
                    ),
            )
            .child(
                div()
                    .flex()
                    .items_center()
                    .justify_between()
                    .child(
                        div()
                            .text_size(px(10.))
                            .text_color(if error.is_some() {
                                rgbc(theme.danger)
                            } else {
                                rgbc(theme.muted)
                            })
                            .child(error.unwrap_or(mcp_hint)),
                    )
                    .child(
                        div()
                            .flex()
                            .gap_2()
                            .child(secondary_button(self.tr("Cancel"), theme).on_mouse_up(
                                MouseButton::Left,
                                cx.listener(|this, _, _, cx| {
                                    this.ai_config = None;
                                    cx.notify();
                                }),
                            ))
                            .child(primary_button(self.tr("Save"), theme).on_mouse_up(
                                MouseButton::Left,
                                cx.listener(|this, _, _, cx| this.save_ai_config(cx)),
                            )),
                    ),
            )
    }

    fn open_palette(&mut self, cx: &mut Context<Self>) {
        let theme = self.theme.clone();
        let input = Self::make_input(&theme, &self.tr("Type a command…"), false, cx);
        self.palette = Some(PaletteState { input, selected: 0 });
        self.pending_focus = Some(PendingFocus::Palette);
        cx.notify();
    }

    fn filtered_commands(&self, cx: &Context<Self>) -> Vec<Command> {
        let Some(palette) = &self.palette else {
            return Vec::new();
        };
        let query = palette.input.read(cx).value().trim().to_lowercase();
        self.commands
            .list()
            .into_iter()
            .filter(|command| {
                query.is_empty()
                    || command.title.to_lowercase().contains(&query)
                    || self.command_title(command).to_lowercase().contains(&query)
                    || command.category.to_lowercase().contains(&query)
                    || self
                        .command_category(command)
                        .to_lowercase()
                        .contains(&query)
                    || command.id.to_lowercase().contains(&query)
            })
            .collect()
    }

    fn execute_palette(&mut self, cx: &mut Context<Self>) {
        let commands = self.filtered_commands(cx);
        let index = self
            .palette
            .as_ref()
            .map(|palette| palette.selected)
            .unwrap_or(0)
            .min(commands.len().saturating_sub(1));
        if let Some(command) = commands.get(index) {
            let id = command.id;
            self.palette = None;
            self.commands.run(id, &self.ctx);
            self.sync(cx);
        } else {
            self.palette = None;
        }
        cx.notify();
    }

    fn refresh_plugins(&mut self) {
        self.plugin_infos = self.kernel.plugins();
    }

    fn reload_highlighter(&mut self, cx: &mut Context<Self>) {
        let Some(id) = self.kernel.plugin_id_by_name("highlights") else {
            return;
        };
        match self.kernel.reload(id) {
            Ok(()) => {
                if let Ok(highlights) = self.ctx.require::<Highlights>(services::HIGHLIGHTS) {
                    let highlights = (*highlights).clone();
                    self.highlights = highlights.clone();
                    self.terminal.update(cx, |view, cx| {
                        view.set_highlights(highlights, cx);
                    });
                    self.set_notice(NoticeLevel::Success, "Highlighter hot-reloaded.");
                }
            }
            Err(error) => self.set_notice(NoticeLevel::Error, error.to_string()),
        }
        self.refresh_plugins();
        cx.notify();
    }

    fn apply_pending_focus(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(pending) = self.pending_focus.take() else {
            return;
        };
        let handle = match pending {
            PendingFocus::Terminal => Some(self.terminal.read(cx).focus_handle()),
            PendingFocus::Compose => Some(self.compose.read(cx).focus_handle_only()),
            PendingFocus::Palette => self
                .palette
                .as_ref()
                .map(|palette| palette.input.read(cx).focus_handle_only()),
            PendingFocus::DialogName => self
                .session_form
                .as_ref()
                .map(|form| form.name.read(cx).focus_handle_only()),
            PendingFocus::DialogSecret => self
                .session_form
                .as_ref()
                .map(|form| form.secret.read(cx).focus_handle_only()),
            PendingFocus::AiInput => Some(self.ai_input.read(cx).focus_handle_only()),
            PendingFocus::AiBase => self
                .ai_config
                .as_ref()
                .map(|form| form.base_url.read(cx).focus_handle_only()),
        };
        if let Some(handle) = handle {
            window.focus(&handle);
        }
    }

    fn global_key(&mut self, event: &KeyDownEvent, _window: &mut Window, cx: &mut Context<Self>) {
        let keystroke = &event.keystroke;
        let control = keystroke.modifiers.control || keystroke.modifiers.platform;
        let shift = keystroke.modifiers.shift;
        if control && shift {
            match keystroke.key.as_str() {
                "p" => {
                    self.open_palette(cx);
                    cx.stop_propagation();
                }
                "t" => {
                    self.open_session_dialog(None, cx);
                    cx.stop_propagation();
                }
                "w" => {
                    if let Some(id) = self.active {
                        self.workspace.close(id);
                        self.sync(cx);
                        cx.notify();
                    }
                    cx.stop_propagation();
                }
                "e" => {
                    self.settings.compose_bar = true;
                    self.pending_focus = Some(PendingFocus::Compose);
                    cx.notify();
                    cx.stop_propagation();
                }
                "i" => {
                    self.ai_open = !self.ai_open;
                    if self.ai_open {
                        self.pending_focus = Some(PendingFocus::AiInput);
                    }
                    cx.notify();
                    cx.stop_propagation();
                }
                _ => {}
            }
        }
        if keystroke.key == "escape" {
            if self.palette.is_some() {
                self.palette = None;
                cx.notify();
                cx.stop_propagation();
            } else if self.session_form.is_some() {
                self.close_session_dialog(cx);
                cx.stop_propagation();
            } else if self.settings_form.is_some() {
                self.settings_form = None;
                cx.notify();
                cx.stop_propagation();
            } else if self.plugins_panel {
                self.plugins_panel = false;
                cx.notify();
                cx.stop_propagation();
            }
        }
    }

    fn theme(&self) -> &Theme {
        &self.theme
    }

    fn render_header(&self, cx: &mut Context<Self>) -> Div {
        let theme = self.theme();
        div()
            .h(px(46.))
            .flex()
            .items_center()
            .gap_3()
            .px_4()
            .bg(rgbc(theme.panel))
            .border_b_1()
            .border_color(rgbc(theme.border))
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap_2()
                    .child(
                        div()
                            .text_size(px(16.))
                            .font_weight(gpui::FontWeight::BOLD)
                            .text_color(rgbc(theme.accent))
                            .child("XCORD"),
                    )
                    .child(
                        div()
                            .text_size(px(11.))
                            .text_color(rgbc(theme.muted))
                            .child(self.tr("plugin-first SSH")),
                    ),
            )
            .child(div().flex_1())
            .child(self.header_button(self.tr("AI"), cx, |this, cx| {
                this.ai_open = !this.ai_open;
                if this.ai_open {
                    this.pending_focus = Some(PendingFocus::AiInput);
                }
                cx.notify();
            }))
            .child(self.header_button(self.tr("Palette"), cx, |this, cx| this.open_palette(cx)))
            .child(self.header_button(self.tr("Plugins"), cx, |this, cx| {
                this.plugins_panel = true;
                this.refresh_plugins();
                cx.notify();
            }))
            .child(self.header_button(self.tr("Settings"), cx, |this, cx| this.open_settings(cx)))
    }

    fn header_button(
        &self,
        label: impl Into<SharedString>,
        cx: &mut Context<Self>,
        handler: impl Fn(&mut Self, &mut Context<Self>) + 'static,
    ) -> Div {
        let label = label.into();
        let theme = self.theme();
        let hover_bg = rgbc(theme.panel_alt);
        div()
            .px_3()
            .py_1()
            .rounded_md()
            .text_size(px(11.))
            .text_color(rgbc(theme.muted))
            .cursor_pointer()
            .hover(move |style| style.bg(hover_bg))
            .on_mouse_up(
                MouseButton::Left,
                cx.listener(move |this, _, _, cx| {
                    handler(this, cx);
                }),
            )
            .child(label)
    }

    fn render_sidebar(&self, cx: &mut Context<Self>) -> Div {
        let theme = self.theme();
        let sessions = self.filtered_sessions(cx);
        let mut groups: BTreeMap<String, Vec<SessionProfile>> = BTreeMap::new();
        for profile in sessions {
            let group = if profile.group.trim().is_empty() {
                "Sessions".to_string()
            } else {
                profile.group.clone()
            };
            groups.entry(group).or_default().push(profile);
        }

        let mut list = div()
            .id("session-list")
            .flex()
            .flex_col()
            .gap_1()
            .flex_1()
            .overflow_y_scroll()
            .px_2()
            .pb_2();
        if groups.is_empty() {
            list = list.child(
                div()
                    .p_3()
                    .text_size(px(11.5))
                    .text_color(rgbc(theme.muted))
                    .child(self.tr("No sessions match. Create one to get started.")),
            );
        }
        for (group, profiles) in groups {
            list = list.child(
                div()
                    .px_2()
                    .pt_3()
                    .pb_1()
                    .text_size(px(9.5))
                    .font_weight(gpui::FontWeight::BOLD)
                    .text_color(rgbc(theme.muted))
                    .child(self.tr(&group).to_uppercase()),
            );
            for profile in profiles {
                list = list.child(self.render_session_row(profile, cx));
            }
        }

        div()
            .w(px(self.settings.sidebar_width.max(200.0)))
            .h_full()
            .flex()
            .flex_col()
            .bg(rgbc(theme.panel))
            .border_r_1()
            .border_color(rgbc(theme.border))
            .child(
                div()
                    .p_3()
                    .flex()
                    .flex_col()
                    .gap_2()
                    .child(
                        div()
                            .h(px(34.))
                            .px(px(10.))
                            .flex()
                            .items_center()
                            .gap_2()
                            .bg(rgbc(theme.panel_alt))
                            .border_1()
                            .border_color(rgbc(theme.border))
                            .rounded_md()
                            .on_key_down(cx.listener(|this, event: &KeyDownEvent, window, cx| {
                                if event.keystroke.key == "enter" {
                                    this.connect_first_filtered(window, cx);
                                    cx.stop_propagation();
                                }
                            }))
                            .child(self.search.clone()),
                    )
                    .child(primary_button(self.tr("New session"), theme).on_mouse_up(
                        MouseButton::Left,
                        cx.listener(|this, _, _, cx| this.open_session_dialog(None, cx)),
                    )),
            )
            .child(list)
            .child(
                div()
                    .p_2()
                    .border_t_1()
                    .border_color(rgbc(theme.border))
                    .flex()
                    .gap_2()
                    .child(
                        small_button(self.tr("Plugins"), theme)
                            .flex_1()
                            .on_mouse_up(
                                MouseButton::Left,
                                cx.listener(|this, _, _, cx| {
                                    this.plugins_panel = true;
                                    this.refresh_plugins();
                                    cx.notify();
                                }),
                            ),
                    )
                    .child(
                        small_button(self.tr("Settings"), theme)
                            .flex_1()
                            .on_mouse_up(
                                MouseButton::Left,
                                cx.listener(|this, _, _, cx| {
                                    this.open_settings(cx);
                                }),
                            ),
                    ),
            )
    }

    fn render_session_row(&self, profile: SessionProfile, cx: &mut Context<Self>) -> Div {
        let theme = self.theme();
        let tab = self.tabs.iter().find(|tab| tab.profile_id == profile.id);
        let active = tab.map(|tab| Some(tab.id) == self.active).unwrap_or(false);
        let status_color = tab
            .map(|tab| match &tab.status {
                TabStatus::Connected => rgbc(theme.success),
                TabStatus::Connecting => rgbc(theme.accent),
                TabStatus::Failed(_) => rgbc(theme.danger),
                TabStatus::Closed => rgbc(theme.muted),
            })
            .unwrap_or(rgba(theme.muted, 0.35));
        let row_bg = if active {
            rgbc(theme.panel_alt)
        } else {
            rgba(theme.panel_alt, 0.0)
        };
        let hover_bg = rgba(theme.accent, 0.1);
        let id = profile.id.clone();
        let edit_id = profile.id.clone();
        let subtitle = if profile.user.is_empty() {
            profile.host.clone()
        } else {
            format!("{}@{}", profile.user, profile.host)
        };

        div()
            .flex()
            .items_center()
            .gap_1()
            .rounded_md()
            .bg(row_bg)
            .child(
                div()
                    .id(SharedString::from(format!("session-{}", profile.id)))
                    .flex()
                    .flex_1()
                    .min_w(px(0.))
                    .items_center()
                    .gap_2()
                    .px_2()
                    .py_2()
                    .rounded_md()
                    .cursor_pointer()
                    .hover(move |style| style.bg(hover_bg))
                    .on_mouse_up(
                        MouseButton::Left,
                        cx.listener(move |this, _, window, cx| {
                            this.connect_profile(id.clone(), window, cx);
                        }),
                    )
                    .child(div().w(px(7.)).h(px(7.)).rounded_full().bg(status_color))
                    .child(
                        div()
                            .flex()
                            .flex_col()
                            .flex_1()
                            .min_w(px(0.))
                            .child(
                                div()
                                    .text_size(px(12.5))
                                    .text_color(rgbc(theme.foreground))
                                    .overflow_hidden()
                                    .text_ellipsis()
                                    .child(profile.name.clone()),
                            )
                            .child(
                                div()
                                    .text_size(px(10.))
                                    .text_color(rgbc(theme.muted))
                                    .overflow_hidden()
                                    .text_ellipsis()
                                    .child(subtitle),
                            ),
                    ),
            )
            .child(
                div()
                    .id(SharedString::from(format!("edit-{}", edit_id)))
                    .px_2()
                    .py_1()
                    .rounded_md()
                    .text_size(px(10.))
                    .text_color(rgbc(theme.muted))
                    .cursor_pointer()
                    .hover(move |style| style.text_color(rgbc(theme.accent)))
                    .on_mouse_up(
                        MouseButton::Left,
                        cx.listener(move |this, _, _, cx| {
                            this.open_session_dialog(Some(edit_id.clone()), cx);
                        }),
                    )
                    .child(self.tr("edit")),
            )
    }

    fn render_tab_bar(&self, cx: &mut Context<Self>) -> Div {
        let theme = self.theme();
        let mut tabs = div()
            .flex()
            .items_center()
            .gap_1()
            .flex_1()
            .overflow_hidden()
            .px_2();
        for tab in &self.tabs {
            let active = Some(tab.id) == self.active;
            let tab_id = tab.id;
            let dot = match &tab.status {
                TabStatus::Connected => rgbc(theme.success),
                TabStatus::Connecting => rgbc(theme.accent),
                TabStatus::Failed(_) => rgbc(theme.danger),
                TabStatus::Closed => rgbc(theme.muted),
            };
            tabs = tabs.child(
                div()
                    .id(SharedString::from(format!("tab-{}", tab.id)))
                    .h(px(34.))
                    .flex()
                    .items_center()
                    .gap_2()
                    .px_3()
                    .rounded_md()
                    .bg(if active {
                        rgbc(theme.background)
                    } else {
                        rgba(theme.panel_alt, 0.0)
                    })
                    .border_1()
                    .border_color(if active {
                        rgbc(theme.border)
                    } else {
                        rgba(theme.border, 0.0)
                    })
                    .cursor_pointer()
                    .hover(|style| style.bg(rgbc(theme.panel_alt)))
                    .on_mouse_up(
                        MouseButton::Left,
                        cx.listener(move |this, _, window, cx| {
                            this.workspace.select(tab_id);
                            this.pending_focus = Some(PendingFocus::Terminal);
                            let _ = window;
                            this.sync(cx);
                            cx.notify();
                        }),
                    )
                    .child(div().w(px(6.)).h(px(6.)).rounded_full().bg(dot))
                    .child(
                        div()
                            .max_w(px(180.))
                            .text_size(px(12.))
                            .text_color(if active {
                                rgbc(theme.foreground)
                            } else {
                                rgbc(theme.muted)
                            })
                            .overflow_hidden()
                            .text_ellipsis()
                            .child(tab.title.clone()),
                    )
                    .child(
                        div()
                            .id(SharedString::from(format!("close-{}", tab.id)))
                            .px_1()
                            .rounded_sm()
                            .text_size(px(12.))
                            .text_color(rgbc(theme.muted))
                            .cursor_pointer()
                            .hover(|style| style.text_color(rgbc(theme.danger)))
                            .on_mouse_up(
                                MouseButton::Left,
                                cx.listener(move |this, _, _, cx| {
                                    this.workspace.close(tab_id);
                                    this.sync(cx);
                                    cx.notify();
                                }),
                            )
                            .child("×"),
                    ),
            );
        }

        div()
            .h(px(42.))
            .flex()
            .items_center()
            .bg(rgbc(theme.panel))
            .border_b_1()
            .border_color(rgbc(theme.border))
            .child(tabs)
            .child(
                div()
                    .id("new-tab")
                    .px_3()
                    .py_1()
                    .mr_2()
                    .rounded_md()
                    .text_size(px(18.))
                    .text_color(rgbc(theme.muted))
                    .cursor_pointer()
                    .hover(|style| style.text_color(rgbc(theme.accent)))
                    .on_mouse_up(
                        MouseButton::Left,
                        cx.listener(|this, _, _, cx| this.open_session_dialog(None, cx)),
                    )
                    .child("+"),
            )
    }

    fn render_compose(&self, cx: &mut Context<Self>) -> Div {
        let theme = self.theme();
        let snippets = self.sessions.snippets();
        let mut chips = div().flex().items_center().gap_1();
        for snippet in snippets.iter().take(3) {
            let command = snippet.command.clone();
            let enter = snippet.send_enter;
            chips = chips.child(
                div()
                    .id(SharedString::from(format!("snippet-{}", snippet.id)))
                    .px_2()
                    .py_1()
                    .rounded_sm()
                    .border_1()
                    .border_color(rgbc(theme.border))
                    .text_size(px(9.5))
                    .text_color(rgbc(theme.muted))
                    .cursor_pointer()
                    .hover(|style| style.bg(rgbc(theme.panel_alt)))
                    .on_mouse_up(
                        MouseButton::Left,
                        cx.listener(move |this, _, _, cx| {
                            this.send_snippet(&command, enter, cx);
                        }),
                    )
                    .child(snippet.name.clone()),
            );
        }

        div()
            .flex()
            .items_center()
            .gap_2()
            .px_3()
            .py_2()
            .bg(rgbc(theme.panel))
            .border_t_1()
            .border_color(rgbc(theme.border))
            .on_key_down(cx.listener(|this, event: &KeyDownEvent, window, cx| {
                if event.keystroke.key == "enter" {
                    this.send_compose(window, cx);
                    cx.stop_propagation();
                }
            }))
            .child(
                div()
                    .flex_1()
                    .h(px(34.))
                    .bg(rgbc(theme.panel_alt))
                    .border_1()
                    .border_color(rgbc(theme.border))
                    .rounded_md()
                    .child(self.compose.clone()),
            )
            .child(primary_button("Send", theme).on_mouse_up(
                MouseButton::Left,
                cx.listener(|this, _, window, cx| this.send_compose(window, cx)),
            ))
            .child(chips)
    }

    fn render_status(&self, _cx: &mut Context<Self>) -> Div {
        let theme = self.theme();
        let action = |this: &Self| {
            if let Some(tab) = this
                .active
                .and_then(|id| this.tabs.iter().find(|tab| tab.id == id))
            {
                div()
                    .flex()
                    .items_center()
                    .gap_3()
                    .text_size(px(10.5))
                    .text_color(rgbc(theme.muted))
                    .child(match &tab.status {
                        TabStatus::Connected => self.status_label("connected"),
                        TabStatus::Connecting => self.status_label("connecting"),
                        TabStatus::Closed => self.status_label("closed"),
                        TabStatus::Failed(_) => self.status_label("failed"),
                    })
                    .child(format!("{}@{}", tab.user, tab.host))
                    .child(
                        this.workspace
                            .dimensions(tab.id)
                            .map(|(cols, rows)| format!("{cols}×{rows}"))
                            .unwrap_or_default(),
                    )
                    .child(
                        tab.host_key
                            .as_ref()
                            .map(|key| {
                                let fingerprint = key.fingerprint.clone();
                                let short =
                                    fingerprint.strip_prefix("SHA256:").unwrap_or(&fingerprint);
                                format!("SHA256:{}", &short[..short.len().min(22)])
                            })
                            .unwrap_or_else(|| self.tr("no host key")),
                    )
            } else {
                div()
                    .flex()
                    .items_center()
                    .gap_2()
                    .text_size(px(10.5))
                    .text_color(rgbc(theme.muted))
                    .child(self.tr("ready"))
                    .child(self.tr_args(
                        "{count} plugins active",
                        &[("count", this.kernel.active_plugins().len().to_string())],
                    ))
            }
        };

        div()
            .h(px(24.))
            .flex()
            .items_center()
            .justify_between()
            .px_3()
            .bg(rgbc(theme.panel))
            .border_t_1()
            .border_color(rgbc(theme.border))
            .child(action(self))
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap_3()
                    .text_size(px(10.5))
                    .text_color(rgbc(theme.muted))
                    .child("UTF-8")
                    .child("xterm-256color")
                    .child(self.tr("Ctrl+Shift+P palette")),
            )
    }

    fn render_welcome(&self, cx: &mut Context<Self>) -> Div {
        let theme = self.theme();
        let recent = self.sessions.list().into_iter().take(4).collect::<Vec<_>>();
        let mut recent_list = div().flex().flex_col().gap_2().w(px(420.));
        for profile in recent {
            let id = profile.id.clone();
            recent_list = recent_list.child(
                div()
                    .id(SharedString::from(format!("recent-{}", profile.id)))
                    .flex()
                    .items_center()
                    .justify_between()
                    .px_3()
                    .py_2()
                    .rounded_md()
                    .bg(rgbc(theme.panel_alt))
                    .border_1()
                    .border_color(rgbc(theme.border))
                    .cursor_pointer()
                    .hover(|style| style.border_color(rgbc(theme.accent)))
                    .on_mouse_up(
                        MouseButton::Left,
                        cx.listener(move |this, _, window, cx| {
                            this.connect_profile(id.clone(), window, cx);
                        }),
                    )
                    .child(
                        div()
                            .flex()
                            .flex_col()
                            .child(
                                div()
                                    .text_size(px(12.5))
                                    .text_color(rgbc(theme.foreground))
                                    .child(profile.name.clone()),
                            )
                            .child(
                                div()
                                    .text_size(px(10.))
                                    .text_color(rgbc(theme.muted))
                                    .child(profile.endpoint()),
                            ),
                    )
                    .child(
                        div()
                            .text_size(px(10.))
                            .text_color(rgbc(theme.accent))
                            .child(self.tr("connect")),
                    ),
            );
        }

        div()
            .flex_1()
            .min_h(px(0.))
            .flex()
            .flex_col()
            .items_center()
            .justify_center()
            .gap_5()
            .bg(rgbc(theme.background))
            .child(
                div()
                    .text_size(px(46.))
                    .font_weight(gpui::FontWeight::BOLD)
                    .text_color(rgbc(theme.accent))
                    .child("XCORD"),
            )
            .child(
                div()
                    .text_size(px(14.))
                    .text_color(rgbc(theme.muted))
                    .child(self.tr("An SSH workspace where everything is a plugin.")),
            )
            .child(
                div()
                    .flex()
                    .gap_3()
                    .child(primary_button(self.tr("New session"), theme).on_mouse_up(
                        MouseButton::Left,
                        cx.listener(|this, _, _, cx| this.open_session_dialog(None, cx)),
                    ))
                    .child(
                        secondary_button(self.tr("Command palette"), theme).on_mouse_up(
                            MouseButton::Left,
                            cx.listener(|this, _, _, cx| this.open_palette(cx)),
                        ),
                    )
                    .child(
                        secondary_button(self.tr("Manage plugins"), theme).on_mouse_up(
                            MouseButton::Left,
                            cx.listener(|this, _, _, cx| {
                                this.plugins_panel = true;
                                this.refresh_plugins();
                                cx.notify();
                            }),
                        ),
                    ),
            )
            .child(
                div()
                    .text_size(px(10.))
                    .text_color(rgbc(theme.muted))
                    .child(
                        self.tr(
                            "SSH • terminal • compose • snippets • highlight • logging • themes",
                        ),
                    ),
            )
            .child(recent_list)
    }

    fn render_main(&self, cx: &mut Context<Self>) -> Div {
        let theme = self.theme();
        let active = self.active;
        let mut main = div()
            .flex_1()
            .min_w(px(0.))
            .h_full()
            .flex()
            .flex_col()
            .bg(rgbc(theme.background));
        main = main.child(self.render_tab_bar(cx));
        if active.is_some() {
            main = main.child(div().flex_1().min_h(px(0.)).child(self.terminal.clone()));
            if self.settings.compose_bar {
                main = main.child(self.render_compose(cx));
            }
            main = main.child(self.render_status(cx));
        } else {
            main = main.child(self.render_welcome(cx));
            main = main.child(self.render_status(cx));
        }
        main
    }

    fn render_session_dialog(&self, cx: &mut Context<Self>) -> Div {
        let theme = self.theme();
        let Some(form) = self.session_form.as_ref() else {
            return div();
        };
        let editing = form.editing.is_some();
        let auth = form.auth;
        let save_secret = form.save_secret;
        let logging = form.logging;
        let error = form.error.clone();
        let (status_text, status_color) = if let Some(error) = error {
            (error, theme.danger)
        } else if self.test_pending {
            (self.tr("Testing connection…"), theme.accent)
        } else if let Some(test) = self.test_result.as_ref() {
            (
                test.message.clone(),
                if test.ok { theme.success } else { theme.danger },
            )
        } else {
            (
                self.tr("Host keys are verified with trust-on-first-use."),
                theme.muted,
            )
        };

        let mut card = div()
            .w(px(640.))
            .bg(rgbc(theme.panel_alt))
            .border_1()
            .border_color(rgbc(theme.border))
            .rounded_lg()
            .shadow_lg()
            .p(px(22.))
            .flex()
            .flex_col()
            .gap_3()
            .on_key_down(cx.listener(|this, event: &KeyDownEvent, window, cx| {
                if event.keystroke.key == "tab" {
                    this.focus_session_next(event.keystroke.modifiers.shift, window, cx);
                    cx.stop_propagation();
                } else if event.keystroke.key == "escape" {
                    this.close_session_dialog(cx);
                    cx.stop_propagation();
                } else if event.keystroke.key == "enter" {
                    if event.keystroke.modifiers.control {
                        this.test_session(cx);
                    } else {
                        this.submit_session(window, cx);
                    }
                    cx.stop_propagation();
                }
            }))
            .child(
                div()
                    .flex()
                    .items_center()
                    .justify_between()
                    .child(
                        div()
                            .text_size(px(18.))
                            .font_weight(gpui::FontWeight::BOLD)
                            .text_color(rgbc(theme.foreground))
                            .child(if editing {
                                self.tr("Edit session")
                            } else {
                                self.tr("New session")
                            }),
                    )
                    .child(
                        div()
                            .id("dialog-close")
                            .px_2()
                            .text_size(px(18.))
                            .text_color(rgbc(theme.muted))
                            .cursor_pointer()
                            .hover(|style| style.text_color(rgbc(theme.danger)))
                            .on_mouse_up(
                                MouseButton::Left,
                                cx.listener(|this, _, _, cx| this.close_session_dialog(cx)),
                            )
                            .child("×"),
                    ),
            )
            .child(
                div()
                    .text_size(px(10.5))
                    .text_color(rgbc(theme.muted))
                    .child(
                        self.tr("Credentials are stored in the OS keychain, never in config.toml."),
                    ),
            )
            .child(
                div()
                    .flex()
                    .gap_3()
                    .child(field(self.tr("Name"), form.name.clone(), theme).flex_1())
                    .child(field(self.tr("Group"), form.group.clone(), theme).flex_1()),
            )
            .child(
                div()
                    .flex()
                    .gap_3()
                    .child(field(self.tr("Host"), form.host.clone(), theme).flex_1())
                    .child(field(self.tr("Port"), form.port.clone(), theme).w(px(100.)))
                    .child(field(self.tr("User"), form.user.clone(), theme).w(px(170.))),
            )
            .child(
                div()
                    .flex()
                    .gap_2()
                    .child(
                        auth_chip(self.tr("Password"), auth == AuthMode::Password, theme)
                            .on_mouse_up(
                                MouseButton::Left,
                                cx.listener(|this, _, _, cx| {
                                    if let Some(form) = &mut this.session_form {
                                        form.auth = AuthMode::Password;
                                    }
                                    cx.notify();
                                }),
                            ),
                    )
                    .child(
                        auth_chip(self.tr("Private key"), auth == AuthMode::PublicKey, theme)
                            .on_mouse_up(
                                MouseButton::Left,
                                cx.listener(|this, _, _, cx| {
                                    if let Some(form) = &mut this.session_form {
                                        form.auth = AuthMode::PublicKey;
                                    }
                                    cx.notify();
                                }),
                            ),
                    ),
            );

        if auth == AuthMode::Password {
            card = card.child(field(self.tr("Password"), form.secret.clone(), theme));
        } else {
            card = card.child(
                div()
                    .flex()
                    .flex_col()
                    .gap_1()
                    .child(
                        div()
                            .text_size(px(10.))
                            .text_color(rgbc(theme.muted))
                            .child(self.tr("Private key file")),
                    )
                    .child(
                        div()
                            .flex()
                            .items_center()
                            .gap_2()
                            .child(
                                div()
                                    .flex_1()
                                    .bg(rgbc(theme.background))
                                    .border_1()
                                    .border_color(rgbc(theme.border))
                                    .rounded_md()
                                    .child(form.key_path.clone()),
                            )
                            .child(secondary_button(self.tr("Browse…"), theme).on_mouse_up(
                                MouseButton::Left,
                                cx.listener(|this, _, window, cx| {
                                    this.pick_private_key(window, cx)
                                }),
                            )),
                    ),
            );
            card = card.child(field(
                self.tr("Key passphrase (optional)"),
                form.secret.clone(),
                theme,
            ));
        }

        card = card
            .child(
                div()
                    .flex()
                    .gap_5()
                    .child(
                        checkbox(
                            self.tr(if auth == AuthMode::Password {
                                "Save password in keychain"
                            } else {
                                "Save passphrase in keychain"
                            }),
                            save_secret,
                            theme,
                        )
                        .on_mouse_up(
                            MouseButton::Left,
                            cx.listener(|this, _, _, cx| {
                                if let Some(form) = &mut this.session_form {
                                    form.save_secret = !form.save_secret;
                                }
                                cx.notify();
                            }),
                        ),
                    )
                    .child(
                        checkbox(self.tr("Log this session"), logging, theme).on_mouse_up(
                            MouseButton::Left,
                            cx.listener(|this, _, _, cx| {
                                if let Some(form) = &mut this.session_form {
                                    form.logging = !form.logging;
                                }
                                cx.notify();
                            }),
                        ),
                    ),
            )
            .child(
                div()
                    .flex()
                    .items_center()
                    .justify_between()
                    .child(
                        div()
                            .text_size(px(10.5))
                            .text_color(rgbc(status_color))
                            .child(status_text),
                    )
                    .child(
                        div()
                            .flex()
                            .gap_2()
                            .child(
                                secondary_button(self.tr("Test connection"), theme).on_mouse_up(
                                    MouseButton::Left,
                                    cx.listener(|this, _, _, cx| this.test_session(cx)),
                                ),
                            )
                            .child(secondary_button(self.tr("Cancel"), theme).on_mouse_up(
                                MouseButton::Left,
                                cx.listener(|this, _, _, cx| this.close_session_dialog(cx)),
                            ))
                            .child(
                                primary_button(self.tr("Save & connect"), theme).on_mouse_up(
                                    MouseButton::Left,
                                    cx.listener(|this, _, window, cx| {
                                        this.submit_session(window, cx)
                                    }),
                                ),
                            ),
                    ),
            );

        card
    }

    fn render_plugins_panel(&self, cx: &mut Context<Self>) -> Div {
        let theme = self.theme();
        let mut rows = div()
            .flex()
            .flex_col()
            .gap_1()
            .max_h(px(380.))
            .id("plugin-rows")
            .overflow_y_scroll();
        for plugin in &self.plugin_infos {
            let color = match plugin.state {
                PluginState::Active => rgbc(theme.success),
                PluginState::Pending | PluginState::Loading => rgbc(theme.accent),
                PluginState::Failed => rgbc(theme.danger),
                _ => rgbc(theme.muted),
            };
            let is_highlighter = plugin.id.to_string() == "highlights";
            rows =
                rows.child(
                    div()
                        .flex()
                        .items_center()
                        .gap_3()
                        .px_3()
                        .py_2()
                        .rounded_md()
                        .bg(rgbc(theme.background))
                        .border_1()
                        .border_color(rgbc(theme.border))
                        .child(div().w(px(7.)).h(px(7.)).rounded_full().bg(color))
                        .child(
                            div()
                                .flex()
                                .flex_col()
                                .flex_1()
                                .min_w(px(0.))
                                .child(
                                    div()
                                        .flex()
                                        .items_center()
                                        .gap_2()
                                        .child(
                                            div()
                                                .text_size(px(12.))
                                                .text_color(rgbc(theme.foreground))
                                                .child(self.plugin_name(
                                                    &plugin.id.to_string(),
                                                    &plugin.name,
                                                )),
                                        )
                                        .child(
                                            div()
                                                .text_size(px(9.5))
                                                .text_color(rgbc(theme.muted))
                                                .child(self.tr_args(
                                                    "v{version} · {state}",
                                                    &[
                                                        ("version", plugin.version.to_string()),
                                                        (
                                                            "state",
                                                            self.status_label(plugin.state.label()),
                                                        ),
                                                    ],
                                                )),
                                        ),
                                )
                                .child(
                                    div()
                                        .text_size(px(10.))
                                        .text_color(rgbc(theme.muted))
                                        .overflow_hidden()
                                        .text_ellipsis()
                                        .child(if let Some(error) = &plugin.error {
                                            error.clone()
                                        } else if plugin.description.is_empty() {
                                            plugin.id.to_string()
                                        } else {
                                            self.plugin_description(
                                                &plugin.id.to_string(),
                                                &plugin.description,
                                            )
                                        }),
                                ),
                        )
                        .child(if is_highlighter {
                            secondary_button(self.tr("Reload"), theme)
                                .on_mouse_up(
                                    MouseButton::Left,
                                    cx.listener(|this, _, _, cx| this.reload_highlighter(cx)),
                                )
                                .into_any_element()
                        } else {
                            div()
                                .text_size(px(9.5))
                                .text_color(rgbc(theme.muted))
                                .child(self.tr("core"))
                                .into_any_element()
                        }),
                );
        }

        let diagnostics = self.kernel.diagnostics();
        let mut log = div()
            .flex()
            .flex_col()
            .gap_1()
            .max_h(px(150.))
            .id("diagnostic-log")
            .overflow_y_scroll();
        for diagnostic in diagnostics.iter().rev().take(12) {
            let color = match diagnostic.level {
                xcord_cordis::DiagnosticLevel::Error => rgbc(theme.danger),
                xcord_cordis::DiagnosticLevel::Warn => rgbc(theme.accent),
                xcord_cordis::DiagnosticLevel::Info => rgbc(theme.muted),
            };
            log = log.child(
                div()
                    .text_size(px(10.))
                    .text_color(color)
                    .child(format!("[{}] {}", diagnostic.level, diagnostic.message)),
            );
        }

        let active = self
            .plugin_infos
            .iter()
            .filter(|p| p.state == PluginState::Active)
            .count();
        div()
            .w(px(760.))
            .max_h(px(620.))
            .bg(rgbc(theme.panel_alt))
            .border_1()
            .border_color(rgbc(theme.border))
            .rounded_lg()
            .shadow_lg()
            .p(px(22.))
            .flex()
            .flex_col()
            .gap_3()
            .child(
                div()
                    .flex()
                    .items_center()
                    .justify_between()
                    .child(
                        div()
                            .text_size(px(18.))
                            .font_weight(gpui::FontWeight::BOLD)
                            .text_color(rgbc(theme.foreground))
                            .child(self.tr("Plugins")),
                    )
                    .child(
                        div()
                            .flex()
                            .items_center()
                            .gap_3()
                            .child(
                                div()
                                    .text_size(px(10.5))
                                    .text_color(rgbc(theme.muted))
                                    .child(self.tr_args(
                                        "{active} active / {total} registered",
                                        &[
                                            ("active", active.to_string()),
                                            ("total", self.plugin_infos.len().to_string()),
                                        ],
                                    )),
                            )
                            .child(
                                div()
                                    .id("plugins-close")
                                    .text_size(px(18.))
                                    .text_color(rgbc(theme.muted))
                                    .cursor_pointer()
                                    .hover(|style| style.text_color(rgbc(theme.danger)))
                                    .on_mouse_up(MouseButton::Left, cx.listener(|this, _, _, cx| {
                                        this.plugins_panel = false;
                                        cx.notify();
                                    }))
                                    .child("×"),
                            ),
                    ),
            )
            .child(
                div()
                    .text_size(px(10.5))
                    .text_color(rgbc(theme.muted))
                    .child(self.tr("Everything is a plugin. Unloading a service cascades to its dependents; effects run in reverse order.")),
            )
            .child(rows)
            .child(
                div()
                    .text_size(px(10.5))
                    .text_color(rgbc(theme.muted))
                    .child(self.tr("Kernel diagnostics")),
            )
            .child(log)
            .child(
                div()
                    .flex()
                    .justify_between()
                    .child(
                        div()
                            .text_size(px(10.))
                            .text_color(rgbc(theme.muted))
                            .child(self.tr("Highlighter reload is live. Core plugins restart with the app.")),
                    )
                    .child(
                        secondary_button(self.tr("Reinitialize highlighter"), theme).on_mouse_up(MouseButton::Left, 
                            cx.listener(|this, _, _, cx| this.reload_highlighter(cx)),
                        ),
                    ),
            )
    }

    fn render_settings_panel(&self, cx: &mut Context<Self>) -> Div {
        let theme = self.theme();
        let Some(form) = self.settings_form.as_ref() else {
            return div();
        };
        let settings_hint =
            self.tr("Changes apply live. Some plugin options need a plugin reload.");
        let themes = self.themes.themes();
        let mut cards = div().flex().gap_2();
        for candidate in &themes {
            let selected = candidate.id == form.theme_id;
            let id = candidate.id.to_string();
            cards = cards.child(
                div()
                    .id(SharedString::from(format!("theme-{}", candidate.id)))
                    .flex_1()
                    .p_2()
                    .rounded_md()
                    .bg(rgb(rgb_u32(candidate.background)))
                    .border_2()
                    .border_color(if selected {
                        rgbc(theme.accent)
                    } else {
                        rgbc(theme.border)
                    })
                    .cursor_pointer()
                    .on_mouse_up(
                        MouseButton::Left,
                        cx.listener(move |this, _, _, cx| {
                            if let Some(form) = &mut this.settings_form {
                                form.theme_id = id.clone();
                            }
                            cx.notify();
                        }),
                    )
                    .child(
                        div()
                            .text_size(px(10.5))
                            .text_color(rgb(rgb_u32(candidate.foreground)))
                            .child(candidate.name),
                    ),
            );
        }

        let error = form.error.clone();
        div()
            .w(px(620.))
            .bg(rgbc(theme.panel_alt))
            .border_1()
            .border_color(rgbc(theme.border))
            .rounded_lg()
            .shadow_lg()
            .p(px(22.))
            .flex()
            .flex_col()
            .gap_3()
            .on_key_down(cx.listener(|this, event: &KeyDownEvent, _, cx| {
                if event.keystroke.key == "escape" {
                    this.settings_form = None;
                    cx.notify();
                }
            }))
            .child(
                div()
                    .flex()
                    .items_center()
                    .justify_between()
                    .child(
                        div()
                            .text_size(px(18.))
                            .font_weight(gpui::FontWeight::BOLD)
                            .text_color(rgbc(theme.foreground))
                            .child(self.tr("Settings")),
                    )
                    .child(
                        div()
                            .id("settings-close")
                            .text_size(px(18.))
                            .text_color(rgbc(theme.muted))
                            .cursor_pointer()
                            .hover(|style| style.text_color(rgbc(theme.danger)))
                            .on_mouse_up(
                                MouseButton::Left,
                                cx.listener(|this, _, _, cx| {
                                    this.settings_form = None;
                                    cx.notify();
                                }),
                            )
                            .child("×"),
                    ),
            )
            .child(
                div()
                    .text_size(px(11.))
                    .text_color(rgbc(theme.muted))
                    .child(self.tr("Language")),
            )
            .child(
                div()
                    .flex()
                    .gap_2()
                    .child(
                        auth_chip("中文（简体）", form.language == "zh-CN", theme).on_mouse_up(
                            MouseButton::Left,
                            cx.listener(|this, _, _, cx| {
                                if let Some(form) = &mut this.settings_form {
                                    form.language = "zh-CN".into();
                                }
                                cx.notify();
                            }),
                        ),
                    )
                    .child(
                        auth_chip("English", form.language == "en-US", theme).on_mouse_up(
                            MouseButton::Left,
                            cx.listener(|this, _, _, cx| {
                                if let Some(form) = &mut this.settings_form {
                                    form.language = "en-US".into();
                                }
                                cx.notify();
                            }),
                        ),
                    ),
            )
            .child(
                div()
                    .text_size(px(11.))
                    .text_color(rgbc(theme.muted))
                    .child(self.tr("Theme")),
            )
            .child(cards)
            .child(
                div()
                    .flex()
                    .items_center()
                    .justify_between()
                    .child(
                        div()
                            .text_size(px(11.))
                            .text_color(rgbc(theme.foreground))
                            .child(self.tr("Font size")),
                    )
                    .child(
                        div()
                            .flex()
                            .items_center()
                            .gap_2()
                            .child(small_button("−", theme).on_mouse_up(
                                MouseButton::Left,
                                cx.listener(|this, _, _, cx| {
                                    if let Some(form) = &mut this.settings_form {
                                        form.font_size = (form.font_size - 1.0).clamp(9.0, 28.0);
                                    }
                                    cx.notify();
                                }),
                            ))
                            .child(
                                div()
                                    .w(px(44.))
                                    .text_size(px(12.))
                                    .text_color(rgbc(theme.foreground))
                                    .child(format!("{:.0}", form.font_size)),
                            )
                            .child(small_button("+", theme).on_mouse_up(
                                MouseButton::Left,
                                cx.listener(|this, _, _, cx| {
                                    if let Some(form) = &mut this.settings_form {
                                        form.font_size = (form.font_size + 1.0).clamp(9.0, 28.0);
                                    }
                                    cx.notify();
                                }),
                            )),
                    ),
            )
            .child(
                div()
                    .flex()
                    .gap_4()
                    .child(
                        checkbox(self.tr("Copy on select"), form.copy_on_select, theme)
                            .on_mouse_up(
                                MouseButton::Left,
                                cx.listener(|this, _, _, cx| {
                                    if let Some(form) = &mut this.settings_form {
                                        form.copy_on_select = !form.copy_on_select;
                                    }
                                    cx.notify();
                                }),
                            ),
                    )
                    .child(
                        checkbox(self.tr("Compose bar"), form.compose_bar, theme).on_mouse_up(
                            MouseButton::Left,
                            cx.listener(|this, _, _, cx| {
                                if let Some(form) = &mut this.settings_form {
                                    form.compose_bar = !form.compose_bar;
                                }
                                cx.notify();
                            }),
                        ),
                    )
                    .child(
                        checkbox(self.tr("Strict host keys"), form.strict_host_keys, theme)
                            .on_mouse_up(
                                MouseButton::Left,
                                cx.listener(|this, _, _, cx| {
                                    if let Some(form) = &mut this.settings_form {
                                        form.strict_host_keys = !form.strict_host_keys;
                                    }
                                    cx.notify();
                                }),
                            ),
                    ),
            )
            .child(
                div()
                    .flex()
                    .gap_4()
                    .child(
                        checkbox(self.tr("Session logging"), form.logging, theme).on_mouse_up(
                            MouseButton::Left,
                            cx.listener(|this, _, _, cx| {
                                if let Some(form) = &mut this.settings_form {
                                    form.logging = !form.logging;
                                }
                                cx.notify();
                            }),
                        ),
                    )
                    .child(
                        checkbox(self.tr("Output highlighting"), form.highlight, theme)
                            .on_mouse_up(
                                MouseButton::Left,
                                cx.listener(|this, _, _, cx| {
                                    if let Some(form) = &mut this.settings_form {
                                        form.highlight = !form.highlight;
                                    }
                                    cx.notify();
                                }),
                            ),
                    ),
            )
            .child(
                div()
                    .text_size(px(10.))
                    .text_color(if error.is_some() {
                        rgbc(theme.danger)
                    } else {
                        rgbc(theme.muted)
                    })
                    .child(error.unwrap_or(settings_hint)),
            )
            .child(
                div()
                    .flex()
                    .justify_end()
                    .gap_2()
                    .child(secondary_button(self.tr("Cancel"), theme).on_mouse_up(
                        MouseButton::Left,
                        cx.listener(|this, _, _, cx| {
                            this.settings_form = None;
                            cx.notify();
                        }),
                    ))
                    .child(primary_button(self.tr("Save"), theme).on_mouse_up(
                        MouseButton::Left,
                        cx.listener(|this, _, window, cx| this.save_settings(window, cx)),
                    )),
            )
    }

    fn render_palette(&self, cx: &mut Context<Self>) -> Div {
        let theme = self.theme();
        let Some(palette) = self.palette.as_ref() else {
            return div();
        };
        let commands = self.filtered_commands(cx);
        let selected = palette.selected.min(commands.len().saturating_sub(1));
        let mut rows = div()
            .flex()
            .flex_col()
            .gap_1()
            .max_h(px(360.))
            .id("palette-rows")
            .overflow_y_scroll()
            .p_1();
        for (index, command) in commands.iter().enumerate() {
            let id = command.id;
            let is_selected = index == selected;
            rows = rows.child(
                div()
                    .id(SharedString::from(format!("command-{}", command.id)))
                    .flex()
                    .items_center()
                    .justify_between()
                    .px_3()
                    .py_2()
                    .rounded_md()
                    .bg(if is_selected {
                        rgba(theme.accent, 0.16)
                    } else {
                        rgba(theme.panel_alt, 0.0)
                    })
                    .cursor_pointer()
                    .hover(|style| style.bg(rgba(theme.accent, 0.1)))
                    .on_mouse_up(
                        MouseButton::Left,
                        cx.listener(move |this, _, _, cx| {
                            this.palette = None;
                            this.commands.run(id, &this.ctx);
                            this.sync(cx);
                            cx.notify();
                        }),
                    )
                    .child(
                        div()
                            .flex()
                            .flex_col()
                            .child(
                                div()
                                    .text_size(px(12.))
                                    .text_color(rgbc(theme.foreground))
                                    .child(self.command_title(command)),
                            )
                            .child(
                                div()
                                    .text_size(px(9.5))
                                    .text_color(rgbc(theme.muted))
                                    .child(self.command_category(command)),
                            ),
                    )
                    .child(
                        div()
                            .text_size(px(10.))
                            .text_color(rgbc(theme.muted))
                            .child(command.shortcut.unwrap_or("").to_string()),
                    ),
            );
        }
        if commands.is_empty() {
            rows = rows.child(
                div()
                    .p_4()
                    .text_size(px(11.))
                    .text_color(rgbc(theme.muted))
                    .child(self.tr("No matching commands.")),
            );
        }

        div()
            .w(px(640.))
            .bg(rgbc(theme.panel_alt))
            .border_1()
            .border_color(rgbc(theme.border))
            .rounded_lg()
            .shadow_lg()
            .p(px(12.))
            .flex()
            .flex_col()
            .gap_2()
            .on_key_down(cx.listener(|this, event: &KeyDownEvent, _, cx| {
                match event.keystroke.key.as_str() {
                    "escape" => {
                        this.palette = None;
                        cx.notify();
                        cx.stop_propagation();
                    }
                    "up" => {
                        if let Some(palette) = &mut this.palette {
                            palette.selected = palette.selected.saturating_sub(1);
                        }
                        cx.notify();
                        cx.stop_propagation();
                    }
                    "down" => {
                        if let Some(palette) = &mut this.palette {
                            palette.selected = palette.selected.saturating_add(1);
                        }
                        cx.notify();
                        cx.stop_propagation();
                    }
                    "enter" => {
                        this.execute_palette(cx);
                        cx.stop_propagation();
                    }
                    _ => {}
                }
            }))
            .child(
                div()
                    .bg(rgbc(theme.background))
                    .border_1()
                    .border_color(rgbc(theme.border))
                    .rounded_md()
                    .child(palette.input.clone()),
            )
            .child(rows)
            .child(
                div()
                    .px_2()
                    .text_size(px(9.5))
                    .text_color(rgbc(theme.muted))
                    .child(self.tr("↑↓ navigate · Enter run · Esc close")),
            )
    }

    fn render_notice(&self, cx: &mut Context<Self>) -> gpui::AnyElement {
        let theme = self.theme();
        let Some(notice) = self.notice.as_ref() else {
            return div().into_any_element();
        };
        let color = match notice.level {
            NoticeLevel::Info => theme.accent,
            NoticeLevel::Warning => theme.accent,
            NoticeLevel::Error => theme.danger,
            NoticeLevel::Success => theme.success,
        };
        let right = if self.ai_open { px(418.) } else { px(18.) };
        div()
            .absolute()
            .bottom(px(40.))
            .right(right)
            .w(px(380.))
            .p_3()
            .rounded_md()
            .bg(rgbc(theme.panel_alt))
            .border_1()
            .border_color(rgbc(color))
            .id("notice-card")
            .shadow_lg()
            .cursor_pointer()
            .on_mouse_up(
                MouseButton::Left,
                cx.listener(|this, _, _, cx| {
                    this.notice = None;
                    cx.notify();
                }),
            )
            .child(
                div()
                    .text_size(px(11.5))
                    .text_color(rgbc(theme.foreground))
                    .child(notice.message.clone()),
            )
            .into_any_element()
    }
}

impl Render for XcordApp {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        self.sync(cx);
        self.apply_pending_focus(window, cx);

        if let Some(notice) = &self.notice {
            if notice.at.elapsed() > Duration::from_secs(6) {
                self.notice = None;
            }
        }

        let theme = self.theme().clone();
        let mut root = div()
            .size_full()
            .flex()
            .flex_col()
            .relative()
            .bg(rgbc(theme.background))
            .text_color(rgbc(theme.foreground))
            .font(ui_font())
            .text_size(px(13.))
            .on_key_down(cx.listener(Self::global_key))
            .child(self.render_header(cx));
        let body = div()
            .flex_1()
            .min_h(px(0.))
            .flex()
            .child(self.render_sidebar(cx))
            .child(self.render_main(cx))
            .when(self.ai_open, |body| body.child(self.render_ai_panel(cx)));
        let content = if self.session_form.is_some() {
            // Keep dialogs in normal layout flow. Absolute overlays in this
            // GPUI version paint correctly but their children miss mouse
            // hit-testing, which made fields and Test/Save buttons dead.
            modal_shell(self.render_session_dialog(cx))
        } else if self.settings_form.is_some() {
            modal_shell(self.render_settings_panel(cx))
        } else if self.ai_config.is_some() {
            modal_shell(self.render_ai_config(cx))
        } else if self.plugins_panel {
            modal_shell(self.render_plugins_panel(cx))
        } else if self.palette.is_some() {
            palette_shell(self.render_palette(cx))
        } else {
            body
        };
        root = root.child(content);
        root = root.child(self.render_notice(cx));
        root
    }
}

impl Drop for XcordApp {
    fn drop(&mut self) {
        self.kernel.shutdown();
    }
}

// ---------------------------------------------------------------------------
// Small style helpers

fn modal_shell(content: Div) -> Div {
    div()
        .flex_1()
        .min_h(px(0.))
        .flex()
        .justify_center()
        .items_center()
        .bg(rgba([0, 0, 0], 0.55))
        .child(content)
}

fn palette_shell(content: Div) -> Div {
    div()
        .flex_1()
        .min_h(px(0.))
        .flex()
        .justify_center()
        .items_start()
        .pt(px(90.))
        .bg(rgba([0, 0, 0], 0.35))
        .child(content)
}

fn render_ai_message(message: &AiMessageView, theme: &Theme) -> Div {
    let is_tool = message.role.starts_with("Tool");
    let mut card = div().w_full().flex().flex_col().gap_1();
    if is_tool {
        card = card
            .p_2()
            .rounded_md()
            .bg(rgbc(theme.panel_alt))
            .border_1()
            .border_color(rgbc(theme.border));
    }
    let role = match (&message.tool, message.role.as_str()) {
        (Some(tool), _) => format!("{} · {tool}", message.role),
        _ => message.role.clone(),
    };
    card.child(
        div()
            .text_size(px(9.5))
            .text_color(if message.role == "You" {
                rgbc(theme.accent)
            } else if message.role == "AI" {
                rgbc(theme.success)
            } else {
                rgbc(theme.muted)
            })
            .child(role),
    )
    .child(text_block(
        &message.text,
        if is_tool {
            rgbc(theme.muted)
        } else {
            rgbc(theme.foreground)
        },
        if is_tool { 10.0 } else { 12.0 },
    ))
}

fn text_block(text: &str, color: Rgba, size: f32) -> Div {
    let mut column = div().w_full().flex().flex_col().gap_1();
    for line in text.lines() {
        column = column.child(
            div()
                .min_h(px(15.))
                .text_size(px(size))
                .text_color(color)
                .child(if line.is_empty() {
                    " ".to_string()
                } else {
                    line.to_string()
                }),
        );
    }
    column
}

fn mask_token(token: &str) -> String {
    if token.len() <= 10 {
        return "…".into();
    }
    format!("{}…{}", &token[..6], &token[token.len() - 4..])
}

fn ui_font() -> Font {
    if cfg!(windows) {
        gpui::font("Segoe UI")
    } else if cfg!(target_os = "macos") {
        gpui::font("SF Pro Text")
    } else {
        gpui::font("Inter")
    }
}

fn rgbc(color: [u8; 3]) -> Rgba {
    rgb(rgb_u32(color))
}

fn rgb_u32(color: [u8; 3]) -> u32 {
    ((color[0] as u32) << 16) | ((color[1] as u32) << 8) | color[2] as u32
}

fn rgba(color: [u8; 3], alpha: f32) -> Rgba {
    gpui::rgba((rgb_u32(color) << 8) | (alpha.clamp(0.0, 1.0) * 255.0) as u32)
}

fn input_colors(theme: &Theme) -> (Hsla, Hsla, Hsla, Hsla) {
    (
        rgbc(theme.foreground).into(),
        rgba(theme.muted, 0.75).into(),
        rgba(theme.accent, 0.28).into(),
        rgbc(theme.cursor).into(),
    )
}

fn primary_button(label: impl Into<SharedString>, theme: &Theme) -> Stateful<Div> {
    let label = label.into();
    let bg = rgbc(theme.accent);
    let fg = rgbc(theme.background);
    div()
        .id(SharedString::from(format!("primary-{label}")))
        .h(px(32.))
        .px_3()
        .flex()
        .items_center()
        .justify_center()
        .rounded_md()
        .bg(bg)
        .text_color(fg)
        .text_size(px(11.5))
        .font_weight(gpui::FontWeight::MEDIUM)
        .cursor_pointer()
        .hover(|style| style.opacity(0.88))
        .child(label)
}

fn secondary_button(label: impl Into<SharedString>, theme: &Theme) -> Stateful<Div> {
    let label = label.into();
    div()
        .id(SharedString::from(format!("secondary-{label}")))
        .h(px(32.))
        .px_3()
        .flex()
        .items_center()
        .justify_center()
        .rounded_md()
        .bg(rgbc(theme.panel_alt))
        .border_1()
        .border_color(rgbc(theme.border))
        .text_color(rgbc(theme.foreground))
        .text_size(px(11.5))
        .cursor_pointer()
        .hover(|style| style.bg(rgbc(theme.background)))
        .child(label)
}

fn small_button(label: impl Into<SharedString>, theme: &Theme) -> Stateful<Div> {
    let label = label.into();
    div()
        .id(SharedString::from(format!("small-{label}")))
        .h(px(28.))
        .px_2()
        .flex()
        .items_center()
        .justify_center()
        .rounded_md()
        .bg(rgbc(theme.panel_alt))
        .border_1()
        .border_color(rgbc(theme.border))
        .text_color(rgbc(theme.muted))
        .text_size(px(10.5))
        .cursor_pointer()
        .hover(|style| style.text_color(rgbc(theme.foreground)))
        .child(label)
}

fn checkbox(label: impl Into<SharedString>, checked: bool, theme: &Theme) -> Stateful<Div> {
    let label = label.into();
    div()
        .id(SharedString::from(format!("check-{label}")))
        .flex()
        .items_center()
        .gap_2()
        .cursor_pointer()
        .child(
            div()
                .w(px(14.))
                .h(px(14.))
                .rounded_sm()
                .border_1()
                .border_color(if checked {
                    rgbc(theme.accent)
                } else {
                    rgbc(theme.border)
                })
                .bg(if checked {
                    rgbc(theme.accent)
                } else {
                    rgbc(theme.background)
                })
                .flex()
                .items_center()
                .justify_center()
                .text_size(px(10.))
                .text_color(rgbc(theme.background))
                .child(if checked { "✓" } else { "" }),
        )
        .child(
            div()
                .text_size(px(10.5))
                .text_color(rgbc(theme.muted))
                .child(label),
        )
}

fn field(label: impl Into<SharedString>, input: Entity<TextInput>, theme: &Theme) -> Div {
    let label = label.into();
    div()
        .flex()
        .flex_col()
        .gap_1()
        .min_w(px(0.))
        .child(
            div()
                .text_size(px(10.))
                .text_color(rgbc(theme.muted))
                .child(label),
        )
        .child(
            div()
                .bg(rgbc(theme.background))
                .border_1()
                .border_color(rgbc(theme.border))
                .rounded_md()
                .child(input),
        )
}

fn auth_chip(label: impl Into<SharedString>, selected: bool, theme: &Theme) -> Stateful<Div> {
    let label = label.into();
    div()
        .id(SharedString::from(format!("auth-{label}")))
        .h(px(28.))
        .px_3()
        .flex()
        .items_center()
        .rounded_md()
        .bg(if selected {
            rgba(theme.accent, 0.18)
        } else {
            rgbc(theme.background)
        })
        .border_1()
        .border_color(if selected {
            rgbc(theme.accent)
        } else {
            rgbc(theme.border)
        })
        .text_color(if selected {
            rgbc(theme.accent)
        } else {
            rgbc(theme.muted)
        })
        .text_size(px(10.5))
        .cursor_pointer()
        .child(label)
}
