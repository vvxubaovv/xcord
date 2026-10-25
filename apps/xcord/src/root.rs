//! The GPUI shell.
//!
//! The shell owns no protocol or terminal logic. It provides the `ui-host`
//! service, renders the services plugins publish, and forwards user intent as
//! commands. Plugins can be reloaded without touching this file.

use std::cell::{Cell, RefCell};
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::time::{Duration, Instant};

use async_channel::Receiver;
use base64::Engine as _;
use gpui::prelude::*;
use gpui::{
    div, px, rgb, ClipboardItem, Context, Div, DragMoveEvent, Entity, Font, Hsla, KeyDownEvent,
    MouseButton, MouseUpEvent, PathPromptOptions, Render, Rgba, ScrollHandle, SharedString,
    Stateful, Timer, Window,
};
use tokio::sync::broadcast::error::RecvError;
use xcord_cordis::{Context as PluginContext, Kernel, PluginInfo, PluginState};
use xcord_plugin_api::{
    services, Ai, AiMessageView, AiSnapshot, Command, Commands, ConnectionTest, Ftp, FtpEntry,
    FtpEvent, FtpSnapshot, Highlights, I18n, McpStatus, NoticeLevel, OpenRequest, Runtime,
    Sessions, TabInfo, TabStatus, TerminalId, Theme, Themes, Ui, UiCommand, UiHost, Workspace,
    WorkspaceEvent,
};
use xcord_store::{AuthKind, HostKeyPolicy, SessionProfile, Settings, Snippet};

use crate::selectable_text::SelectableText;
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

struct GroupForm {
    name: Entity<TextInput>,
    editing: Option<String>,
    error: Option<String>,
}

#[derive(Clone)]
struct DraggedSession {
    id: String,
    name: String,
}

struct DragHover {
    group: String,
    target: Option<String>,
    after: bool,
}

struct SessionDragPreview {
    name: String,
    theme: Theme,
}

#[derive(Clone)]
struct DraggedGroup {
    name: String,
}

struct GroupDragPreview {
    name: String,
    theme: Theme,
}

#[derive(Clone)]
struct DraggedTab {
    id: TerminalId,
    name: String,
}

#[derive(Clone)]
struct TabGroupState {
    id: String,
    name: String,
}

struct TabGroupForm {
    name: Entity<TextInput>,
    error: Option<String>,
}

struct TabDragPreview {
    name: String,
    theme: Theme,
}

impl Render for TabDragPreview {
    fn render(&mut self, _window: &mut Window, _cx: &mut Context<Self>) -> impl IntoElement {
        div()
            .px_3()
            .py_2()
            .rounded_md()
            .border_1()
            .border_color(rgbc(self.theme.accent))
            .bg(rgbc(self.theme.panel_alt))
            .text_size(px(11.))
            .text_color(rgbc(self.theme.foreground))
            .child(self.name.clone())
    }
}

impl Render for GroupDragPreview {
    fn render(&mut self, _window: &mut Window, _cx: &mut Context<Self>) -> impl IntoElement {
        div()
            .px_3()
            .py_2()
            .rounded_md()
            .border_1()
            .border_color(rgbc(self.theme.accent))
            .bg(rgbc(self.theme.panel_alt))
            .text_size(px(10.))
            .font_weight(gpui::FontWeight::BOLD)
            .text_color(rgbc(self.theme.foreground))
            .child(self.name.clone())
    }
}

impl Render for SessionDragPreview {
    fn render(&mut self, _window: &mut Window, _cx: &mut Context<Self>) -> impl IntoElement {
        div()
            .px_3()
            .py_2()
            .rounded_md()
            .border_1()
            .border_color(rgbc(self.theme.accent))
            .bg(rgbc(self.theme.panel_alt))
            .text_size(px(11.5))
            .text_color(rgbc(self.theme.foreground))
            .child(self.name.clone())
    }
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
    GroupName,
    TabGroupName,
    Compose,
    Palette,
    AiInput,
    AiBase,
}

struct NoticeState {
    level: NoticeLevel,
    message: String,
    at: Instant,
    /// True only for the transient "connecting" status. Kept as a semantic
    /// flag so localized messages still clear when the tab connects.
    connecting: bool,
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
    ftp: Ftp,
    mcp: McpStatus,

    ui_rx: Receiver<UiCommand>,
    terminal: Entity<TerminalView>,
    search: Entity<TextInput>,
    compose: Entity<TextInput>,
    ai_input: Entity<TextInput>,
    ai_scroll: ScrollHandle,
    ai_follow: Cell<bool>,
    ai_jump_target: Cell<Option<usize>>,
    ai_selected_message: Cell<Option<usize>>,
    ai_menu_open: Cell<bool>,
    ai_last_messages: Cell<usize>,
    ai_last_chars: Cell<usize>,
    ai_text_entities: RefCell<Vec<Entity<SelectableText>>>,
    ai_selection_owner: Rc<Cell<Option<usize>>>,

    b64_upload_open: bool,
    b64_upload_source: Option<PathBuf>,
    b64_upload_size: u64,
    b64_upload_temp_input: Entity<TextInput>,
    b64_upload_dest_input: Entity<TextInput>,
    b64_upload_error: Option<String>,
    b64_uploading: bool,
    b64_upload_status: Option<String>,
    b64_upload_cancel: Rc<Cell<bool>>,
    b64_upload_temp_path: Option<String>,
    b64_upload_tab: Option<TerminalId>,

    base64_target: Option<TerminalId>,
    base64_capturing: bool,
    base64_last_output: Option<Instant>,
    base64_started: Option<Instant>,
    base64_settle_generation: Cell<u64>,
    base64_timeout_generation: Cell<u64>,
    base64_error: Option<String>,

    quick_commands_open: bool,
    quick_search: Entity<TextInput>,
    quick_name: Entity<TextInput>,
    quick_command: Entity<TextInput>,
    quick_editing: Option<String>,
    quick_error: Option<String>,

    ftp_open: bool,
    ftp_profile: Option<String>,
    ftp_snapshot: Option<FtpSnapshot>,
    ftp_remote_selected: Option<String>,
    ftp_local_selected: Option<String>,
    ftp_remote_input: Entity<TextInput>,
    ftp_local_input: Entity<TextInput>,
    ftp_new_dir: Entity<TextInput>,

    session_form: Option<SessionForm>,
    session_menu: Option<String>,
    group_menu: Option<String>,
    group_form: Option<GroupForm>,
    tab_groups: RefCell<Vec<TabGroupState>>,
    tab_group_of: RefCell<HashMap<TerminalId, String>>,
    collapsed_tab_groups: RefCell<HashSet<String>>,
    tab_menu: Option<TerminalId>,
    tab_group_pending: Option<TerminalId>,
    tab_group_form: Option<TabGroupForm>,
    next_tab_group: Cell<u64>,
    collapsed_groups: RefCell<HashSet<String>>,
    drag_hover: RefCell<Option<DragHover>>,
    group_drag_hover: RefCell<Option<(String, bool)>>,
    tab_drag_hover: RefCell<Option<(TerminalId, bool)>>,
    last_session_open: RefCell<Option<(String, Instant)>>,
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
        let ftp = (*ctx
            .require::<Ftp>(services::FTP)
            .expect("ftp plugin did not start"))
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
        let ftp_remote_input = Self::make_input(&theme, "/", false, cx);
        let ftp_local_input = Self::make_input(&theme, &i18n.t("app", "Local folder"), false, cx);
        let ftp_new_dir = Self::make_input(&theme, &i18n.t("app", "New folder"), false, cx);
        let b64_upload_temp_input = Self::make_input(&theme, "/tmp", false, cx);
        let b64_upload_dest_input =
            Self::make_input(&theme, &i18n.t("app", "Destination file"), false, cx);
        let quick_search =
            Self::make_input(&theme, &i18n.t("app", "Search quick commands"), false, cx);
        let quick_name = Self::make_input(&theme, &i18n.t("app", "Name"), false, cx);
        let quick_command = Self::make_input(
            &theme,
            &i18n.t("app", "Command (supports \\n, \\t, \\xNN)"),
            false,
            cx,
        );
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
            ftp,
            mcp,
            ui_rx,
            terminal,
            search,
            compose,
            ai_input,
            ai_scroll: ScrollHandle::new(),
            ai_follow: Cell::new(true),
            ai_jump_target: Cell::new(None),
            ai_selected_message: Cell::new(None),
            ai_menu_open: Cell::new(false),
            ai_last_messages: Cell::new(0),
            ai_last_chars: Cell::new(0),
            ai_text_entities: RefCell::new(Vec::new()),
            ai_selection_owner: Rc::new(Cell::new(None)),
            b64_upload_open: false,
            b64_upload_source: None,
            b64_upload_size: 0,
            b64_upload_temp_input,
            b64_upload_dest_input,
            b64_upload_error: None,
            b64_uploading: false,
            b64_upload_status: None,
            b64_upload_cancel: Rc::new(Cell::new(false)),
            b64_upload_temp_path: None,
            b64_upload_tab: None,
            base64_target: None,
            base64_capturing: false,
            base64_last_output: None,
            base64_started: None,
            base64_settle_generation: Cell::new(0),
            base64_timeout_generation: Cell::new(0),
            base64_error: None,
            quick_commands_open: false,
            quick_search,
            quick_name,
            quick_command,
            quick_editing: None,
            quick_error: None,
            ftp_open: false,
            ftp_profile: None,
            ftp_snapshot: None,
            ftp_remote_selected: None,
            ftp_local_selected: None,
            ftp_remote_input,
            ftp_local_input,
            ftp_new_dir,
            session_form: None,
            session_menu: None,
            group_menu: None,
            group_form: None,
            tab_groups: RefCell::new(Vec::new()),
            tab_group_of: RefCell::new(HashMap::new()),
            collapsed_tab_groups: RefCell::new(HashSet::new()),
            tab_menu: None,
            tab_group_pending: None,
            tab_group_form: None,
            next_tab_group: Cell::new(1),
            collapsed_groups: RefCell::new(HashSet::new()),
            drag_hover: RefCell::new(None),
            group_drag_hover: RefCell::new(None),
            tab_drag_hover: RefCell::new(None),
            last_session_open: RefCell::new(None),
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
                app.connect_profile(session_id, window, cx, false);
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

        // SFTP file-transfer notifications.
        let mut ftp_events = self.ftp.subscribe();
        cx.spawn(async move |this, cx| loop {
            match ftp_events.recv().await {
                Ok(event) => {
                    if this
                        .update(cx, |app, cx| app.on_ftp_event(event, cx))
                        .is_err()
                    {
                        break;
                    }
                }
                Err(RecvError::Lagged(_)) => {
                    if this
                        .update(cx, |app, cx| {
                            app.refresh_ftp_snapshot(cx);
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
        let mut counts: HashMap<String, usize> = HashMap::new();
        for tab in &mut self.tabs {
            let count = counts.entry(tab.profile_id.clone()).or_insert(0);
            *count += 1;
            let base = if tab.profile_name.trim().is_empty() {
                tab.title.clone()
            } else {
                tab.profile_name.clone()
            };
            tab.title = if *count == 1 {
                base
            } else {
                format!("{base} ({count})")
            };
        }
        self.active = self.workspace.active();
        if self
            .tab_menu
            .is_some_and(|id| !self.tabs.iter().any(|tab| tab.id == id))
        {
            self.tab_menu = None;
        }
        if self
            .tab_group_pending
            .is_some_and(|id| !self.tabs.iter().any(|tab| tab.id == id))
        {
            self.tab_group_pending = None;
        }
        if self
            .base64_target
            .is_some_and(|id| !self.tabs.iter().any(|tab| tab.id == id))
        {
            self.base64_target = None;
            self.base64_capturing = false;
            self.base64_last_output = None;
            self.base64_started = None;
        }
        {
            let live = self.tabs.iter().map(|tab| tab.id).collect::<HashSet<_>>();
            self.tab_group_of
                .borrow_mut()
                .retain(|id, _| live.contains(id));
        }
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
            if tab.status.is_connected() {
                self.clear_connecting_notice();
            }
        }
        match event {
            WorkspaceEvent::Notice { level, message } => {
                self.set_notice(level, message);
            }
            WorkspaceEvent::Dirty(id) => {
                if self.base64_target == Some(id) && self.base64_capturing {
                    self.base64_last_output = Some(Instant::now());
                    self.schedule_base64_settle(id, 800, cx);
                }
                if self.active == Some(id) {
                    self.terminal.update(cx, |_, cx| cx.notify());
                }
            }
            WorkspaceEvent::CommandSubmitted(id) => {
                self.on_base64_command_submitted(id, cx);
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
            WorkspaceEvent::ZmodemUploadRequested { terminal } => {
                self.pick_zmodem_uploads(terminal, cx);
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
                    self.ai_follow.set(true);
                    self.ai_jump_target.set(None);
                    self.ai_scroll.scroll_to_bottom();
                    self.pending_focus = Some(PendingFocus::AiInput);
                }
            }
            UiCommand::OpenAiSettings => self.open_ai_config(cx),
            UiCommand::ToggleFtp => self.toggle_ftp(cx),
        }
        cx.notify();
    }

    fn set_notice(&mut self, level: NoticeLevel, message: impl Into<String>) {
        self.notice = Some(NoticeState {
            level,
            message: message.into(),
            at: Instant::now(),
            connecting: false,
        });
    }

    fn set_connecting_notice(&mut self, message: impl Into<String>) {
        self.notice = Some(NoticeState {
            level: NoticeLevel::Info,
            message: message.into(),
            at: Instant::now(),
            connecting: true,
        });
        self.clear_connecting_notice();
    }

    /// Drop the transient "connecting" notice once the active tab is connected.
    /// The check is semantic, so localized messages clear too.
    fn clear_connecting_notice(&mut self) {
        let connected = self
            .active
            .and_then(|id| self.tabs.iter().find(|tab| tab.id == id))
            .map(|tab| tab.status.is_connected())
            .unwrap_or(false);
        if connected && self.notice.as_ref().is_some_and(|notice| notice.connecting) {
            self.notice = None;
        }
    }

    fn close_tab_ids(&mut self, ids: Vec<TerminalId>, cx: &mut Context<Self>) {
        for id in ids {
            self.workspace.close(id);
            self.tab_group_of.borrow_mut().remove(&id);
        }
        self.tab_drag_hover.replace(None);
        self.tab_menu = None;
        self.sync(cx);
        cx.notify();
    }

    fn close_tab_side(&mut self, right: bool, cx: &mut Context<Self>) {
        let Some(id) = self.tab_menu else {
            return;
        };
        let Some(index) = self.tabs.iter().position(|tab| tab.id == id) else {
            return;
        };
        let ids = if right {
            self.tabs[index + 1..]
                .iter()
                .map(|tab| tab.id)
                .collect::<Vec<_>>()
        } else {
            self.tabs[..index]
                .iter()
                .map(|tab| tab.id)
                .collect::<Vec<_>>()
        };
        self.close_tab_ids(ids, cx);
    }

    fn close_all_tabs(&mut self, cx: &mut Context<Self>) {
        let ids = self.tabs.iter().map(|tab| tab.id).collect::<Vec<_>>();
        self.close_tab_ids(ids, cx);
    }

    fn open_tab_group_dialog_for_tab(&mut self, tab_id: TerminalId, cx: &mut Context<Self>) {
        self.tab_menu = None;
        self.open_tab_group_dialog(cx);
        self.tab_group_pending = Some(tab_id);
    }

    fn open_tab_group_dialog(&mut self, cx: &mut Context<Self>) {
        self.tab_group_pending = None;
        let theme = self.theme.clone();
        let name = Self::make_input(&theme, &self.tr("Group name"), false, cx);
        self.tab_group_form = Some(TabGroupForm { name, error: None });
        self.pending_focus = Some(PendingFocus::TabGroupName);
        cx.notify();
    }

    fn save_tab_group(&mut self, cx: &mut Context<Self>) {
        let Some(form) = self.tab_group_form.as_ref() else {
            return;
        };
        let name = form.name.read(cx).value().trim().to_string();
        if name.is_empty() {
            let error = self.tr("Group name cannot be empty.");
            if let Some(form) = &mut self.tab_group_form {
                form.error = Some(error);
            }
            cx.notify();
            return;
        }
        if self
            .tab_groups
            .borrow()
            .iter()
            .any(|group| group.name == name)
        {
            let error = self.tr("A group with that name already exists.");
            if let Some(form) = &mut self.tab_group_form {
                form.error = Some(error);
            }
            cx.notify();
            return;
        }
        let index = self.next_tab_group.get();
        self.next_tab_group.set(index + 1);
        let group_id = format!("tab-group-{index}");
        self.tab_groups.borrow_mut().push(TabGroupState {
            id: group_id.clone(),
            name: name.clone(),
        });
        if let Some(tab_id) = self.tab_group_pending.take() {
            self.tab_group_of
                .borrow_mut()
                .insert(tab_id, group_id.clone());
            self.collapsed_tab_groups.borrow_mut().remove(&group_id);
        }
        self.tab_group_form = None;
        self.set_notice(
            NoticeLevel::Info,
            self.tr_args("Tab group created: {name}", &[("name", name)]),
        );
        cx.notify();
    }

    fn render_tab_group_dialog(&self, cx: &mut Context<Self>) -> Div {
        let theme = self.theme();
        let Some(form) = self.tab_group_form.as_ref() else {
            return div();
        };
        div()
            .w(px(360.))
            .p_4()
            .rounded_lg()
            .flex()
            .flex_col()
            .gap_3()
            .bg(rgbc(theme.panel))
            .border_1()
            .border_color(rgbc(theme.border))
            .child(
                div()
                    .text_size(px(15.))
                    .font_weight(gpui::FontWeight::BOLD)
                    .text_color(rgbc(theme.foreground))
                    .child(self.tr("New tab group")),
            )
            .child(
                div()
                    .h(px(34.))
                    .px_2()
                    .flex()
                    .items_center()
                    .rounded_md()
                    .bg(rgbc(theme.panel_alt))
                    .border_1()
                    .border_color(rgbc(theme.border))
                    .on_key_down(cx.listener(|this, event: &KeyDownEvent, _, cx| {
                        if event.keystroke.key == "enter" {
                            this.save_tab_group(cx);
                            cx.stop_propagation();
                        }
                    }))
                    .child(form.name.clone()),
            )
            .when_some(form.error.clone(), |panel, error| {
                panel.child(
                    div()
                        .text_size(px(10.5))
                        .text_color(rgbc(theme.danger))
                        .child(error),
                )
            })
            .child(
                div()
                    .flex()
                    .justify_end()
                    .gap_2()
                    .child(secondary_button(self.tr("Cancel"), theme).on_mouse_up(
                        MouseButton::Left,
                        cx.listener(|this, _, _, cx| {
                            this.tab_group_form = None;
                            this.tab_group_pending = None;
                            cx.notify();
                        }),
                    ))
                    .child(primary_button(self.tr("Create"), theme).on_mouse_up(
                        MouseButton::Left,
                        cx.listener(|this, _, _, cx| this.save_tab_group(cx)),
                    )),
            )
    }

    fn assign_tab_to_group(
        &mut self,
        tab_id: TerminalId,
        group: Option<String>,
        cx: &mut Context<Self>,
    ) {
        match group {
            Some(group) => {
                if !self
                    .tab_groups
                    .borrow()
                    .iter()
                    .any(|state| state.id == group)
                {
                    return;
                }
                self.tab_group_of.borrow_mut().insert(tab_id, group.clone());
                self.collapsed_tab_groups.borrow_mut().remove(&group);
                let last = {
                    let assigned = self.tab_group_of.borrow();
                    self.tabs
                        .iter()
                        .rev()
                        .find(|entry| entry.id != tab_id && assigned.get(&entry.id) == Some(&group))
                        .map(|entry| entry.id)
                };
                if let Some(last) = last {
                    self.workspace.move_tab(tab_id, last, true);
                }
            }
            None => {
                self.tab_group_of.borrow_mut().remove(&tab_id);
            }
        }
        self.sync(cx);
        cx.notify();
    }

    fn toggle_tab_group(&mut self, group: &str, cx: &mut Context<Self>) {
        {
            let mut collapsed = self.collapsed_tab_groups.borrow_mut();
            if !collapsed.remove(group) {
                collapsed.insert(group.to_string());
            }
        }
        cx.notify();
    }

    fn toggle_ftp(&mut self, cx: &mut Context<Self>) {
        if self.ftp_open {
            self.ftp_open = false;
            self.ftp_snapshot = None;
            cx.notify();
            return;
        }
        let Some(profile_id) = self
            .active
            .and_then(|id| self.tabs.iter().find(|tab| tab.id == id))
            .map(|tab| tab.profile_id.clone())
        else {
            self.set_notice(
                NoticeLevel::Warning,
                self.tr("Open a terminal session before using file transfer."),
            );
            return;
        };
        self.ftp_profile = Some(profile_id.clone());
        match self.ftp.connect(&profile_id) {
            Ok(()) => {
                self.ftp_open = true;
                self.ftp_snapshot = self.ftp.snapshot(&profile_id);
                self.refresh_ftp_snapshot(cx);
            }
            Err(error) => self.set_notice(NoticeLevel::Error, error),
        }
        cx.notify();
    }

    fn on_ftp_event(&mut self, event: FtpEvent, cx: &mut Context<Self>) {
        match event {
            FtpEvent::Changed { profile_id } => {
                if self.ftp_open
                    && (profile_id == "*"
                        || self.ftp_profile.as_deref() == Some(profile_id.as_str()))
                {
                    self.refresh_ftp_snapshot(cx);
                }
            }
            FtpEvent::Notice { level, message } => self.set_notice(level, message),
        }
        cx.notify();
    }

    fn refresh_ftp_snapshot(&mut self, cx: &mut Context<Self>) {
        let Some(profile_id) = self.ftp_profile.clone() else {
            return;
        };
        let Some(snapshot) = self.ftp.snapshot(&profile_id) else {
            return;
        };
        self.ftp_snapshot = Some(snapshot.clone());
        self.ftp_remote_input.update(cx, |input, cx| {
            input.set_value(snapshot.remote_path.clone(), cx)
        });
        self.ftp_local_input.update(cx, |input, cx| {
            input.set_value(snapshot.local_path.clone(), cx)
        });
        if self.ftp_remote_selected.as_ref().is_some_and(|selected| {
            !snapshot
                .remote_entries
                .iter()
                .any(|entry| &entry.path == selected)
        }) {
            self.ftp_remote_selected = None;
        }
        if self.ftp_local_selected.as_ref().is_some_and(|selected| {
            !snapshot
                .local_entries
                .iter()
                .any(|entry| &entry.path == selected)
        }) {
            self.ftp_local_selected = None;
        }
    }

    fn open_group_dialog(&mut self, cx: &mut Context<Self>) {
        let theme = self.theme.clone();
        let name = Self::make_input(&theme, &self.tr("Group name"), false, cx);
        self.group_form = Some(GroupForm {
            name,
            editing: None,
            error: None,
        });
        self.pending_focus = Some(PendingFocus::GroupName);
        self.session_menu = None;
        self.group_menu = None;
        cx.notify();
    }

    fn open_rename_group_dialog(&mut self, group: &str, cx: &mut Context<Self>) {
        let theme = self.theme.clone();
        let name = Self::make_input(&theme, &self.tr("Group name"), false, cx);
        name.update(cx, |input, cx| input.set_value(group.to_string(), cx));
        self.group_form = Some(GroupForm {
            name,
            editing: Some(group.to_string()),
            error: None,
        });
        self.pending_focus = Some(PendingFocus::GroupName);
        self.session_menu = None;
        self.group_menu = None;
        cx.notify();
    }

    fn save_group(&mut self, cx: &mut Context<Self>) {
        let Some(form) = self.group_form.as_ref() else {
            return;
        };
        let name = form.name.read(cx).value().trim().to_string();
        let editing = form.editing.clone();
        if name.is_empty() {
            let error = self.tr("Group name cannot be empty.");
            if let Some(form) = &mut self.group_form {
                form.error = Some(error);
            }
            cx.notify();
            return;
        }
        if let Some(old) = editing.clone() {
            if let Err(error) = self.sessions.rename_group(&old, &name) {
                if let Some(form) = &mut self.group_form {
                    form.error = Some(error);
                }
                cx.notify();
                return;
            }
            {
                let mut collapsed = self.collapsed_groups.borrow_mut();
                if collapsed.remove(&old) {
                    collapsed.insert(name.clone());
                }
            }
            self.group_form = None;
            self.sync(cx);
            self.set_notice(
                NoticeLevel::Info,
                self.tr_args("Group renamed to {name}", &[("name", name)]),
            );
            cx.notify();
            return;
        }
        if let Err(error) = self.sessions.create_group(&name) {
            if let Some(form) = &mut self.group_form {
                form.error = Some(error);
            }
            cx.notify();
            return;
        }
        self.group_form = None;
        self.sync(cx);
        self.set_notice(
            NoticeLevel::Info,
            self.tr_args("Group created: {name}", &[("name", name)]),
        );
        cx.notify();
    }

    fn render_group_dialog(&self, cx: &mut Context<Self>) -> Div {
        let theme = self.theme();
        let Some(form) = self.group_form.as_ref() else {
            return div();
        };
        div()
            .w(px(360.))
            .p_4()
            .rounded_lg()
            .flex()
            .flex_col()
            .gap_3()
            .bg(rgbc(theme.panel))
            .border_1()
            .border_color(rgbc(theme.border))
            .child(
                div()
                    .text_size(px(15.))
                    .font_weight(gpui::FontWeight::BOLD)
                    .text_color(rgbc(theme.foreground))
                    .child(self.tr(if form.editing.is_some() {
                        "Rename group"
                    } else {
                        "New group"
                    })),
            )
            .child(
                div()
                    .h(px(34.))
                    .px_2()
                    .flex()
                    .items_center()
                    .rounded_md()
                    .bg(rgbc(theme.panel_alt))
                    .border_1()
                    .border_color(rgbc(theme.border))
                    .on_key_down(cx.listener(|this, event: &KeyDownEvent, _, cx| {
                        if event.keystroke.key == "enter" {
                            this.save_group(cx);
                            cx.stop_propagation();
                        }
                    }))
                    .child(form.name.clone()),
            )
            .when_some(form.error.clone(), |panel, error| {
                panel.child(
                    div()
                        .text_size(px(10.5))
                        .text_color(rgbc(theme.danger))
                        .child(error),
                )
            })
            .child(
                div()
                    .flex()
                    .justify_end()
                    .gap_2()
                    .child(secondary_button(self.tr("Cancel"), theme).on_mouse_up(
                        MouseButton::Left,
                        cx.listener(|this, _, _, cx| {
                            this.group_form = None;
                            cx.notify();
                        }),
                    ))
                    .child(
                        primary_button(
                            self.tr(if form.editing.is_some() {
                                "Save"
                            } else {
                                "Create"
                            }),
                            theme,
                        )
                        .on_mouse_up(
                            MouseButton::Left,
                            cx.listener(|this, _, _, cx| this.save_group(cx)),
                        ),
                    ),
            )
    }

    fn open_session_dialog(&mut self, profile_id: Option<String>, cx: &mut Context<Self>) {
        self.session_menu = None;
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

    fn pick_zmodem_uploads(&mut self, terminal: TerminalId, cx: &mut Context<Self>) {
        let receiver = cx.prompt_for_paths(PathPromptOptions {
            files: true,
            directories: false,
            multiple: true,
            prompt: Some(self.tr("Select files to send with rz").into()),
        });
        cx.spawn(async move |this, cx| {
            let paths = match receiver.await {
                Ok(Ok(Some(paths))) if !paths.is_empty() => paths,
                _ => {
                    let _ = this.update(cx, |this, cx| {
                        this.workspace.zmodem_cancel(terminal);
                        cx.notify();
                    });
                    return;
                }
            };
            let paths = paths
                .into_iter()
                .map(|path| path.to_string_lossy().into_owned())
                .collect::<Vec<_>>();
            let _ = this.update(cx, |this, cx| {
                match this.workspace.zmodem_upload(terminal, paths) {
                    Ok(()) => {
                        let message = this.tr("Sending files to remote rz…");
                        this.set_notice(NoticeLevel::Info, message);
                    }
                    Err(error) => this.set_notice(NoticeLevel::Error, error),
                }
                cx.notify();
            });
        })
        .detach();
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
                self.set_connecting_notice(self.tr_args(
                    "Connecting to {endpoint}…",
                    &[("endpoint", profile.endpoint())],
                ));
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

    fn connect_profile(
        &mut self,
        profile_id: String,
        window: &mut Window,
        cx: &mut Context<Self>,
        force_new: bool,
    ) {
        self.session_menu = None;
        if !force_new {
            if let Some(tab) = self.tabs.iter().find(|tab| tab.profile_id == profile_id) {
                self.workspace.select(tab.id);
                self.pending_focus = Some(PendingFocus::Terminal);
                self.sync(cx);
                cx.notify();
                return;
            }
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
        let opened_id = profile_id.clone();
        match self.workspace.open(OpenRequest {
            profile,
            credential,
            cols: 80,
            rows: 24,
        }) {
            Ok(_) => {
                self.last_session_open
                    .replace(Some((opened_id, Instant::now())));
                self.pending_focus = Some(PendingFocus::Terminal);
                self.sync(cx);
                self.set_connecting_notice(self.tr("Connecting…"));
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
        sessions
    }

    fn connect_first_filtered(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(profile) = self.filtered_sessions(cx).first() {
            let id = profile.id.clone();
            self.connect_profile(id, window, cx, false);
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

    fn open_settings(&mut self, cx: &mut Context<Self>) {
        let theme_id = if self.settings.theme.is_empty() {
            self.themes.active_id().to_string()
        } else {
            self.settings.theme.clone()
        };
        self.settings_form = Some(SettingsForm {
            theme_id,
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

    fn apply_theme(&mut self, id: String, cx: &mut Context<Self>) {
        if let Some(form) = &mut self.settings_form {
            form.theme_id = id.clone();
        }
        if !self.themes.set_active(&id) {
            if let Some(form) = &mut self.settings_form {
                form.error = Some(format!("Unknown theme `{id}`."));
            }
            return;
        }
        let mut settings = self.settings.clone();
        settings.theme = id;
        if let Err(error) = self.sessions.save_settings(settings.clone()) {
            if let Some(form) = &mut self.settings_form {
                form.error = Some(error);
            }
            cx.notify();
            return;
        }
        self.settings = settings.clone();
        self.theme = self.themes.active();
        self.terminal
            .update(cx, |view, cx| view.set_settings(settings, cx));
        if let Some(form) = &mut self.settings_form {
            form.error = None;
        }
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
        self.ai_follow.set(true);
        self.ai_jump_target.set(None);
        self.ai_scroll.scroll_to_bottom();
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
        self.ai_follow.set(true);
        self.ai_jump_target.set(None);
        self.ai_scroll.scroll_to_bottom();
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
        let message_count = snapshot.messages.len();
        let char_count = snapshot
            .messages
            .iter()
            .map(|message| message.text.len())
            .sum::<usize>();
        if message_count == 0 {
            self.ai_follow.set(true);
            self.ai_jump_target.set(None);
        }
        let changed =
            message_count != self.ai_last_messages.get() || char_count != self.ai_last_chars.get();
        self.ai_last_messages.set(message_count);
        self.ai_last_chars.set(char_count);
        if changed && self.ai_follow.get() {
            self.ai_jump_target.set(None);
            self.ai_scroll.scroll_to_bottom();
        }
        let transcript = snapshot
            .messages
            .iter()
            .map(|message| format!("{}:\n{}\n", message.role, message.text))
            .collect::<Vec<_>>()
            .join("\n");
        let mut entities = self.ai_text_entities.borrow_mut();
        while entities.len() < snapshot.messages.len() {
            let index = entities.len();
            let message = &snapshot.messages[index];
            let (color, size) = if message.role.starts_with("Tool") {
                (rgbc(theme.muted), 10.0)
            } else {
                (rgbc(theme.foreground), 12.0)
            };
            let entity = cx.new(|cx| {
                SelectableText::new(
                    index,
                    self.ai_selection_owner.clone(),
                    message.text.clone(),
                    color,
                    size,
                    rgba(theme.accent, 0.28),
                    cx,
                )
            });
            entities.push(entity);
        }
        if entities.len() > snapshot.messages.len() {
            entities.truncate(snapshot.messages.len());
            self.ai_selection_owner.set(None);
        }
        for (index, message) in snapshot.messages.iter().enumerate() {
            let (color, size) = if message.role.starts_with("Tool") {
                (rgbc(theme.muted), 10.0)
            } else {
                (rgbc(theme.foreground), 12.0)
            };
            entities[index].update(cx, |text, cx| {
                text.set_style(color, size, cx);
                text.set_text(message.text.clone(), cx);
            });
        }
        let text_entities = entities.clone();
        drop(entities);
        let mut messages = div()
            .id("ai-messages")
            .flex()
            .flex_col()
            .gap_3()
            .flex_1()
            .track_scroll(&self.ai_scroll)
            .on_scroll_wheel(cx.listener(|this, _event, _, cx| {
                this.ai_follow.set(false);
                cx.notify();
            }))
            .on_mouse_down(
                MouseButton::Right,
                cx.listener(|this, _, _, cx| {
                    this.ai_selected_message.set(None);
                    this.ai_menu_open.set(true);
                    cx.notify();
                }),
            )
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
            let copy_label = self.tr("Copy");
            for (index, message) in snapshot.messages.iter().enumerate() {
                messages = messages.child(render_ai_message(
                    index,
                    message,
                    theme,
                    &copy_label,
                    self.ai_selected_message.get() == Some(index),
                    text_entities[index].clone(),
                    cx,
                ));
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
                            .max_w(px(110.))
                            .overflow_hidden()
                            .text_ellipsis()
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
            .min_w(px(0.))
            .h_full()
            .overflow_hidden()
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
                            .child(small_button(self.tr("Copy chat"), theme).on_mouse_up(
                                MouseButton::Left,
                                cx.listener({
                                    let transcript = transcript.clone();
                                    move |_, _, _, cx| {
                                        cx.write_to_clipboard(ClipboardItem::new_string(
                                            transcript.clone(),
                                        ));
                                    }
                                }),
                            ))
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
            .child(self.render_ai_context_menu(cx))
            .child(
                div()
                    .flex_1()
                    .min_h(px(0.))
                    .flex()
                    .child(messages)
                    .child(self.render_ai_rail(&snapshot, cx)),
            )
            .child(footer)
            .on_key_down(cx.listener(|this, event: &KeyDownEvent, _, cx| {
                let control =
                    event.keystroke.modifiers.control || event.keystroke.modifiers.platform;
                if control && event.keystroke.key == "c" {
                    this.copy_ai_selected(cx);
                    cx.stop_propagation();
                }
            }))
    }

    fn copy_ai_selected(&mut self, cx: &mut Context<Self>) {
        if let Some(index) = self.ai_selected_message.get() {
            let selected_text = self
                .ai_text_entities
                .borrow()
                .get(index)
                .and_then(|entity| entity.read(cx).selected_text());
            if let Some(text) = selected_text {
                if !text.is_empty() {
                    cx.write_to_clipboard(ClipboardItem::new_string(text));
                    self.ai_menu_open.set(false);
                    cx.notify();
                    return;
                }
            }
        }
        let snapshot = self.ai.snapshot();
        let text = match self.ai_selected_message.get() {
            Some(index) => snapshot
                .messages
                .get(index)
                .map(|message| message.text.clone()),
            None => None,
        }
        .unwrap_or_else(|| {
            snapshot
                .messages
                .iter()
                .map(|message| format!("{}:\n{}\n", message.role, message.text))
                .collect::<Vec<_>>()
                .join("\n")
        });
        cx.write_to_clipboard(ClipboardItem::new_string(text));
        self.ai_menu_open.set(false);
        cx.notify();
    }

    fn copy_ai_transcript(&mut self, cx: &mut Context<Self>) {
        let snapshot = self.ai.snapshot();
        let text = snapshot
            .messages
            .iter()
            .map(|message| format!("{}:\n{}\n", message.role, message.text))
            .collect::<Vec<_>>()
            .join("\n");
        cx.write_to_clipboard(ClipboardItem::new_string(text));
        self.ai_menu_open.set(false);
        cx.notify();
    }

    fn render_ai_context_menu(&self, cx: &mut Context<Self>) -> Div {
        if !self.ai_menu_open.get() {
            return div();
        }
        let theme = self.theme();
        let selected = self.ai_selected_message.get();
        let label = if selected.is_some() {
            self.tr("Selected message")
        } else {
            self.tr("Whole conversation")
        };
        let button = |id: &'static str, label: String, theme: &Theme| {
            div()
                .id(id)
                .h(px(26.))
                .px_2()
                .flex()
                .items_center()
                .rounded_md()
                .border_1()
                .border_color(rgbc(theme.border))
                .bg(rgbc(theme.background))
                .text_size(px(10.))
                .text_color(rgbc(theme.foreground))
                .cursor_pointer()
                .hover(|style| style.bg(rgbc(theme.panel)))
                .child(label)
        };
        div()
            .px_3()
            .py_2()
            .flex()
            .items_center()
            .gap_2()
            .bg(rgba(theme.accent, 0.08))
            .border_b_1()
            .border_color(rgbc(theme.border))
            .child(
                div()
                    .text_size(px(10.))
                    .text_color(rgbc(theme.muted))
                    .child(label),
            )
            .child(button("ai-menu-copy", self.tr("Copy"), theme).on_mouse_up(
                MouseButton::Left,
                cx.listener(|this, _, _, cx| this.copy_ai_selected(cx)),
            ))
            .child(
                button("ai-menu-copy-all", self.tr("Copy chat"), theme).on_mouse_up(
                    MouseButton::Left,
                    cx.listener(|this, _, _, cx| this.copy_ai_transcript(cx)),
                ),
            )
            .child(
                button("ai-menu-cancel", self.tr("Cancel"), theme).on_mouse_up(
                    MouseButton::Left,
                    cx.listener(|this, _, _, cx| {
                        this.ai_menu_open.set(false);
                        cx.notify();
                    }),
                ),
            )
    }

    fn render_ai_rail(&self, snapshot: &AiSnapshot, cx: &mut Context<Self>) -> Div {
        let theme = self.theme();
        let user_indices = snapshot
            .messages
            .iter()
            .enumerate()
            .filter(|(_, message)| message.role == "You")
            .map(|(index, _)| index)
            .collect::<Vec<_>>();
        let last_sent = user_indices.last().copied();
        let selected = self.ai_jump_target.get();
        let mut rail = div()
            .w(px(26.))
            .h_full()
            .flex()
            .flex_col()
            .items_center()
            .gap_1()
            .py_2()
            .border_l_1()
            .border_color(rgbc(theme.border));

        if let Some(last) = last_sent {
            rail = rail.child(
                div()
                    .id("ai-jump-last-sent")
                    .w(px(18.))
                    .h(px(18.))
                    .flex()
                    .items_center()
                    .justify_center()
                    .rounded_full()
                    .text_size(px(11.))
                    .text_color(rgbc(theme.accent))
                    .cursor_pointer()
                    .hover(|style| style.bg(rgba(theme.accent, 0.18)))
                    .on_mouse_up(
                        MouseButton::Left,
                        cx.listener(move |this, _, _, cx| {
                            this.ai_follow.set(false);
                            this.ai_jump_target.set(Some(last));
                            this.ai_scroll.scroll_to_top_of_item(last);
                            cx.notify();
                        }),
                    )
                    .child("↑"),
            );
        }

        for index in user_indices {
            let is_selected = selected == Some(index);
            let is_last = Some(index) == last_sent;
            let size = if is_last { 9.0 } else { 7.0 };
            rail = rail.child(
                div()
                    .id(SharedString::from(format!("ai-jump-{index}")))
                    .w(px(size))
                    .h(px(size))
                    .rounded_full()
                    .bg(if is_selected || (selected.is_none() && is_last) {
                        rgbc(theme.accent)
                    } else {
                        rgbc(theme.border)
                    })
                    .cursor_pointer()
                    .hover(|style| style.bg(rgba(theme.accent, 0.8)))
                    .on_mouse_up(
                        MouseButton::Left,
                        cx.listener(move |this, _, _, cx| {
                            this.ai_follow.set(false);
                            this.ai_jump_target.set(Some(index));
                            this.ai_scroll.scroll_to_top_of_item(index);
                            cx.notify();
                        }),
                    ),
            );
        }

        rail.child(
            div()
                .id("ai-jump-latest")
                .mt_auto()
                .w(px(18.))
                .h(px(18.))
                .flex()
                .items_center()
                .justify_center()
                .rounded_full()
                .text_size(px(12.))
                .text_color(if self.ai_follow.get() {
                    rgbc(theme.accent)
                } else {
                    rgbc(theme.muted)
                })
                .cursor_pointer()
                .hover(|style| style.bg(rgba(theme.accent, 0.18)))
                .on_mouse_up(
                    MouseButton::Left,
                    cx.listener(|this, _, _, cx| {
                        this.ai_follow.set(true);
                        this.ai_jump_target.set(None);
                        this.ai_scroll.scroll_to_bottom();
                        cx.notify();
                    }),
                )
                .child("↓"),
        )
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
            let selected = snapshot.context_terminal == Some(id);
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
                    .max_w_full()
                    .overflow_hidden()
                    .text_ellipsis()
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
            PendingFocus::GroupName => self
                .group_form
                .as_ref()
                .map(|form| form.name.read(cx).focus_handle_only()),
            PendingFocus::TabGroupName => self
                .tab_group_form
                .as_ref()
                .map(|form| form.name.read(cx).focus_handle_only()),
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
            } else if self.b64_upload_open {
                self.close_b64_upload(cx);
                cx.stop_propagation();
            } else if self.base64_error.is_some() {
                self.base64_error = None;
                cx.notify();
                cx.stop_propagation();
            } else if self.tab_menu.is_some() {
                self.tab_menu = None;
                cx.notify();
                cx.stop_propagation();
            } else if self.quick_commands_open {
                self.quick_commands_open = false;
                cx.notify();
                cx.stop_propagation();
            } else if self.ftp_open {
                self.ftp_open = false;
                self.ftp_snapshot = None;
                cx.notify();
                cx.stop_propagation();
            } else if self.tab_group_form.is_some() {
                self.tab_group_form = None;
                cx.notify();
                cx.stop_propagation();
            } else if self.group_form.is_some() {
                self.group_form = None;
                cx.notify();
                cx.stop_propagation();
            } else if self.session_form.is_some() {
                self.close_session_dialog(cx);
                cx.stop_propagation();
            } else if self.settings_form.is_some() {
                self.settings_form = None;
                cx.notify();
                cx.stop_propagation();
            } else if self.session_menu.is_some() {
                self.session_menu = None;
                cx.notify();
                cx.stop_propagation();
            } else if self.group_menu.is_some() {
                self.group_menu = None;
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
                    this.ai_follow.set(true);
                    this.ai_jump_target.set(None);
                    this.ai_scroll.scroll_to_bottom();
                    this.pending_focus = Some(PendingFocus::AiInput);
                }
                cx.notify();
            }))
            .child(self.header_button(self.tr("Files"), cx, |this, cx| this.toggle_ftp(cx)))
            .child(self.header_button(self.tr("Base64 send"), cx, |this, cx| {
                this.open_b64_upload(cx)
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
        let query = self.search.read(cx).value();
        let search_active = !query.trim().is_empty();
        let mut order = self.sessions.groups();
        if !order.iter().any(|group| group == "Sessions") {
            order.push("Sessions".to_string());
        }
        for profile in &sessions {
            let group = if profile.group.trim().is_empty() {
                "Sessions".to_string()
            } else {
                profile.group.clone()
            };
            if !order.iter().any(|existing| existing == &group) {
                order.push(group);
            }
        }
        let mut groups: Vec<(String, Vec<SessionProfile>)> =
            order.into_iter().map(|group| (group, Vec::new())).collect();
        for profile in sessions {
            let group = if profile.group.trim().is_empty() {
                "Sessions".to_string()
            } else {
                profile.group.clone()
            };
            if let Some((_, profiles)) = groups.iter_mut().find(|(name, _)| name == &group) {
                profiles.push(profile);
            }
        }
        if search_active {
            groups.retain(|(_, profiles)| !profiles.is_empty());
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
            let group_drag = group.clone();
            let group_drop = group.clone();
            let group_session_drop = group.clone();
            let group_session_hover = group.clone();
            let group_toggle = group.clone();
            let group_menu_target = group.clone();
            let collapsed = self.collapsed_groups.borrow().contains(&group);
            let header_hover = self
                .drag_hover
                .borrow()
                .as_ref()
                .is_some_and(|hover| hover.target.is_none() && hover.group == group);
            let group_hover = self
                .group_drag_hover
                .borrow()
                .as_ref()
                .is_some_and(|(target, _)| target == &group);
            let group_drag_theme = theme.clone();
            list = list.child(
                div()
                    .id(SharedString::from(format!("group-{group}")))
                    .px_2()
                    .pt_3()
                    .pb_1()
                    .flex()
                    .items_center()
                    .gap_1()
                    .text_size(px(9.5))
                    .font_weight(gpui::FontWeight::BOLD)
                    .text_color(if header_hover || group_hover {
                        rgbc(theme.accent)
                    } else {
                        rgbc(theme.muted)
                    })
                    .cursor_pointer()
                    .on_mouse_up(
                        MouseButton::Left,
                        cx.listener(move |this, _, _, cx| this.toggle_group(&group_toggle, cx)),
                    )
                    .on_mouse_down(
                        MouseButton::Right,
                        cx.listener(move |this, _, _, cx| {
                            this.group_menu = Some(group_menu_target.clone());
                            this.session_menu = None;
                            cx.stop_propagation();
                            cx.notify();
                        }),
                    )
                    .on_drag(
                        DraggedGroup {
                            name: group_drag.clone(),
                        },
                        move |drag, _, _, cx| {
                            let theme = group_drag_theme.clone();
                            cx.new(|_| GroupDragPreview {
                                name: drag.name.clone(),
                                theme,
                            })
                        },
                    )
                    .on_drag_move::<DraggedGroup>(cx.listener(
                        move |this, event: &DragMoveEvent<DraggedGroup>, _, cx| {
                            let after = event.event.position.y >= event.bounds.center().y;
                            this.group_drag_hover
                                .replace(Some((group_drag.clone(), after)));
                            cx.notify();
                        },
                    ))
                    .on_drop::<DraggedGroup>(cx.listener(
                        move |this, drag: &DraggedGroup, _, cx| {
                            let after = this
                                .group_drag_hover
                                .borrow()
                                .as_ref()
                                .filter(|(target, _)| target == &group_drop)
                                .map(|(_, after)| *after)
                                .unwrap_or(true);
                            this.drop_group(drag, &group_drop, after, cx);
                        },
                    ))
                    .on_drag_move::<DraggedSession>(cx.listener(
                        move |this, _event: &DragMoveEvent<DraggedSession>, _, cx| {
                            this.drag_hover.replace(Some(DragHover {
                                group: group_session_hover.clone(),
                                target: None,
                                after: true,
                            }));
                            cx.notify();
                        },
                    ))
                    .on_drop::<DraggedSession>(cx.listener(
                        move |this, drag: &DraggedSession, _, cx| {
                            this.move_session_to_group(&drag.id, &group_session_drop, cx);
                        },
                    ))
                    .child(div().w(px(9.)).child(if collapsed { "▸" } else { "▾" }))
                    .child(div().flex_1().child(self.tr(&group).to_uppercase()))
                    .child(format!("{}", profiles.len())),
            );
            if self.group_menu.as_deref() == Some(group.as_str()) {
                list = list.child(self.render_group_menu(&group, cx));
            }
            if collapsed {
                continue;
            }
            for profile in profiles {
                let profile_id = profile.id.clone();
                list = list.child(self.render_session_row(profile, cx));
                if self.session_menu.as_deref() == Some(profile_id.as_str()) {
                    list = list.child(self.render_session_menu(&profile_id, cx));
                }
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
        let drop_hover = self
            .drag_hover
            .borrow()
            .as_ref()
            .is_some_and(|hover| hover.target.as_deref() == Some(profile.id.as_str()));
        let row_bg = if drop_hover {
            rgba(theme.accent, 0.18)
        } else if active {
            rgbc(theme.panel_alt)
        } else {
            rgba(theme.panel_alt, 0.0)
        };
        let hover_bg = rgba(theme.accent, 0.1);
        let id = profile.id.clone();
        let menu_id = profile.id.clone();
        let target_id = profile.id.clone();
        let target_group = if profile.group.trim().is_empty() {
            "Sessions".to_string()
        } else {
            profile.group.clone()
        };
        let drag_id = profile.id.clone();
        let drag_name = profile.name.clone();
        let drag_theme = theme.clone();
        let hover_target = target_id.clone();
        let hover_group = target_group.clone();
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
            .on_mouse_down(
                MouseButton::Right,
                cx.listener(move |this, _, _, cx| {
                    this.session_menu = Some(menu_id.clone());
                    cx.stop_propagation();
                    cx.notify();
                }),
            )
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
                    .on_drag(
                        DraggedSession {
                            id: drag_id,
                            name: drag_name,
                        },
                        move |drag, _, _, cx| {
                            let theme = drag_theme.clone();
                            cx.new(|_| SessionDragPreview {
                                name: drag.name.clone(),
                                theme,
                            })
                        },
                    )
                    .on_drag_move::<DraggedSession>(cx.listener(
                        move |this, event: &DragMoveEvent<DraggedSession>, _, cx| {
                            let after = event.event.position.y >= event.bounds.center().y;
                            this.drag_hover.replace(Some(DragHover {
                                group: hover_group.clone(),
                                target: Some(hover_target.clone()),
                                after,
                            }));
                            cx.notify();
                        },
                    ))
                    .on_drop::<DraggedSession>(cx.listener(
                        move |this, drag: &DraggedSession, _, cx| {
                            let (group, after) = {
                                let hover = this.drag_hover.borrow();
                                hover
                                    .as_ref()
                                    .filter(|hover| {
                                        hover.target.as_deref() == Some(target_id.as_str())
                                    })
                                    .map(|hover| (hover.group.clone(), hover.after))
                                    .unwrap_or((target_group.clone(), true))
                            };
                            this.drop_session(drag, &group, Some(&target_id), after, cx);
                        },
                    ))
                    .on_mouse_up(
                        MouseButton::Left,
                        cx.listener(move |this, event: &MouseUpEvent, window, cx| {
                            let recent = this.last_session_open.borrow().as_ref().is_some_and(
                                |(opened, at)| {
                                    opened == &id && at.elapsed() < Duration::from_millis(800)
                                },
                            );
                            let force_new = event.click_count >= 2 && !recent;
                            this.connect_profile(id.clone(), window, cx, force_new);
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
    }

    fn render_session_menu(&self, profile_id: &str, cx: &mut Context<Self>) -> Div {
        let theme = self.theme();
        let edit_id = profile_id.to_string();
        let duplicate_id = profile_id.to_string();
        let connect_id = profile_id.to_string();
        let button = |suffix: &str, label: String, theme: &Theme| {
            div()
                .id(SharedString::from(format!(
                    "session-menu-{suffix}-{profile_id}"
                )))
                .flex_1()
                .h(px(26.))
                .flex()
                .items_center()
                .justify_center()
                .rounded_sm()
                .bg(rgbc(theme.background))
                .text_size(px(10.5))
                .text_color(rgbc(theme.foreground))
                .cursor_pointer()
                .hover(|style| style.bg(rgbc(theme.panel)))
                .child(label)
        };
        let groups = self.sessions.groups();
        let current_group = self
            .sessions
            .get(profile_id)
            .map(|profile| {
                if profile.group.trim().is_empty() {
                    "Sessions".to_string()
                } else {
                    profile.group
                }
            })
            .unwrap_or_else(|| "Sessions".to_string());
        let mut group_row = div()
            .flex()
            .flex_wrap()
            .items_center()
            .gap_1()
            .px_1()
            .child(
                div()
                    .text_size(px(9.5))
                    .text_color(rgbc(theme.muted))
                    .child(self.tr("Move to group")),
            );
        for group in groups {
            let target = group.clone();
            let session = profile_id.to_string();
            let selected = group == current_group;
            group_row = group_row.child(
                div()
                    .id(SharedString::from(format!(
                        "session-menu-group-{profile_id}-{group}"
                    )))
                    .px_2()
                    .py_1()
                    .rounded_sm()
                    .border_1()
                    .border_color(if selected {
                        rgbc(theme.accent)
                    } else {
                        rgbc(theme.border)
                    })
                    .bg(if selected {
                        rgba(theme.accent, 0.16)
                    } else {
                        rgbc(theme.background)
                    })
                    .text_size(px(9.5))
                    .text_color(rgbc(theme.foreground))
                    .cursor_pointer()
                    .on_mouse_up(
                        MouseButton::Left,
                        cx.listener(move |this, _, _, cx| {
                            this.move_session_to_group(&session, &target, cx);
                        }),
                    )
                    .child(self.tr(&group)),
            );
        }
        group_row = group_row.child(
            div()
                .id(SharedString::from(format!(
                    "session-menu-new-group-{profile_id}"
                )))
                .px_2()
                .py_1()
                .rounded_sm()
                .border_1()
                .border_color(rgbc(theme.border))
                .bg(rgbc(theme.background))
                .text_size(px(9.5))
                .text_color(rgbc(theme.accent))
                .cursor_pointer()
                .on_mouse_up(
                    MouseButton::Left,
                    cx.listener(move |this, _, _, cx| {
                        this.open_group_dialog(cx);
                    }),
                )
                .child(self.tr("New group")),
        );
        div()
            .mx_2()
            .mb_1()
            .p_1()
            .flex()
            .flex_col()
            .gap_1()
            .rounded_md()
            .border_1()
            .border_color(rgbc(theme.border))
            .bg(rgbc(theme.panel_alt))
            .child(
                div()
                    .flex()
                    .gap_1()
                    .child(button("edit", self.tr("edit"), theme).on_mouse_up(
                        MouseButton::Left,
                        cx.listener(move |this, _, _, cx| {
                            this.open_session_dialog(Some(edit_id.clone()), cx);
                        }),
                    ))
                    .child(
                        button("duplicate", self.tr("Duplicate"), theme).on_mouse_up(
                            MouseButton::Left,
                            cx.listener(move |this, _, _, cx| {
                                this.duplicate_session(&duplicate_id, cx);
                            }),
                        ),
                    )
                    .child(button("connect", self.tr("connect"), theme).on_mouse_up(
                        MouseButton::Left,
                        cx.listener(move |this, _, window, cx| {
                            this.connect_profile(connect_id.clone(), window, cx, true);
                        }),
                    ))
                    .child(button("cancel", self.tr("Cancel"), theme).on_mouse_up(
                        MouseButton::Left,
                        cx.listener(move |this, _, _, cx| {
                            this.session_menu = None;
                            cx.notify();
                        }),
                    )),
            )
            .child(group_row)
    }

    fn drop_session(
        &mut self,
        drag: &DraggedSession,
        group: &str,
        target: Option<&str>,
        after: bool,
        cx: &mut Context<Self>,
    ) {
        if target == Some(drag.id.as_str()) {
            self.drag_hover.replace(None);
            cx.notify();
            return;
        }
        if let Err(error) = self.sessions.move_session(&drag.id, group, target, after) {
            self.set_notice(NoticeLevel::Error, error);
            return;
        }
        self.drag_hover.replace(None);
        self.sync(cx);
        cx.notify();
    }

    fn move_session_to_group(&mut self, id: &str, group: &str, cx: &mut Context<Self>) {
        if let Err(error) = self.sessions.move_session(id, group, None, true) {
            self.set_notice(NoticeLevel::Error, error);
            return;
        }
        self.session_menu = None;
        self.drag_hover.replace(None);
        self.sync(cx);
        cx.notify();
    }

    fn render_group_menu(&self, group: &str, cx: &mut Context<Self>) -> Div {
        let theme = self.theme();
        let rename_group = group.to_string();
        let collapse_group = group.to_string();
        let collapsed = self.collapsed_groups.borrow().contains(group);
        let button = |suffix: &str, label: String, theme: &Theme| {
            div()
                .id(SharedString::from(format!("group-menu-{suffix}-{group}")))
                .flex_1()
                .h(px(26.))
                .flex()
                .items_center()
                .justify_center()
                .rounded_sm()
                .bg(rgbc(theme.background))
                .text_size(px(10.5))
                .text_color(rgbc(theme.foreground))
                .cursor_pointer()
                .hover(|style| style.bg(rgbc(theme.panel)))
                .child(label)
        };
        div()
            .mx_2()
            .mb_1()
            .p_1()
            .flex()
            .gap_1()
            .rounded_md()
            .border_1()
            .border_color(rgbc(theme.border))
            .bg(rgbc(theme.panel_alt))
            .child(
                button("rename", self.tr("Rename group"), theme).on_mouse_up(
                    MouseButton::Left,
                    cx.listener(move |this, _, _, cx| {
                        this.open_rename_group_dialog(&rename_group, cx);
                    }),
                ),
            )
            .child(button("new", self.tr("New group"), theme).on_mouse_up(
                MouseButton::Left,
                cx.listener(move |this, _, _, cx| {
                    this.open_group_dialog(cx);
                }),
            ))
            .child(
                button(
                    "collapse",
                    self.tr(if collapsed { "Expand" } else { "Collapse" }),
                    theme,
                )
                .on_mouse_up(
                    MouseButton::Left,
                    cx.listener(move |this, _, _, cx| {
                        this.toggle_group(&collapse_group, cx);
                        this.group_menu = None;
                        cx.notify();
                    }),
                ),
            )
            .child(button("cancel", self.tr("Cancel"), theme).on_mouse_up(
                MouseButton::Left,
                cx.listener(|this, _, _, cx| {
                    this.group_menu = None;
                    cx.notify();
                }),
            ))
    }

    fn toggle_group(&mut self, group: &str, cx: &mut Context<Self>) {
        {
            let mut collapsed = self.collapsed_groups.borrow_mut();
            if !collapsed.remove(group) {
                collapsed.insert(group.to_string());
            }
        }
        cx.notify();
    }

    fn drop_group(
        &mut self,
        drag: &DraggedGroup,
        target: &str,
        after: bool,
        cx: &mut Context<Self>,
    ) {
        if drag.name == target {
            self.group_drag_hover.replace(None);
            cx.notify();
            return;
        }
        if let Err(error) = self.sessions.move_group(&drag.name, Some(target), after) {
            self.set_notice(NoticeLevel::Error, error);
            return;
        }
        self.group_drag_hover.replace(None);
        self.group_menu = None;
        self.sync(cx);
        cx.notify();
    }

    fn copied_session_name(&self, base: &str) -> String {
        let names = self
            .sessions
            .list()
            .into_iter()
            .map(|profile| profile.name)
            .collect::<Vec<_>>();
        let first = self.tr_args("{name} copy", &[("name", base.to_string())]);
        if !names.iter().any(|name| name == &first) {
            return first;
        }
        for index in 2..10_000 {
            let candidate = self.tr_args(
                "{name} copy {index}",
                &[("name", base.to_string()), ("index", index.to_string())],
            );
            if !names.iter().any(|name| name == &candidate) {
                return candidate;
            }
        }
        first
    }

    fn duplicate_session(&mut self, profile_id: &str, cx: &mut Context<Self>) {
        let Some(profile) = self.sessions.get(profile_id) else {
            self.set_notice(NoticeLevel::Error, self.tr("Session no longer exists."));
            return;
        };
        let name = self.copied_session_name(&profile.name);
        let mut copy = profile.duplicate(name.clone());
        let secret = match &profile.auth {
            AuthKind::Password { .. } => self
                .sessions
                .password(&profile)
                .map(|password| (true, password)),
            AuthKind::PublicKey { .. } => self
                .sessions
                .passphrase(&profile)
                .map(|passphrase| (false, passphrase)),
            _ => None,
        };
        let result = match secret {
            Some((true, password)) => {
                if let AuthKind::Password { secret_id, .. } = &mut copy.auth {
                    *secret_id = None;
                }
                self.sessions.store_password(&copy, &password)
            }
            Some((false, passphrase)) => {
                if let AuthKind::PublicKey { secret_id, .. } = &mut copy.auth {
                    *secret_id = None;
                }
                self.sessions.store_passphrase(&copy, &passphrase)
            }
            None => self.sessions.save(copy.clone()),
        };
        if let Err(error) = result {
            self.set_notice(NoticeLevel::Error, error);
            return;
        }
        self.session_menu = None;
        self.sync(cx);
        self.set_notice(
            NoticeLevel::Info,
            self.tr_args("Copied session to {name}", &[("name", name)]),
        );
        self.open_session_dialog(Some(copy.id), cx);
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
        let groups = self.tab_groups.borrow().clone();
        let assignments = self.tab_group_of.borrow().clone();
        if !groups.is_empty() {
            tabs = tabs.child(
                div()
                    .id("tab-group-ungrouped")
                    .px_2()
                    .py_1()
                    .rounded_sm()
                    .border_1()
                    .border_color(rgbc(theme.border))
                    .text_size(px(8.5))
                    .font_weight(gpui::FontWeight::BOLD)
                    .text_color(rgbc(theme.muted))
                    .on_drop::<DraggedTab>(cx.listener(move |this, drag: &DraggedTab, _, cx| {
                        this.assign_tab_to_group(drag.id, None, cx);
                    }))
                    .child(self.tr("Ungrouped")),
            );
        }
        for tab in &self.tabs {
            if !assignments.contains_key(&tab.id) {
                tabs = tabs.child(self.render_tab_item(tab, cx));
            }
        }
        for group in groups {
            let group_tabs = self
                .tabs
                .iter()
                .filter(|tab| assignments.get(&tab.id) == Some(&group.id))
                .collect::<Vec<_>>();
            let group_id = group.id.clone();
            let toggle_id = group.id.clone();
            let collapsed = self.collapsed_tab_groups.borrow().contains(&group.id);
            tabs = tabs.child(
                div()
                    .id(SharedString::from(format!("tab-group-{}", group.id)))
                    .px_2()
                    .py_1()
                    .rounded_sm()
                    .border_1()
                    .border_color(rgbc(theme.accent))
                    .bg(rgba(theme.accent, 0.12))
                    .text_size(px(8.5))
                    .font_weight(gpui::FontWeight::BOLD)
                    .text_color(rgbc(theme.accent))
                    .cursor_pointer()
                    .on_mouse_up(
                        MouseButton::Left,
                        cx.listener(move |this, _, _, cx| {
                            this.toggle_tab_group(&toggle_id, cx);
                        }),
                    )
                    .on_drop::<DraggedTab>(cx.listener(move |this, drag: &DraggedTab, _, cx| {
                        this.assign_tab_to_group(drag.id, Some(group_id.clone()), cx);
                    }))
                    .child(format!(
                        "{} {} ({})",
                        if collapsed { "▸" } else { "▾" },
                        group.name,
                        group_tabs.len()
                    )),
            );
            if collapsed {
                continue;
            }
            for tab in group_tabs {
                tabs = tabs.child(self.render_tab_item(tab, cx));
            }
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
                    .id("new-tab-group")
                    .px_2()
                    .py_1()
                    .rounded_md()
                    .text_size(px(10.))
                    .text_color(rgbc(theme.muted))
                    .cursor_pointer()
                    .hover(|style| style.text_color(rgbc(theme.accent)))
                    .on_mouse_up(
                        MouseButton::Left,
                        cx.listener(|this, _, _, cx| this.open_tab_group_dialog(cx)),
                    )
                    .child(self.tr("Group")),
            )
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

    fn render_tab_menu(&self, tab_id: TerminalId, cx: &mut Context<Self>) -> Div {
        let theme = self.theme();
        let Some(tab) = self.tabs.iter().find(|tab| tab.id == tab_id) else {
            return div();
        };
        let groups = self.tab_groups.borrow().clone();
        let position = self.tabs.iter().position(|entry| entry.id == tab_id);
        let has_left = position.is_some_and(|index| index > 0);
        let has_right = position.is_some_and(|index| index + 1 < self.tabs.len());
        let close_current = tab_id;
        let close_all = tab_id;
        let close_right = tab_id;
        let close_left = tab_id;
        let create_group = tab_id;
        let move_tab = tab_id;
        let button = |suffix: &str, label: String, enabled: bool, theme: &Theme| {
            div()
                .id(SharedString::from(format!("tab-menu-{suffix}")))
                .px_2()
                .py_1()
                .rounded_sm()
                .border_1()
                .border_color(rgbc(theme.border))
                .text_size(px(10.5))
                .text_color(if enabled {
                    rgbc(theme.foreground)
                } else {
                    rgbc(theme.muted)
                })
                .cursor_pointer()
                .hover(|style| style.bg(rgbc(theme.background)))
                .child(label)
        };
        let mut join_chips = div().flex().items_center().gap_1().flex_wrap();
        for group in &groups {
            let group_id = group.id.clone();
            let label = group.name.clone();
            join_chips = join_chips.child(
                div()
                    .id(SharedString::from(format!("tab-menu-group-{}", group.id)))
                    .px_2()
                    .py_1()
                    .rounded_sm()
                    .border_1()
                    .border_color(rgbc(theme.accent))
                    .text_size(px(10.))
                    .text_color(rgbc(theme.accent))
                    .cursor_pointer()
                    .hover(|style| style.bg(rgbc(theme.background)))
                    .on_mouse_up(
                        MouseButton::Left,
                        cx.listener(move |this, _, _, cx| {
                            this.assign_tab_to_group(move_tab, Some(group_id.clone()), cx);
                            this.tab_menu = None;
                            cx.notify();
                        }),
                    )
                    .child(label),
            );
        }
        join_chips = join_chips.child(
            div()
                .id("tab-menu-group-ungrouped")
                .px_2()
                .py_1()
                .rounded_sm()
                .border_1()
                .border_color(rgbc(theme.border))
                .text_size(px(10.))
                .text_color(rgbc(theme.muted))
                .cursor_pointer()
                .hover(|style| style.bg(rgbc(theme.background)))
                .on_mouse_up(
                    MouseButton::Left,
                    cx.listener(move |this, _, _, cx| {
                        this.assign_tab_to_group(move_tab, None, cx);
                        this.tab_menu = None;
                        cx.notify();
                    }),
                )
                .child(self.tr("Ungrouped")),
        );
        div()
            .px_2()
            .pb_2()
            .bg(rgbc(theme.panel))
            .border_b_1()
            .border_color(rgbc(theme.border))
            .child(
                div()
                    .w(px(700.))
                    .p_2()
                    .rounded_md()
                    .bg(rgbc(theme.panel_alt))
                    .border_1()
                    .border_color(rgbc(theme.border))
                    .flex()
                    .flex_col()
                    .gap_2()
                    .child(
                        div()
                            .flex()
                            .items_center()
                            .gap_2()
                            .child(
                                div()
                                    .text_size(px(10.5))
                                    .font_weight(gpui::FontWeight::BOLD)
                                    .text_color(rgbc(theme.muted))
                                    .child(self.tr("Tab actions")),
                            )
                            .child(
                                div()
                                    .text_size(px(10.5))
                                    .text_color(rgbc(theme.foreground))
                                    .overflow_hidden()
                                    .text_ellipsis()
                                    .child(tab.title.clone()),
                            )
                            .child(div().flex_1())
                            .child(
                                div()
                                    .id("tab-menu-close")
                                    .px_1()
                                    .rounded_sm()
                                    .text_color(rgbc(theme.muted))
                                    .cursor_pointer()
                                    .hover(|style| style.text_color(rgbc(theme.danger)))
                                    .on_mouse_up(
                                        MouseButton::Left,
                                        cx.listener(|this, _, _, cx| {
                                            this.tab_menu = None;
                                            cx.notify();
                                        }),
                                    )
                                    .child("×"),
                            ),
                    )
                    .child(
                        div()
                            .flex()
                            .flex_col()
                            .gap_1()
                            .child(
                                button("close-current", self.tr("Close current"), true, theme)
                                    .w_full()
                                    .on_mouse_up(
                                        MouseButton::Left,
                                        cx.listener(move |this, _, _, cx| {
                                            this.close_tab_ids(vec![close_current], cx);
                                        }),
                                    ),
                            )
                            .child(
                                button("close-all", self.tr("Close all"), true, theme)
                                    .w_full()
                                    .on_mouse_up(
                                        MouseButton::Left,
                                        cx.listener(move |this, _, _, cx| {
                                            let _ = close_all;
                                            this.close_all_tabs(cx);
                                        }),
                                    ),
                            )
                            .child(
                                button(
                                    "close-right",
                                    self.tr("Close to the right"),
                                    has_right,
                                    theme,
                                )
                                .w_full()
                                .on_mouse_up(
                                    MouseButton::Left,
                                    cx.listener(move |this, _, _, cx| {
                                        let _ = close_right;
                                        this.tab_menu = Some(close_right);
                                        this.close_tab_side(true, cx);
                                    }),
                                ),
                            )
                            .child(
                                button("close-left", self.tr("Close to the left"), has_left, theme)
                                    .on_mouse_up(
                                        MouseButton::Left,
                                        cx.listener(move |this, _, _, cx| {
                                            this.tab_menu = Some(close_left);
                                            this.close_tab_side(false, cx);
                                        }),
                                    ),
                            ),
                    )
                    .child(
                        div()
                            .flex()
                            .items_center()
                            .gap_2()
                            .child(
                                button("create-group", self.tr("Create group"), true, theme)
                                    .on_mouse_up(
                                        MouseButton::Left,
                                        cx.listener(move |this, _, _, cx| {
                                            this.open_tab_group_dialog_for_tab(create_group, cx);
                                            cx.notify();
                                        }),
                                    ),
                            )
                            .child(
                                div()
                                    .text_size(px(10.))
                                    .text_color(rgbc(theme.muted))
                                    .child(self.tr("Join group:")),
                            )
                            .child(join_chips),
                    ),
            )
    }

    fn render_tab_item(&self, tab: &TabInfo, cx: &mut Context<Self>) -> Stateful<Div> {
        let theme = self.theme();
        let active = Some(tab.id) == self.active;
        let tab_id = tab.id;
        let dot = match &tab.status {
            TabStatus::Connected => rgbc(theme.success),
            TabStatus::Connecting => rgbc(theme.accent),
            TabStatus::Failed(_) => rgbc(theme.danger),
            TabStatus::Closed => rgbc(theme.muted),
        };
        let tab_theme = theme.clone();
        let drag_name = tab.title.clone();
        let drop_target = tab.id;
        let hover = self
            .tab_drag_hover
            .borrow()
            .as_ref()
            .is_some_and(|(target, _)| *target == tab.id);
        div()
            .id(SharedString::from(format!("tab-{}", tab.id)))
            .h(px(34.))
            .flex()
            .items_center()
            .gap_2()
            .px_3()
            .rounded_md()
            .bg(if hover {
                rgba(theme.accent, 0.16)
            } else if active {
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
            .on_drag(
                DraggedTab {
                    id: tab.id,
                    name: drag_name,
                },
                move |drag, _, _, cx| {
                    let theme = tab_theme.clone();
                    cx.new(|_| TabDragPreview {
                        name: drag.name.clone(),
                        theme,
                    })
                },
            )
            .on_drag_move::<DraggedTab>(cx.listener(
                move |this, event: &DragMoveEvent<DraggedTab>, _, cx| {
                    let after = event.event.position.y >= event.bounds.center().y;
                    this.tab_drag_hover.replace(Some((drop_target, after)));
                    cx.notify();
                },
            ))
            .on_drop::<DraggedTab>(cx.listener(move |this, drag: &DraggedTab, _, cx| {
                if drag.id != drop_target {
                    let after = this
                        .tab_drag_hover
                        .borrow()
                        .as_ref()
                        .filter(|(target, _)| *target == drop_target)
                        .map(|(_, after)| *after)
                        .unwrap_or(true);
                    let target_group = this.tab_group_of.borrow().get(&drop_target).cloned();
                    match target_group {
                        Some(group) => {
                            this.tab_group_of.borrow_mut().insert(drag.id, group);
                        }
                        None => {
                            this.tab_group_of.borrow_mut().remove(&drag.id);
                        }
                    }
                    this.workspace.move_tab(drag.id, drop_target, after);
                    this.sync(cx);
                }
                this.tab_drag_hover.replace(None);
                cx.notify();
            }))
            .on_mouse_up(
                MouseButton::Right,
                cx.listener(move |this, _, _, cx| {
                    this.tab_menu = Some(tab_id);
                    this.session_menu = None;
                    this.group_menu = None;
                    cx.stop_propagation();
                    cx.notify();
                }),
            )
            .on_mouse_up(
                MouseButton::Left,
                cx.listener(move |this, _, window, cx| {
                    this.tab_menu = None;
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
                            this.tab_group_of.borrow_mut().remove(&tab_id);
                            this.tab_drag_hover.replace(None);
                            this.sync(cx);
                            cx.notify();
                        }),
                    )
                    .child("×"),
            )
    }

    fn snippet_label(&self, name: &str) -> String {
        match name {
            "List files" | "Disk usage" | "Tail syslog" => self.tr(name),
            _ => name.to_string(),
        }
    }

    fn open_b64_upload(&mut self, cx: &mut Context<Self>) {
        let Some(tab) = self.active else {
            self.set_notice(
                NoticeLevel::Warning,
                self.tr("Open a terminal tab before sending a file."),
            );
            return;
        };
        self.b64_upload_open = true;
        self.b64_upload_source = None;
        self.b64_upload_size = 0;
        self.b64_upload_error = None;
        self.b64_uploading = false;
        self.b64_upload_status = None;
        self.b64_upload_cancel = Rc::new(Cell::new(false));
        self.b64_upload_temp_path = None;
        self.b64_upload_tab = Some(tab);
        self.b64_upload_temp_input
            .update(cx, |input, cx| input.set_value("/tmp", cx));
        self.b64_upload_dest_input
            .update(cx, |input, cx| input.clear(cx));
        cx.notify();
    }

    fn close_b64_upload(&mut self, cx: &mut Context<Self>) {
        if self.b64_uploading {
            self.cancel_b64_upload(cx);
        }
        self.b64_upload_open = false;
        self.b64_upload_status = None;
        cx.notify();
    }

    fn pick_b64_upload_source(&mut self, cx: &mut Context<Self>) {
        let receiver = cx.prompt_for_paths(PathPromptOptions {
            files: true,
            directories: false,
            multiple: false,
            prompt: Some(self.tr("Select a file to send").into()),
        });
        cx.spawn(async move |this, cx| {
            let Ok(Ok(Some(paths))) = receiver.await else {
                return;
            };
            let Some(path) = paths.into_iter().next() else {
                return;
            };
            let size = std::fs::metadata(&path).map(|meta| meta.len()).unwrap_or(0);
            let _ = this.update(cx, |app, cx| {
                app.b64_upload_source = Some(path.clone());
                app.b64_upload_size = size;
                app.b64_upload_error = None;
                let name = path
                    .file_name()
                    .and_then(|name| name.to_str())
                    .unwrap_or("file");
                app.b64_upload_dest_input
                    .update(cx, |input, cx| input.set_value(format!("/tmp/{name}"), cx));
                cx.notify();
            });
        })
        .detach();
    }

    fn start_b64_upload(&mut self, cx: &mut Context<Self>) {
        if self.b64_uploading {
            return;
        }
        let Some(source) = self.b64_upload_source.clone() else {
            self.b64_upload_error = Some(self.tr("Choose the local file to send."));
            cx.notify();
            return;
        };
        let temp_dir = self
            .b64_upload_temp_input
            .read(cx)
            .value()
            .trim()
            .to_string();
        let destination = self
            .b64_upload_dest_input
            .read(cx)
            .value()
            .trim()
            .to_string();
        if temp_dir.is_empty() || destination.is_empty() {
            self.b64_upload_error =
                Some(self.tr("Temporary directory and destination are required."));
            cx.notify();
            return;
        }
        let Some(tab) = self.active else {
            self.b64_upload_error = Some(self.tr("Open a terminal tab before sending a file."));
            cx.notify();
            return;
        };
        self.b64_uploading = true;
        self.b64_upload_error = None;
        self.b64_upload_status = Some(self.tr("Reading local file…"));
        self.b64_upload_cancel = Rc::new(Cell::new(false));
        self.b64_upload_tab = Some(tab);
        cx.notify();

        let name = source
            .file_name()
            .and_then(|name| name.to_str())
            .unwrap_or("file")
            .to_string();
        let task = cx.background_executor().spawn(async move {
            let bytes = std::fs::read(&source).map_err(|error| error.to_string())?;
            let encoded = base64::engine::general_purpose::STANDARD.encode(&bytes);
            let mut payload = String::with_capacity(encoded.len() + encoded.len() / 76 + 2);
            for chunk in encoded.as_bytes().chunks(76) {
                payload.push_str(std::str::from_utf8(chunk).unwrap_or(""));
                payload.push('\n');
            }
            Ok::<String, String>(payload)
        });
        cx.spawn(async move |this, cx| {
            let result = task.await;
            let _ = this.update(cx, |app, cx| match result {
                Ok(payload) => {
                    if app.b64_upload_cancel.get() {
                        app.b64_uploading = false;
                        app.b64_upload_status = None;
                        app.set_notice(NoticeLevel::Info, app.tr("Transfer cancelled."));
                        cx.notify();
                        return;
                    }
                    app.begin_b64_upload_payload(tab, temp_dir, destination, name, payload, cx);
                }
                Err(error) => {
                    app.b64_uploading = false;
                    app.b64_upload_status = None;
                    app.b64_upload_error = Some(
                        app.tr_args("Could not read local file: {error}", &[("error", error)]),
                    );
                    cx.notify();
                }
            });
        })
        .detach();
    }

    fn begin_b64_upload_payload(
        &mut self,
        tab: TerminalId,
        temp_dir: String,
        destination: String,
        name: String,
        payload: String,
        cx: &mut Context<Self>,
    ) {
        if !self.b64_uploading || self.b64_upload_tab != Some(tab) {
            return;
        }
        let temp_file = format!(
            "{}/xcord-b64-{}-{}.part",
            temp_dir.trim_end_matches('/'),
            epoch_seconds(),
            sanitize_filename(&name)
        );
        self.b64_upload_temp_path = Some(temp_file.clone());
        let temp_quoted = shell_quote(&temp_file);
        let dest_quoted = shell_quote(&destination);
        let dir_quoted = shell_quote(&temp_dir);
        let command = format!(
            "stty -echo; mkdir -p {dir_quoted}; mkdir -p \"$(dirname {dest_quoted})\"; base64 -d > {temp_quoted}; s=$?; if [ $s -eq 0 ]; then mv {temp_quoted} {dest_quoted}; s=$?; fi; stty echo; echo __XCORD_B64_DONE__:$s"
        );
        self.workspace.write(tab, command.as_bytes());
        self.workspace.write(tab, b"\r");
        self.b64_upload_status = Some(self.tr("Transferring…"));
        let workspace = self.workspace.clone();
        let cancel = self.b64_upload_cancel.clone();
        let total = payload.len();
        let fallback_temp = temp_file.clone();
        cx.spawn(async move |this, cx| {
            Timer::after(Duration::from_millis(250)).await;
            let bytes = payload.as_bytes();
            let mut sent = 0;
            let mut chunks = 0usize;
            while sent < total {
                if cancel.get() {
                    workspace.write(tab, b"\x03");
                    Timer::after(Duration::from_millis(250)).await;
                    workspace.write(
                        tab,
                        format!("stty echo; rm -f {}\r", shell_quote(&fallback_temp)).as_bytes(),
                    );
                    let _ = this.update(cx, |app, cx| {
                        app.b64_uploading = false;
                        app.b64_upload_temp_path = None;
                        app.b64_upload_status = None;
                        app.b64_upload_open = false;
                        app.set_notice(NoticeLevel::Info, app.tr("Transfer cancelled."));
                        cx.notify();
                    });
                    return;
                }
                let end = (sent + 4096).min(total);
                workspace.write(tab, &bytes[sent..end]);
                sent = end;
                chunks += 1;
                if chunks % 8 == 0 {
                    Timer::after(Duration::from_millis(4)).await;
                }
                if chunks % 24 == 0 {
                    let percent = if total == 0 {
                        100
                    } else {
                        (sent * 100 / total).min(100)
                    };
                    let _ = this.update(cx, |app, cx| {
                        app.b64_upload_status = Some(app.tr_args(
                            "Transferring… {percent}%",
                            &[("percent", percent.to_string())],
                        ));
                        cx.notify();
                    });
                }
            }
            workspace.write(tab, b"\x04");
            let deadline = Instant::now() + Duration::from_secs(600);
            loop {
                Timer::after(Duration::from_millis(300)).await;
                if cancel.get() {
                    workspace.write(tab, b"\x03");
                    Timer::after(Duration::from_millis(250)).await;
                    workspace.write(
                        tab,
                        format!("stty echo; rm -f {}\r", shell_quote(&fallback_temp)).as_bytes(),
                    );
                    let _ = this.update(cx, |app, cx| {
                        app.b64_uploading = false;
                        app.b64_upload_temp_path = None;
                        app.b64_upload_status = None;
                        app.b64_upload_open = false;
                        app.set_notice(NoticeLevel::Info, app.tr("Transfer cancelled."));
                        cx.notify();
                    });
                    return;
                }
                let done = workspace.snapshot(tab).and_then(|snapshot| {
                    snapshot.lines.iter().rev().find_map(|line| {
                        line.text
                            .find("__XCORD_B64_DONE__:")
                            .and_then(|index| {
                                line.text[index + "__XCORD_B64_DONE__:".len()..]
                                    .split_whitespace()
                                    .next()
                            })
                            .and_then(|status| status.parse::<i32>().ok())
                    })
                });
                if let Some(status) = done {
                    let _ = this.update(cx, |app, cx| {
                        app.b64_uploading = false;
                        app.b64_upload_status = None;
                        app.b64_upload_temp_path = None;
                        if status == 0 {
                            app.b64_upload_open = false;
                            app.set_notice(
                                NoticeLevel::Success,
                                app.tr_args(
                                    "Base64 transfer complete: {path}",
                                    &[("path", destination.clone())],
                                ),
                            );
                        } else {
                            app.b64_upload_error = Some(app.tr_args(
                                "Remote transfer failed with status {status}.",
                                &[("status", status.to_string())],
                            ));
                        }
                        cx.notify();
                    });
                    return;
                }
                if Instant::now() >= deadline {
                    workspace.write(tab, b"\x03");
                    Timer::after(Duration::from_millis(250)).await;
                    workspace.write(
                        tab,
                        format!("stty echo; rm -f {}\r", shell_quote(&fallback_temp)).as_bytes(),
                    );
                    let _ = this.update(cx, |app, cx| {
                        app.b64_uploading = false;
                        app.b64_upload_status = None;
                        app.b64_upload_temp_path = None;
                        app.b64_upload_error = Some(app.tr("Remote transfer timed out."));
                        cx.notify();
                    });
                    return;
                }
            }
        })
        .detach();
        cx.notify();
    }

    fn cancel_b64_upload(&mut self, cx: &mut Context<Self>) {
        self.b64_upload_cancel.set(true);
        self.b64_upload_status = Some(self.tr("Cancelling…"));
        cx.notify();
    }

    fn render_b64_upload(&self, cx: &mut Context<Self>) -> Div {
        let theme = self.theme();
        let source = self
            .b64_upload_source
            .as_ref()
            .map(|path| path.to_string_lossy().into_owned())
            .unwrap_or_else(|| self.tr("No file selected"));
        let large = self.b64_upload_size > 1_048_576;
        div()
            .w(px(640.))
            .p_4()
            .rounded_lg()
            .border_1()
            .border_color(rgbc(theme.border))
            .bg(rgbc(theme.panel))
            .flex()
            .flex_col()
            .gap_3()
            .child(
                div()
                    .flex()
                    .items_center()
                    .child(
                        div()
                            .flex_1()
                            .text_size(px(15.))
                            .font_weight(gpui::FontWeight::BOLD)
                            .text_color(rgbc(theme.foreground))
                            .child(self.tr("Base64 file transfer")),
                    )
                    .child(
                        div()
                            .id("b64-upload-close")
                            .px_2()
                            .py_1()
                            .rounded_md()
                            .text_color(rgbc(theme.muted))
                            .cursor_pointer()
                            .hover(|style| style.text_color(rgbc(theme.danger)))
                            .on_mouse_up(
                                MouseButton::Left,
                                cx.listener(|this, _, _, cx| this.close_b64_upload(cx)),
                            )
                            .child("×"),
                    ),
            )
            .child(self.b64_upload_label(self.tr("Local file"), theme))
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap_2()
                    .child(
                        div()
                            .flex_1()
                            .min_w(px(0.))
                            .h(px(30.))
                            .px_2()
                            .flex()
                            .items_center()
                            .rounded_md()
                            .bg(rgbc(theme.panel_alt))
                            .border_1()
                            .border_color(rgbc(theme.border))
                            .text_size(px(10.5))
                            .text_color(rgbc(theme.muted))
                            .child(source),
                    )
                    .child(small_button(self.tr("Browse…"), theme).on_mouse_up(
                        MouseButton::Left,
                        cx.listener(|this, _, _, cx| this.pick_b64_upload_source(cx)),
                    )),
            )
            .when(large, |panel| {
                panel.child(
                    div()
                        .text_size(px(10.))
                        .text_color(rgbc(theme.accent))
                        .child(self.tr_args(
                            "The file is large ({size}). Base64 transfer may take a long time.",
                            &[("size", format_size(self.b64_upload_size))],
                        )),
                )
            })
            .child(self.b64_upload_label(self.tr("Remote temp directory"), theme))
            .child(self.b64_upload_input(self.b64_upload_temp_input.clone(), theme))
            .child(self.b64_upload_label(self.tr("Destination file"), theme))
            .child(self.b64_upload_input(self.b64_upload_dest_input.clone(), theme))
            .child(
                div()
                    .text_size(px(9.5))
                    .text_color(rgbc(theme.muted))
                    .child(self.tr(
                        "The file is sent as Base64 through the terminal and decoded on the remote host.",
                    )),
            )
            .when_some(self.b64_upload_error.clone(), |panel, error| {
                panel.child(
                    div()
                        .text_size(px(10.5))
                        .text_color(rgbc(theme.danger))
                        .child(error),
                )
            })
            .when_some(self.b64_upload_status.clone(), |panel, status| {
                panel.child(
                    div()
                        .text_size(px(10.5))
                        .text_color(rgbc(theme.accent))
                        .child(status),
                )
            })
            .child(
                div()
                    .flex()
                    .justify_end()
                    .gap_2()
                    .child(secondary_button(self.tr("Close"), theme).on_mouse_up(
                        MouseButton::Left,
                        cx.listener(|this, _, _, cx| this.close_b64_upload(cx)),
                    ))
                    .when(self.b64_uploading, |actions| {
                        actions.child(primary_button(self.tr("Cancel"), theme).on_mouse_up(
                            MouseButton::Left,
                            cx.listener(|this, _, _, cx| this.cancel_b64_upload(cx)),
                        ))
                    })
                    .when(!self.b64_uploading, |actions| {
                        actions.child(primary_button(self.tr("Start transfer"), theme).on_mouse_up(
                            MouseButton::Left,
                            cx.listener(|this, _, _, cx| this.start_b64_upload(cx)),
                        ))
                    }),
            )
    }

    fn b64_upload_label(&self, label: String, theme: &Theme) -> Div {
        div()
            .text_size(px(10.5))
            .text_color(rgbc(theme.muted))
            .child(label)
    }

    fn b64_upload_input(&self, input: Entity<TextInput>, theme: &Theme) -> Div {
        div()
            .h(px(32.))
            .px_2()
            .flex()
            .items_center()
            .rounded_md()
            .bg(rgbc(theme.panel_alt))
            .border_1()
            .border_color(rgbc(theme.border))
            .child(input)
    }

    fn toggle_base64_capture(&mut self, cx: &mut Context<Self>) {
        let Some(id) = self.active else {
            self.set_notice(
                NoticeLevel::Warning,
                self.tr("Open a terminal tab before enabling Base64 capture."),
            );
            return;
        };
        if self.base64_target == Some(id) {
            self.cancel_base64_capture(cx, true);
            return;
        }
        if let Some(old) = self.base64_target.take() {
            self.workspace.take_output_capture(old);
        }
        self.base64_target = Some(id);
        self.base64_capturing = false;
        self.base64_last_output = None;
        self.base64_started = Some(Instant::now());
        self.base64_settle_generation
            .set(self.base64_settle_generation.get() + 1);
        self.base64_timeout_generation
            .set(self.base64_timeout_generation.get() + 1);
        self.base64_error = None;
        self.workspace.begin_output_capture(id);
        self.set_notice(
            NoticeLevel::Info,
            self.tr("Base64 capture armed. Run the next command."),
        );
        cx.notify();
    }

    fn cancel_base64_capture(&mut self, cx: &mut Context<Self>, notify: bool) {
        if let Some(id) = self.base64_target.take() {
            self.workspace.take_output_capture(id);
        }
        self.base64_capturing = false;
        self.base64_last_output = None;
        self.base64_started = None;
        self.base64_settle_generation
            .set(self.base64_settle_generation.get() + 1);
        self.base64_timeout_generation
            .set(self.base64_timeout_generation.get() + 1);
        if notify {
            self.set_notice(NoticeLevel::Info, self.tr("Base64 capture cancelled."));
        }
        cx.notify();
    }

    fn on_base64_command_submitted(&mut self, id: TerminalId, cx: &mut Context<Self>) {
        if self.base64_target != Some(id) || self.base64_capturing {
            return;
        }
        self.base64_capturing = true;
        self.base64_started = Some(Instant::now());
        self.base64_last_output = None;
        self.base64_settle_generation
            .set(self.base64_settle_generation.get() + 1);
        self.schedule_base64_settle(id, 900, cx);
        let generation = self.base64_timeout_generation.get() + 1;
        self.base64_timeout_generation.set(generation);
        cx.spawn(async move |this, cx| {
            Timer::after(Duration::from_secs(30)).await;
            let _ = this.update(cx, |app, cx| {
                if app.base64_target == Some(id)
                    && app.base64_capturing
                    && app.base64_timeout_generation.get() == generation
                {
                    app.finish_base64_capture(cx);
                }
            });
        })
        .detach();
        cx.notify();
    }

    fn schedule_base64_settle(&mut self, id: TerminalId, delay_ms: u64, cx: &mut Context<Self>) {
        let generation = self.base64_settle_generation.get() + 1;
        self.base64_settle_generation.set(generation);
        cx.spawn(async move |this, cx| {
            Timer::after(Duration::from_millis(delay_ms)).await;
            let _ = this.update(cx, |app, cx| {
                if app.base64_target == Some(id)
                    && app.base64_capturing
                    && app.base64_settle_generation.get() == generation
                {
                    app.finish_base64_capture(cx);
                }
            });
        })
        .detach();
    }

    fn finish_base64_capture(&mut self, cx: &mut Context<Self>) {
        let Some(id) = self.base64_target.take() else {
            return;
        };
        self.base64_capturing = false;
        self.base64_last_output = None;
        self.base64_started = None;
        self.base64_settle_generation
            .set(self.base64_settle_generation.get() + 1);
        self.base64_timeout_generation
            .set(self.base64_timeout_generation.get() + 1);
        let raw = self.workspace.take_output_capture(id).unwrap_or_default();
        match decode_base64_output(&raw) {
            Ok((name, bytes)) => match save_base64_file(&name, &bytes) {
                Ok(path) => self.set_notice(
                    NoticeLevel::Success,
                    self.tr_args(
                        "Base64 file saved: {path}",
                        &[("path", path.to_string_lossy().into_owned())],
                    ),
                ),
                Err(error) => {
                    self.base64_error = Some(
                        self.tr_args("Could not save decoded file: {error}", &[("error", error)]),
                    );
                }
            },
            Err(Base64DecodeError::Empty) => {
                self.base64_error =
                    Some(self.tr("The command produced no output. Base64 parsing failed."));
            }
            Err(Base64DecodeError::NotFound) => {
                self.base64_error = Some(self.tr("No Base64 text found in the command output."));
            }
            Err(Base64DecodeError::Invalid(error)) => {
                self.base64_error =
                    Some(self.tr_args("Base64 data is invalid: {error}", &[("error", error)]));
            }
        }
        cx.notify();
    }

    fn render_base64_error(&self, cx: &mut Context<Self>) -> Div {
        let theme = self.theme();
        let message = self.base64_error.clone().unwrap_or_default();
        div()
            .w(px(520.))
            .p_4()
            .rounded_lg()
            .border_1()
            .border_color(rgbc(theme.danger))
            .bg(rgbc(theme.panel))
            .flex()
            .flex_col()
            .gap_3()
            .child(
                div()
                    .text_size(px(15.))
                    .font_weight(gpui::FontWeight::BOLD)
                    .text_color(rgbc(theme.danger))
                    .child(self.tr("Base64 decode failed")),
            )
            .child(
                div()
                    .text_size(px(11.5))
                    .text_color(rgbc(theme.foreground))
                    .child(message),
            )
            .child(div().flex().justify_end().child(
                primary_button(self.tr("Close"), theme).on_mouse_up(
                    MouseButton::Left,
                    cx.listener(|this, _, _, cx| {
                        this.base64_error = None;
                        cx.notify();
                    }),
                ),
            ))
    }

    fn open_quick_commands(&mut self, cx: &mut Context<Self>) {
        self.quick_commands_open = true;
        self.quick_editing = None;
        self.quick_error = None;
        self.quick_name.update(cx, |input, cx| input.clear(cx));
        self.quick_command.update(cx, |input, cx| input.clear(cx));
        self.quick_search.update(cx, |input, cx| input.clear(cx));
        cx.notify();
    }

    fn load_quick_command(&mut self, snippet: Snippet, cx: &mut Context<Self>) {
        self.quick_editing = Some(snippet.id.clone());
        self.quick_error = None;
        let display_name = self.snippet_label(&snippet.name);
        self.quick_name
            .update(cx, |input, cx| input.set_value(display_name, cx));
        self.quick_command
            .update(cx, |input, cx| input.set_value(snippet.command.clone(), cx));
        cx.notify();
    }

    fn new_quick_command(&mut self, cx: &mut Context<Self>) {
        self.quick_editing = None;
        self.quick_error = None;
        self.quick_name.update(cx, |input, cx| input.clear(cx));
        self.quick_command.update(cx, |input, cx| input.clear(cx));
        cx.notify();
    }

    fn save_quick_command(&mut self, cx: &mut Context<Self>) {
        let name = self.quick_name.read(cx).value().trim().to_string();
        let command = self.quick_command.read(cx).value().to_string();
        if name.is_empty() {
            self.quick_error = Some(self.tr("Quick command name cannot be empty."));
            cx.notify();
            return;
        }
        if command.trim().is_empty() {
            self.quick_error = Some(self.tr("Quick command cannot be empty."));
            cx.notify();
            return;
        }
        let mut snippet = self
            .quick_editing
            .as_ref()
            .and_then(|id| {
                self.sessions
                    .snippets()
                    .into_iter()
                    .find(|snippet| &snippet.id == id)
            })
            .unwrap_or_else(|| Snippet::new(name.clone(), command.clone()));
        snippet.name = name;
        snippet.command = command;
        snippet.send_enter = true;
        if let Err(error) = self.sessions.save_snippet(snippet.clone()) {
            self.quick_error = Some(error);
            cx.notify();
            return;
        }
        self.quick_editing = Some(snippet.id);
        self.quick_error = None;
        self.set_notice(
            NoticeLevel::Success,
            self.tr_args("Quick command saved: {name}", &[("name", snippet.name)]),
        );
        cx.notify();
    }

    fn delete_quick_command(&mut self, cx: &mut Context<Self>) {
        let Some(id) = self.quick_editing.clone() else {
            return;
        };
        match self.sessions.delete_snippet(&id) {
            Ok(()) => {
                self.new_quick_command(cx);
                self.set_notice(NoticeLevel::Info, self.tr("Quick command deleted."));
            }
            Err(error) => self.quick_error = Some(error),
        }
        cx.notify();
    }

    fn insert_quick_command(&mut self, command: &str, cx: &mut Context<Self>) {
        if command.trim().is_empty() {
            self.quick_error = Some(self.tr("Quick command cannot be empty."));
            cx.notify();
            return;
        }
        let decoded = decode_quick_command(command);
        self.compose
            .update(cx, |input, cx| input.set_value(decoded, cx));
        self.quick_commands_open = false;
        self.settings.compose_bar = true;
        self.pending_focus = Some(PendingFocus::Compose);
        cx.notify();
    }

    fn render_quick_commands(&self, cx: &mut Context<Self>) -> Div {
        let theme = self.theme();
        let query = self.quick_search.read(cx).value().to_lowercase();
        let snippets = self
            .sessions
            .snippets()
            .into_iter()
            .filter(|snippet| {
                query.is_empty()
                    || snippet.name.to_lowercase().contains(&query)
                    || snippet.command.to_lowercase().contains(&query)
            })
            .collect::<Vec<_>>();
        let mut list = div()
            .id("quick-command-list")
            .flex()
            .flex_col()
            .gap_1()
            .flex_1()
            .min_h(px(0.))
            .overflow_y_scroll();
        if snippets.is_empty() {
            list = list.child(
                div()
                    .p_3()
                    .text_size(px(11.))
                    .text_color(rgbc(theme.muted))
                    .child(self.tr("No quick commands yet.")),
            );
        }
        for snippet in snippets {
            let selected = self.quick_editing.as_deref() == Some(snippet.id.as_str());
            let command = snippet.command.clone();
            let row_snippet = snippet.clone();
            list = list.child(
                div()
                    .id(SharedString::from(format!("quick-command-{}", snippet.id)))
                    .px_2()
                    .py_2()
                    .rounded_md()
                    .border_1()
                    .border_color(if selected {
                        rgbc(theme.accent)
                    } else {
                        rgba(theme.border, 0.0)
                    })
                    .bg(if selected {
                        rgba(theme.accent, 0.12)
                    } else {
                        rgba(theme.panel_alt, 0.0)
                    })
                    .cursor_pointer()
                    .hover(|style| style.bg(rgbc(theme.panel_alt)))
                    .on_mouse_up(
                        MouseButton::Left,
                        cx.listener(move |this, event: &MouseUpEvent, _, cx| {
                            this.load_quick_command(row_snippet.clone(), cx);
                            if event.click_count >= 2 {
                                this.insert_quick_command(&command, cx);
                            }
                        }),
                    )
                    .child(
                        div()
                            .text_size(px(11.5))
                            .text_color(rgbc(theme.foreground))
                            .overflow_hidden()
                            .text_ellipsis()
                            .child(self.snippet_label(&snippet.name)),
                    )
                    .child(
                        div()
                            .text_size(px(9.5))
                            .text_color(rgbc(theme.muted))
                            .overflow_hidden()
                            .text_ellipsis()
                            .child(snippet.command.clone()),
                    ),
            );
        }
        let editing = self.quick_editing.is_some();
        let mut actions = div()
            .id("quick-actions")
            .h(px(32.))
            .w_full()
            .flex()
            .items_center()
            .gap_2()
            .flex_shrink_0()
            .child(primary_button(self.tr("Insert"), theme).on_mouse_up(
                MouseButton::Left,
                cx.listener(|this, _, _, cx| {
                    let command = this.quick_command.read(cx).value().to_string();
                    this.insert_quick_command(&command, cx);
                }),
            ))
            .child(secondary_button(self.tr("Save"), theme).on_mouse_up(
                MouseButton::Left,
                cx.listener(|this, _, _, cx| this.save_quick_command(cx)),
            ));
        if editing {
            actions = actions.child(small_button(self.tr("Delete"), theme).on_mouse_up(
                MouseButton::Left,
                cx.listener(|this, _, _, cx| this.delete_quick_command(cx)),
            ));
        }
        div()
            .w(px(940.))
            .h(px(620.))
            .flex()
            .flex_col()
            .rounded_lg()
            .border_1()
            .border_color(rgbc(theme.border))
            .bg(rgbc(theme.panel))
            .child(
                div()
                    .h(px(48.))
                    .px_4()
                    .flex()
                    .items_center()
                    .gap_3()
                    .border_b_1()
                    .border_color(rgbc(theme.border))
                    .child(
                        div()
                            .text_size(px(15.))
                            .font_weight(gpui::FontWeight::BOLD)
                            .text_color(rgbc(theme.foreground))
                            .child(self.tr("Quick commands")),
                    )
                    .child(div().flex_1())
                    .child(
                        div()
                            .id("quick-commands-close")
                            .px_2()
                            .py_1()
                            .rounded_md()
                            .text_color(rgbc(theme.muted))
                            .cursor_pointer()
                            .hover(|style| style.text_color(rgbc(theme.danger)))
                            .on_mouse_up(
                                MouseButton::Left,
                                cx.listener(|this, _, _, cx| {
                                    this.quick_commands_open = false;
                                    cx.notify();
                                }),
                            )
                            .child("×"),
                    ),
            )
            .child(
                div()
                    .flex_1()
                    .min_h(px(0.))
                    .p_3()
                    .flex()
                    .gap_3()
                    .child(
                        div()
                            .w(px(300.))
                            .h_full()
                            .flex()
                            .flex_col()
                            .gap_2()
                            .child(
                                div()
                                    .h(px(30.))
                                    .px_2()
                                    .flex()
                                    .items_center()
                                    .rounded_md()
                                    .bg(rgbc(theme.panel_alt))
                                    .border_1()
                                    .border_color(rgbc(theme.border))
                                    .child(self.quick_search.clone()),
                            )
                            .child(primary_button(self.tr("New"), theme).on_mouse_up(
                                MouseButton::Left,
                                cx.listener(|this, _, _, cx| this.new_quick_command(cx)),
                            ))
                            .child(actions)
                            .child(list),
                    )
                    .child(
                        div()
                            .flex_1()
                            .min_w(px(0.))
                            .h_full()
                            .flex()
                            .flex_col()
                            .gap_2()
                            .px_2()
                            .child(self.tr("Name"))
                            .child(
                                div()
                                    .h(px(32.))
                                    .px_2()
                                    .flex()
                                    .items_center()
                                    .rounded_md()
                                    .bg(rgbc(theme.panel_alt))
                                    .border_1()
                                    .border_color(rgbc(theme.border))
                                    .child(self.quick_name.clone()),
                            )
                            .child(self.tr("Command"))
                            .child(
                                div()
                                    .h(px(32.))
                                    .px_2()
                                    .flex()
                                    .items_center()
                                    .rounded_md()
                                    .bg(rgbc(theme.panel_alt))
                                    .border_1()
                                    .border_color(rgbc(theme.border))
                                    .child(self.quick_command.clone()),
                            )
                            .child(
                                div()
                                    .text_size(px(10.))
                                    .text_color(rgbc(theme.muted))
                                    .child(self.tr(
                                        "Escapes: \\n newline, \\t tab, \\e ESC, \\xNN byte, \\\\ backslash",
                                    )),
                            )
                            .when_some(self.quick_error.clone(), |panel, error| {
                                panel.child(
                                    div()
                                        .text_size(px(10.))
                                        .text_color(rgbc(theme.danger))
                                        .child(error),
                                )
                            }),
                    ),
            )
    }

    fn render_compose(&self, cx: &mut Context<Self>) -> Div {
        let theme = self.theme();
        let snippets = self.sessions.snippets();
        let mut chips = div().flex().items_center().gap_1().flex_shrink_0();
        for snippet in snippets.iter().take(3) {
            let command = snippet.command.clone();
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
                            this.insert_quick_command(&command, cx);
                        }),
                    )
                    .child(self.snippet_label(&snippet.name)),
            );
        }
        let base64_armed = self.base64_target.is_some() && self.base64_target == self.active;
        chips = chips.child(
            div()
                .id("base64-capture")
                .px_2()
                .py_1()
                .rounded_sm()
                .border_1()
                .border_color(if base64_armed {
                    rgbc(theme.success)
                } else {
                    rgbc(theme.border)
                })
                .text_size(px(9.5))
                .text_color(if base64_armed {
                    rgbc(theme.success)
                } else {
                    rgbc(theme.muted)
                })
                .cursor_pointer()
                .hover(|style| style.bg(rgbc(theme.panel_alt)))
                .on_mouse_up(
                    MouseButton::Left,
                    cx.listener(|this, _, _, cx| this.toggle_base64_capture(cx)),
                )
                .child(if base64_armed {
                    self.tr("Base64 armed")
                } else {
                    self.tr("Base64 file")
                }),
        );
        chips = chips.child(
            div()
                .id("quick-commands-open")
                .px_2()
                .py_1()
                .rounded_sm()
                .border_1()
                .border_color(rgbc(theme.accent))
                .text_size(px(9.5))
                .text_color(rgbc(theme.accent))
                .cursor_pointer()
                .hover(|style| style.bg(rgbc(theme.panel_alt)))
                .on_mouse_up(
                    MouseButton::Left,
                    cx.listener(|this, _, _, cx| this.open_quick_commands(cx)),
                )
                .child(self.tr("Quick commands")),
        );

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
            .child(primary_button(self.tr("Send"), theme).on_mouse_up(
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
                            this.connect_profile(id.clone(), window, cx, false);
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
        if let Some(tab_id) = self.tab_menu {
            main = main.child(self.render_tab_menu(tab_id, cx));
        }
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

    fn render_ftp_panel(&self, cx: &mut Context<Self>) -> Div {
        let theme = self.theme();
        let Some(snapshot) = self.ftp_snapshot.clone() else {
            return div()
                .p_6()
                .text_color(rgbc(theme.muted))
                .child(self.tr("Select a terminal tab first."));
        };
        let profile_id = snapshot.profile_id.clone();
        let local_pane = self.render_ftp_pane(
            self.tr("Local"),
            self.ftp_local_input.clone(),
            &snapshot.local_entries,
            self.ftp_local_selected.as_deref(),
            false,
            &profile_id,
            cx,
        );
        let remote_pane = self.render_ftp_pane(
            self.tr("Remote"),
            self.ftp_remote_input.clone(),
            &snapshot.remote_entries,
            self.ftp_remote_selected.as_deref(),
            true,
            &profile_id,
            cx,
        );
        let upload_id = profile_id.clone();
        let download_id = profile_id.clone();
        let delete_id = profile_id.clone();
        div()
            .w(px(1060.))
            .h(px(680.))
            .flex()
            .flex_col()
            .rounded_lg()
            .border_1()
            .border_color(rgbc(theme.border))
            .bg(rgbc(theme.panel))
            .child(
                div()
                    .h(px(48.))
                    .px_4()
                    .flex()
                    .items_center()
                    .gap_3()
                    .border_b_1()
                    .border_color(rgbc(theme.border))
                    .child(
                        div()
                            .text_size(px(15.))
                            .font_weight(gpui::FontWeight::BOLD)
                            .text_color(rgbc(theme.foreground))
                            .child(self.tr("File transfer")),
                    )
                    .child(
                        div()
                            .text_size(px(10.5))
                            .text_color(rgbc(theme.muted))
                            .child(format!("{} · {}", snapshot.profile_name, snapshot.host)),
                    )
                    .child(div().flex_1())
                    .when(snapshot.loading, |header| {
                        header.child(
                            div()
                                .text_size(px(10.5))
                                .text_color(rgbc(theme.accent))
                                .child(self.tr("Connecting…")),
                        )
                    })
                    .when_some(snapshot.error.clone(), |header, error| {
                        header.child(
                            div()
                                .max_w(px(360.))
                                .text_size(px(10.5))
                                .text_color(rgbc(theme.danger))
                                .child(error),
                        )
                    })
                    .child(
                        div()
                            .id("ftp-close")
                            .px_2()
                            .py_1()
                            .rounded_md()
                            .text_color(rgbc(theme.muted))
                            .cursor_pointer()
                            .hover(|style| style.text_color(rgbc(theme.danger)))
                            .on_mouse_up(
                                MouseButton::Left,
                                cx.listener(|this, _, _, cx| {
                                    this.ftp_open = false;
                                    this.ftp_snapshot = None;
                                    cx.notify();
                                }),
                            )
                            .child("×"),
                    ),
            )
            .child(
                div()
                    .flex_1()
                    .min_h(px(0.))
                    .p_3()
                    .flex()
                    .gap_3()
                    .overflow_hidden()
                    .child(local_pane)
                    .child(remote_pane),
            )
            .child(
                div()
                    .h(px(52.))
                    .px_4()
                    .flex()
                    .items_center()
                    .gap_2()
                    .border_t_1()
                    .border_color(rgbc(theme.border))
                    .child(primary_button(self.tr("Upload"), theme).on_mouse_up(
                        MouseButton::Left,
                        cx.listener(move |this, _, _, cx| {
                            let Some(path) = this.ftp_local_selected.clone() else {
                                return;
                            };
                            if let Err(error) = this.ftp.upload(&upload_id, vec![path]) {
                                this.set_notice(NoticeLevel::Error, error);
                            }
                            cx.notify();
                        }),
                    ))
                    .child(primary_button(self.tr("Download"), theme).on_mouse_up(
                        MouseButton::Left,
                        cx.listener(move |this, _, _, cx| {
                            let Some(path) = this.ftp_remote_selected.clone() else {
                                return;
                            };
                            if let Err(error) = this.ftp.download(&download_id, vec![path]) {
                                this.set_notice(NoticeLevel::Error, error);
                            }
                            cx.notify();
                        }),
                    ))
                    .child(
                        div()
                            .id("ftp-upload-picker")
                            .px_3()
                            .py_1()
                            .rounded_md()
                            .border_1()
                            .border_color(rgbc(theme.border))
                            .text_size(px(10.5))
                            .text_color(rgbc(theme.foreground))
                            .cursor_pointer()
                            .hover(|style| style.bg(rgbc(theme.panel_alt)))
                            .on_mouse_up(
                                MouseButton::Left,
                                cx.listener(|this, _, _, cx| this.pick_ftp_uploads(cx)),
                            )
                            .child(self.tr("Upload files…")),
                    )
                    .child(small_button(self.tr("Delete"), theme).on_mouse_up(
                        MouseButton::Left,
                        cx.listener(move |this, _, _, cx| {
                            let Some(path) = this.ftp_remote_selected.clone() else {
                                return;
                            };
                            if let Err(error) = this.ftp.delete_remote(&delete_id, vec![path]) {
                                this.set_notice(NoticeLevel::Error, error);
                            }
                            cx.notify();
                        }),
                    ))
                    .child(div().flex_1())
                    .when_some(snapshot.busy.clone(), |footer, busy| {
                        footer.child(
                            div()
                                .text_size(px(10.5))
                                .text_color(rgbc(theme.accent))
                                .child(busy),
                        )
                    }),
            )
    }

    fn render_ftp_pane(
        &self,
        title: String,
        path_input: Entity<TextInput>,
        entries: &[FtpEntry],
        selected: Option<&str>,
        remote: bool,
        profile_id: &str,
        cx: &mut Context<Self>,
    ) -> Div {
        let theme = self.theme();
        let path = if remote {
            self.ftp_snapshot
                .as_ref()
                .map(|snapshot| snapshot.remote_path.clone())
                .unwrap_or_default()
        } else {
            self.ftp_snapshot
                .as_ref()
                .map(|snapshot| snapshot.local_path.clone())
                .unwrap_or_default()
        };
        let up_path = if remote {
            remote_parent(&path)
        } else {
            local_parent(&path)
        };
        let up_profile = profile_id.to_string();
        let refresh_profile = profile_id.to_string();
        let refresh_path = path.clone();
        let enter_profile = profile_id.to_string();
        let mut toolbar = div()
            .h(px(32.))
            .flex()
            .items_center()
            .gap_1()
            .child(
                div()
                    .id(if remote {
                        "ftp-up-remote"
                    } else {
                        "ftp-up-local"
                    })
                    .px_2()
                    .py_1()
                    .rounded_sm()
                    .text_size(px(11.))
                    .text_color(rgbc(theme.muted))
                    .cursor_pointer()
                    .hover(|style| style.text_color(rgbc(theme.accent)))
                    .on_mouse_up(
                        MouseButton::Left,
                        cx.listener(move |this, _, _, cx| {
                            if remote {
                                this.ftp.navigate_remote(&up_profile, &up_path);
                            } else {
                                this.ftp.navigate_local(&up_path);
                            }
                            cx.notify();
                        }),
                    )
                    .child("↑"),
            )
            .child(
                div()
                    .flex_1()
                    .min_w(px(0.))
                    .h(px(28.))
                    .px_1()
                    .flex()
                    .items_center()
                    .rounded_sm()
                    .bg(rgbc(theme.panel_alt))
                    .border_1()
                    .border_color(rgbc(theme.border))
                    .on_key_down(cx.listener(move |this, event: &KeyDownEvent, _, cx| {
                        if event.keystroke.key == "enter" {
                            let value = if remote {
                                this.ftp_remote_input.read(cx).value()
                            } else {
                                this.ftp_local_input.read(cx).value()
                            };
                            if remote {
                                this.ftp.navigate_remote(&enter_profile, &value);
                            } else {
                                this.ftp.navigate_local(&value);
                            }
                            cx.stop_propagation();
                        }
                    }))
                    .child(path_input.clone()),
            )
            .child(
                div()
                    .id(if remote {
                        "ftp-refresh-remote"
                    } else {
                        "ftp-refresh-local"
                    })
                    .px_2()
                    .py_1()
                    .rounded_sm()
                    .text_size(px(11.))
                    .text_color(rgbc(theme.muted))
                    .cursor_pointer()
                    .hover(|style| style.text_color(rgbc(theme.accent)))
                    .on_mouse_up(
                        MouseButton::Left,
                        cx.listener(move |this, _, _, cx| {
                            if remote {
                                this.ftp.refresh(&refresh_profile);
                            } else {
                                this.ftp.navigate_local(&refresh_path);
                            }
                            cx.notify();
                        }),
                    )
                    .child("⟳"),
            );
        if remote {
            let create_profile = profile_id.to_string();
            toolbar = toolbar
                .child(
                    div()
                        .w(px(118.))
                        .h(px(28.))
                        .px_1()
                        .flex()
                        .items_center()
                        .rounded_sm()
                        .bg(rgbc(theme.panel_alt))
                        .border_1()
                        .border_color(rgbc(theme.border))
                        .child(self.ftp_new_dir.clone()),
                )
                .child(
                    div()
                        .id("ftp-create-dir")
                        .px_2()
                        .py_1()
                        .rounded_sm()
                        .text_size(px(10.))
                        .text_color(rgbc(theme.muted))
                        .cursor_pointer()
                        .hover(|style| style.text_color(rgbc(theme.accent)))
                        .on_mouse_up(
                            MouseButton::Left,
                            cx.listener(move |this, _, _, cx| {
                                let name = this.ftp_new_dir.read(cx).value();
                                if let Err(error) = this.ftp.create_dir(&create_profile, &name) {
                                    this.set_notice(NoticeLevel::Error, error);
                                }
                                cx.notify();
                            }),
                        )
                        .child(self.tr("New folder")),
                );
        }
        let mut rows = div()
            .id(if remote {
                "ftp-rows-remote"
            } else {
                "ftp-rows-local"
            })
            .flex()
            .flex_col()
            .gap_1()
            .flex_1()
            .min_h(px(0.))
            .overflow_y_scroll()
            .pt_2();
        if entries.is_empty() {
            rows = rows.child(
                div()
                    .p_2()
                    .text_size(px(10.5))
                    .text_color(rgbc(theme.muted))
                    .child(self.tr("Empty folder")),
            );
        }
        for (index, entry) in entries.iter().enumerate() {
            let path = entry.path.clone();
            let is_dir = entry.is_dir;
            let name = entry.name.clone();
            let size = if is_dir {
                String::new()
            } else {
                format_size(entry.size)
            };
            let is_selected = selected == Some(path.as_str());
            let entry_profile = profile_id.to_string();
            rows = rows.child(
                div()
                    .id(SharedString::from(format!(
                        "ftp-{}-{index}",
                        if remote { "remote" } else { "local" }
                    )))
                    .h(px(26.))
                    .px_2()
                    .flex()
                    .items_center()
                    .gap_2()
                    .rounded_sm()
                    .bg(if is_selected {
                        rgba(theme.accent, 0.16)
                    } else {
                        rgba(theme.panel_alt, 0.0)
                    })
                    .text_size(px(11.))
                    .text_color(if is_dir {
                        rgbc(theme.accent)
                    } else {
                        rgbc(theme.foreground)
                    })
                    .cursor_pointer()
                    .hover(|style| style.bg(rgbc(theme.panel_alt)))
                    .on_mouse_up(
                        MouseButton::Left,
                        cx.listener(move |this, event: &MouseUpEvent, _, cx| {
                            if remote {
                                this.ftp_remote_selected = Some(path.clone());
                            } else {
                                this.ftp_local_selected = Some(path.clone());
                            }
                            if event.click_count >= 2 && is_dir {
                                if remote {
                                    this.ftp.navigate_remote(&entry_profile, &path);
                                } else {
                                    this.ftp.navigate_local(&path);
                                }
                            }
                            cx.notify();
                        }),
                    )
                    .child(if is_dir { "▸" } else { "·" })
                    .child(
                        div()
                            .flex_1()
                            .min_w(px(0.))
                            .overflow_hidden()
                            .text_ellipsis()
                            .child(name),
                    )
                    .child(
                        div()
                            .text_size(px(9.5))
                            .text_color(rgbc(theme.muted))
                            .child(size),
                    ),
            );
        }
        div()
            .w(px(500.))
            .h(px(550.))
            .min_h(px(0.))
            .flex()
            .flex_col()
            .overflow_hidden()
            .rounded_md()
            .border_1()
            .border_color(rgbc(theme.border))
            .child(
                div()
                    .px_2()
                    .pt_2()
                    .text_size(px(10.))
                    .font_weight(gpui::FontWeight::BOLD)
                    .text_color(rgbc(theme.muted))
                    .child(title),
            )
            .child(div().px_2().pb_1().child(toolbar))
            .child(div().px_2().flex_1().min_h(px(0.)).child(rows))
    }

    fn pick_ftp_uploads(&mut self, cx: &mut Context<Self>) {
        let Some(profile_id) = self.ftp_profile.clone() else {
            return;
        };
        let receiver = cx.prompt_for_paths(PathPromptOptions {
            files: true,
            directories: false,
            multiple: true,
            prompt: Some(self.tr("Select files to upload").into()),
        });
        cx.spawn(async move |this, cx| {
            let Ok(Ok(Some(paths))) = receiver.await else {
                return;
            };
            let paths = paths
                .into_iter()
                .map(|path| path.to_string_lossy().into_owned())
                .collect::<Vec<_>>();
            let _ = this.update(cx, |this, cx| {
                if let Err(error) = this.ftp.upload(&profile_id, paths) {
                    this.set_notice(NoticeLevel::Error, error);
                }
                cx.notify();
            });
        })
        .detach();
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
                            this.apply_theme(id.clone(), cx);
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
        let content = if self.b64_upload_open {
            modal_shell(self.render_b64_upload(cx))
        } else if self.base64_error.is_some() {
            modal_shell(self.render_base64_error(cx))
        } else if self.quick_commands_open {
            modal_shell(self.render_quick_commands(cx))
        } else if self.ftp_open {
            modal_shell(self.render_ftp_panel(cx))
        } else if self.tab_group_form.is_some() {
            modal_shell(self.render_tab_group_dialog(cx))
        } else if self.group_form.is_some() {
            modal_shell(self.render_group_dialog(cx))
        } else if self.session_form.is_some() {
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

fn render_ai_message(
    index: usize,
    message: &AiMessageView,
    theme: &Theme,
    copy_label: &str,
    selected: bool,
    text: Entity<SelectableText>,
    cx: &mut Context<XcordApp>,
) -> Div {
    let is_tool = message.role.starts_with("Tool");
    let mut card = div()
        .w_full()
        .min_w(px(0.))
        .flex()
        .flex_col()
        .gap_1()
        .p_2()
        .rounded_md()
        .border_1()
        .border_color(if selected {
            rgbc(theme.accent)
        } else {
            rgba([0, 0, 0], 0.0)
        })
        .when(selected, |card| card.bg(rgba(theme.accent, 0.08)))
        .on_mouse_down(
            MouseButton::Left,
            cx.listener(move |this, _, window, cx| {
                this.ai_selected_message.set(Some(index));
                this.ai_menu_open.set(false);
                let handle = this.ai_input.read(cx).focus_handle_only();
                window.focus(&handle);
                cx.notify();
            }),
        )
        .on_mouse_down(
            MouseButton::Right,
            cx.listener(move |this, _, window, cx| {
                this.ai_selected_message.set(Some(index));
                this.ai_menu_open.set(true);
                let handle = this.ai_input.read(cx).focus_handle_only();
                window.focus(&handle);
                cx.stop_propagation();
                cx.notify();
            }),
        );
    if is_tool {
        card = card.bg(rgbc(theme.panel_alt)).border_color(if selected {
            rgbc(theme.accent)
        } else {
            rgbc(theme.border)
        });
    }
    let role = match (&message.tool, message.role.as_str()) {
        (Some(tool), _) => format!("{} · {tool}", message.role),
        _ => message.role.clone(),
    };
    let copy_text = message.text.clone();
    card.child(
        div()
            .flex()
            .items_center()
            .justify_between()
            .gap_2()
            .child(
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
            .child(
                div()
                    .id(SharedString::from(format!("ai-copy-{index}")))
                    .text_size(px(9.5))
                    .text_color(rgbc(theme.muted))
                    .cursor_pointer()
                    .hover(|style| style.text_color(rgbc(theme.accent)))
                    .on_mouse_up(
                        MouseButton::Left,
                        cx.listener(move |_, _, _, cx| {
                            cx.write_to_clipboard(ClipboardItem::new_string(copy_text.clone()));
                        }),
                    )
                    .child(copy_label.to_string()),
            ),
    )
    .child(text)
}

#[derive(Debug)]
enum Base64DecodeError {
    Empty,
    NotFound,
    Invalid(String),
}

fn decode_base64_output(raw: &str) -> Result<(String, Vec<u8>), Base64DecodeError> {
    let clean = strip_ansi(raw);
    if clean.trim().is_empty() {
        return Err(Base64DecodeError::Empty);
    }
    let lines = clean.lines().map(str::trim).collect::<Vec<_>>();
    let mut groups: Vec<(usize, String)> = Vec::new();
    let mut current: Option<(usize, String)> = None;
    for (index, line) in lines.iter().enumerate() {
        if line.is_empty() {
            if let Some((start, data)) = current.take() {
                groups.push((start, data));
            }
            continue;
        }
        if let Some(compact) = base64_line(line) {
            if let Some((_, data)) = current.as_mut() {
                data.push_str(&compact);
            } else {
                current = Some((index, compact));
            }
        } else if let Some((start, data)) = current.take() {
            groups.push((start, data));
        }
    }
    if let Some((start, data)) = current.take() {
        groups.push((start, data));
    }
    let Some((start, data)) = groups
        .into_iter()
        .filter(|(_, data)| data.len() >= 8 && data.len() % 4 != 1)
        .last()
    else {
        return Err(Base64DecodeError::NotFound);
    };
    let name = base64_filename(&lines, start);
    let bytes = decode_base64_data(&data).map_err(|error| Base64DecodeError::Invalid(error))?;
    Ok((name, bytes))
}

fn base64_line(line: &str) -> Option<String> {
    let line = line.trim();
    if line.is_empty() {
        return None;
    }
    let lower = line.to_ascii_lowercase();
    for command in ["base64", "echo", "printf", "cat", "sudo", "python", "perl"] {
        if lower == command
            || lower.starts_with(&format!("{command} "))
            || lower.starts_with(&format!("{command}\t"))
            || lower.starts_with(&format!("{command}'"))
            || lower.starts_with(&format!("{command}\""))
        {
            return None;
        }
    }
    if let Some(token) = valid_base64_prefix(line) {
        if token.len() == line.len() && token.len() >= 4 {
            return Some(token);
        }
    }
    let suffix_at = line
        .find('@')
        .or_else(|| line.find('=').map(|index| index + 1));
    let Some(cut) = suffix_at else {
        return None;
    };
    let candidate = line[..cut].trim_end_matches(|ch: char| ch == '=');
    let padding = line[..cut].len() - candidate.len();
    let mut token = valid_base64_prefix(candidate)?;
    if padding > 0 {
        token.push_str(&"=".repeat(padding.min(2)));
    }
    if token.len() >= 4 && token.len() % 4 != 1 {
        Some(token)
    } else {
        None
    }
}

fn valid_base64_prefix(line: &str) -> Option<String> {
    let mut token = String::new();
    let mut padding = 0;
    for ch in line.chars() {
        match ch {
            ch if ch.is_ascii_alphanumeric()
                || ch == '+'
                || ch == '/'
                || ch == '-'
                || ch == '_' =>
            {
                if padding > 0 {
                    break;
                }
                token.push(ch);
            }
            '=' => {
                if token.is_empty() || padding >= 2 {
                    break;
                }
                token.push('=');
                padding += 1;
            }
            _ => break,
        }
    }
    if token.is_empty() {
        return None;
    }
    if token.len() % 4 == 1 {
        return None;
    }
    if token.len() < 4 {
        return None;
    }
    Some(token)
}

fn decode_base64_data(data: &str) -> Result<Vec<u8>, String> {
    let normalized = data.replace('-', "+").replace('_', "/");
    let mut padded = normalized;
    while padded.len() % 4 != 0 {
        padded.push('=');
    }
    base64::engine::general_purpose::STANDARD
        .decode(padded.as_bytes())
        .map_err(|error| error.to_string())
}

fn base64_filename(lines: &[&str], start: usize) -> String {
    for line in lines[..start].iter().rev().take(8) {
        let lower = line.to_ascii_lowercase();
        if let Some(rest) = lower
            .strip_prefix("filename:")
            .or_else(|| lower.strip_prefix("file:"))
        {
            let _ = rest;
            if let Some(colon) = line.find(':') {
                let name = line[colon + 1..].trim();
                if !name.is_empty() {
                    return sanitize_filename(name);
                }
            }
        }
    }
    for line in lines[..start].iter().rev().take(6) {
        if line.contains("base64") {
            if let Some(name) = filename_from_command(line) {
                return sanitize_filename(&name);
            }
        }
    }
    format!("base64-{}.bin", epoch_seconds())
}

fn filename_from_command(line: &str) -> Option<String> {
    let tokens = line.split_whitespace().collect::<Vec<_>>();
    for token in tokens.iter().rev() {
        let token = token.trim_matches(|ch| ch == '"' || ch == '\'' || ch == ';');
        if token.is_empty()
            || token.starts_with('-')
            || token == "|"
            || token == ">"
            || token == ">>"
            || token == "base64"
            || token == "cat"
            || token == "sudo"
        {
            continue;
        }
        if !token.contains('.') && !token.contains('/') && !token.contains('\\') {
            continue;
        }
        if token.contains('@') || token.contains('$') {
            continue;
        }
        return Some(token.to_string());
    }
    None
}

fn sanitize_filename(name: &str) -> String {
    let name = name.trim().trim_matches('"').trim_matches('\'');
    let base = Path::new(name)
        .file_name()
        .and_then(|part| part.to_str())
        .unwrap_or(name);
    let mut result = String::new();
    for ch in base.chars() {
        if ch.is_control() || matches!(ch, '<' | '>' | ':' | '"' | '/' | '\\' | '|' | '?' | '*') {
            result.push('_');
        } else {
            result.push(ch);
        }
    }
    let result = result.trim().trim_end_matches(['.', ' ']).to_string();
    if result.is_empty() || result == "." || result == ".." {
        format!("base64-{}.bin", epoch_seconds())
    } else {
        result
    }
}

fn save_base64_file(name: &str, bytes: &[u8]) -> Result<PathBuf, String> {
    let dir = base64_download_dir();
    std::fs::create_dir_all(&dir).map_err(|error| error.to_string())?;
    let name = sanitize_filename(name);
    let mut path = dir.join(&name);
    if path.exists() {
        let stem = Path::new(&name)
            .file_stem()
            .and_then(|stem| stem.to_str())
            .unwrap_or("decoded");
        let extension = Path::new(&name)
            .extension()
            .and_then(|extension| extension.to_str())
            .unwrap_or("");
        let mut index = 1;
        loop {
            let candidate = if extension.is_empty() {
                format!("{stem} ({index})")
            } else {
                format!("{stem} ({index}).{extension}")
            };
            path = dir.join(candidate);
            if !path.exists() {
                break;
            }
            index += 1;
        }
    }
    std::fs::write(&path, bytes).map_err(|error| error.to_string())?;
    Ok(path)
}

fn base64_download_dir() -> PathBuf {
    let home = std::env::var_os("USERPROFILE")
        .or_else(|| std::env::var_os("HOME"))
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("."));
    let downloads = home.join("Downloads");
    if downloads.is_dir() {
        downloads
    } else {
        home
    }
}

fn epoch_seconds() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|duration| duration.as_secs())
        .unwrap_or(0)
}

fn strip_ansi(input: &str) -> String {
    let mut output = String::with_capacity(input.len());
    let mut chars = input.chars().peekable();
    while let Some(ch) = chars.next() {
        match ch {
            '\x1b' => match chars.next() {
                Some('[') => {
                    for next in chars.by_ref() {
                        if ('@'..='~').contains(&next) {
                            break;
                        }
                    }
                }
                Some(']') => {
                    while let Some(next) = chars.next() {
                        if next == '\x07' {
                            break;
                        }
                        if next == '\x1b' && chars.peek() == Some(&'\\') {
                            chars.next();
                            break;
                        }
                    }
                }
                Some(_) => {}
                None => {}
            },
            '\r' => {
                if chars.peek() == Some(&'\n') {
                    chars.next();
                }
                output.push('\n');
            }
            '\x08' => {
                output.pop();
            }
            '\n' | '\t' => output.push(ch),
            ch if ch.is_control() => {}
            ch => output.push(ch),
        }
    }
    output
}

fn shell_quote(value: &str) -> String {
    format!("'{}'", value.replace('\'', "'\\''"))
}

fn decode_quick_command(text: &str) -> String {
    let mut output = String::with_capacity(text.len());
    let mut chars = text.chars().peekable();
    while let Some(ch) = chars.next() {
        if ch != '\\' {
            output.push(ch);
            continue;
        }
        match chars.next() {
            Some('n') => output.push('\n'),
            Some('r') => output.push('\r'),
            Some('t') => output.push('\t'),
            Some('e') | Some('E') => output.push('\x1b'),
            Some('0') => output.push('\0'),
            Some('\\') => output.push('\\'),
            Some('"') => output.push('"'),
            Some('\'') => output.push('\''),
            Some('x') => {
                let mut hex = String::new();
                for _ in 0..2 {
                    if let Some(next) = chars.peek().copied() {
                        if next.is_ascii_hexdigit() {
                            hex.push(next);
                            chars.next();
                        }
                    }
                }
                if hex.len() == 2 {
                    if let Ok(byte) = u8::from_str_radix(&hex, 16) {
                        output.push(byte as char);
                    }
                } else {
                    output.push_str("\\x");
                    output.push_str(&hex);
                }
            }
            Some('u') => {
                if chars.peek() == Some(&'{') {
                    chars.next();
                    let mut hex = String::new();
                    while let Some(next) = chars.peek().copied() {
                        if next == '}' {
                            chars.next();
                            break;
                        }
                        if next.is_ascii_hexdigit() {
                            hex.push(next);
                            chars.next();
                        } else {
                            break;
                        }
                    }
                    if let Ok(value) = u32::from_str_radix(&hex, 16) {
                        if let Some(decoded) = char::from_u32(value) {
                            output.push(decoded);
                            continue;
                        }
                    }
                    output.push_str("\\u{");
                    output.push_str(&hex);
                    output.push('}');
                } else {
                    output.push_str("\\u");
                }
            }
            Some(other) => {
                output.push('\\');
                output.push(other);
            }
            None => output.push('\\'),
        }
    }
    output
}

fn remote_parent(path: &str) -> String {
    let trimmed = path.trim_end_matches('/');
    if trimmed.is_empty() {
        return "/".to_string();
    }
    match trimmed.rfind('/') {
        Some(0) | None => "/".to_string(),
        Some(index) => trimmed[..index].to_string(),
    }
}

fn local_parent(path: &str) -> String {
    let path = std::path::Path::new(path);
    path.parent()
        .map(|parent| parent.to_string_lossy().into_owned())
        .filter(|parent| !parent.is_empty())
        .unwrap_or_else(|| path.to_string_lossy().into_owned())
}

fn format_size(bytes: u64) -> String {
    const UNITS: [&str; 4] = ["B", "KB", "MB", "GB"];
    let mut value = bytes as f64;
    let mut unit = 0;
    while value >= 1024.0 && unit < UNITS.len() - 1 {
        value /= 1024.0;
        unit += 1;
    }
    if unit == 0 {
        format!("{bytes} {}", UNITS[unit])
    } else {
        format!("{value:.1} {}", UNITS[unit])
    }
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

#[cfg(test)]
mod quick_command_tests {
    use super::decode_quick_command;

    #[test]
    fn decodes_supported_escapes() {
        assert_eq!(decode_quick_command(r"echo a\tb"), "echo a\tb");
        assert_eq!(decode_quick_command(r"echo a\nb"), "echo a\nb");
        assert_eq!(decode_quick_command(r"echo a\rb"), "echo a\rb");
        assert_eq!(
            decode_quick_command(r"echo \e[31mred"),
            "echo \u{1b}[31mred"
        );
        assert_eq!(decode_quick_command(r"echo \x41"), "echo A");
        assert_eq!(decode_quick_command(r"echo \u{41}"), "echo A");
    }

    #[test]
    fn preserves_literal_backslashes_and_unknown_escapes() {
        assert_eq!(decode_quick_command(r"echo a\\nb"), r"echo a\nb");
        assert_eq!(decode_quick_command(r"C:\Users\a_xub"), r"C:\Users\a_xub");
        assert_eq!(decode_quick_command(r"echo \q"), r"echo \q");
    }
}

#[cfg(test)]
mod base64_capture_tests {
    use super::{decode_base64_output, shell_quote, Base64DecodeError};

    #[test]
    fn decodes_command_output_and_uses_command_filename() {
        let output = "base64 hello.txt\r\naGVsbG8=\r\nxubao@host:~$ ";
        let (name, bytes) = decode_base64_output(output).unwrap();
        assert_eq!(name, "hello.txt");
        assert_eq!(bytes, b"hello");
    }

    #[test]
    fn decodes_wrapped_output_and_file_header() {
        let output = "FILE: notes.txt\nSGVs\nbG8g\nd29y\nbGQ=\n";
        let (name, bytes) = decode_base64_output(output).unwrap();
        assert_eq!(name, "notes.txt");
        assert_eq!(bytes, b"Hello world");
    }

    #[test]
    fn ignores_ansi_and_prompt_text() {
        let output = "\u{1b}[32mbase64 data.bin\u{1b}[0m\r\n\u{1b}[1maGVsbG8=\u{1b}[0m\r\n$ ";
        let (name, bytes) = decode_base64_output(output).unwrap();
        assert_eq!(name, "data.bin");
        assert_eq!(bytes, b"hello");
    }

    #[test]
    fn decodes_output_without_trailing_newline() {
        let output = "printf 'aGVsbG8='\r\naGVsbG8=xubao@host:~$ ";
        let (_, bytes) = decode_base64_output(output).unwrap();
        assert_eq!(bytes, b"hello");
    }

    #[test]
    fn ignores_command_echo_lines() {
        let output = "printf 'aGVsbG8='\r\naGVsbG8=\r\nxubao@host:~$ ";
        let (_, bytes) = decode_base64_output(output).unwrap();
        assert_eq!(bytes, b"hello");
    }

    #[test]
    fn shell_quote_escapes_paths() {
        assert_eq!(shell_quote("/tmp/a b"), "'/tmp/a b'");
        assert_eq!(shell_quote("a'b"), r#"'a'\''b'"#);
    }

    #[test]
    fn reports_missing_base64() {
        assert!(matches!(
            decode_base64_output("just a normal command\r\n"),
            Err(Base64DecodeError::NotFound)
        ));
        assert!(matches!(
            decode_base64_output(""),
            Err(Base64DecodeError::Empty)
        ));
    }
}
