use std::cell::{Cell, RefCell};
use std::collections::HashMap;
use std::path::PathBuf;
use std::rc::Rc;
use std::time::Duration;

use tokio::sync::broadcast;
use xcord_cordis::{Context, Plugin, PluginMeta, Result};
use xcord_plugin_api::{
    services, AuthRequest, Command, Commands, ConnectSpec, ConnectionTest, HostKeySummary,
    OpenRequest, Runtime, RuntimeHandle, SessionService, Sessions, TabInfo, TabStatus, TerminalId,
    Transport, TransportEvent, TransportRegistry, Transports, Ui, UiCommand, Workspace,
    WorkspaceEvent, WorkspaceService,
};
use xcord_store::{AuthKind, HostKeyPolicy, SessionProfile};
use xcord_vt::Terminal;

use crate::logging::TerminalOutputEvent;

/// Internal service name used by posted I/O jobs to reach the concrete state.
const INTERNAL: &str = "workspace/internal";

/// Plugin that owns tabs and terminal buffers.
#[derive(Clone)]
pub struct WorkspacePlugin;

impl Plugin for WorkspacePlugin {
    fn meta(&self) -> PluginMeta {
        PluginMeta::new("workspace", "Terminal Workspace")
            .description("Tabs, terminal emulation and connection lifecycle")
            .author("xcord")
            .capabilities(&["core", "terminal", "tabs"])
    }

    fn inject(&self) -> &'static [&'static str] {
        &[
            services::RUNTIME,
            services::TRANSPORTS,
            services::SESSIONS,
            services::COMMANDS,
            services::UI_HOST,
        ]
    }

    fn apply(&self, ctx: Context, _config: serde_json::Value) -> Result<()> {
        let runtime = ctx.require::<Runtime>(services::RUNTIME)?;
        let transports = ctx.require::<Transports>(services::TRANSPORTS)?;
        let sessions = ctx.require::<Sessions>(services::SESSIONS)?;
        let commands = ctx.require::<Commands>(services::COMMANDS)?;
        let ui = ctx.require::<Ui>(services::UI_HOST)?;

        let state = Rc::new(WorkspaceState::new(
            ctx.clone(),
            runtime.handle(),
            transports.0.clone(),
            sessions.0.clone(),
        ));

        let service: Rc<dyn WorkspaceService> = state.clone();
        ctx.provide(services::WORKSPACE, Workspace(service))?;
        ctx.provide(INTERNAL, state.clone())?;

        let mut handles = Vec::new();
        handles.push(
            commands.register(
                Command::new("tab.close", "Close active tab", "Tabs", {
                    let state = state.clone();
                    move |_| state.close_active()
                })
                .shortcut("Ctrl+Shift+W"),
            ),
        );
        handles.push(
            commands.register(Command::new("tab.next", "Next tab", "Tabs", {
                let state = state.clone();
                move |_| state.select_next()
            })),
        );
        handles.push(
            commands.register(Command::new("tab.prev", "Previous tab", "Tabs", {
                let state = state.clone();
                move |_| state.select_previous()
            })),
        );
        handles.push(commands.register(Command::new(
            "terminal.reconnect",
            "Reconnect active tab",
            "Terminal",
            {
                let state = state.clone();
                move |_| {
                    let _ = state.reconnect_active();
                }
            },
        )));
        handles.push(commands.register(Command::new(
            "terminal.clear",
            "Clear active terminal",
            "Terminal",
            {
                let state = state.clone();
                move |_| state.clear_active()
            },
        )));
        handles.push(commands.register(Command::new(
            "session.new",
            "New session…",
            "Sessions",
            {
                let ui = ui.clone();
                move |_| ui.send(UiCommand::NewSession)
            },
        )));
        handles.push(commands.register(Command::new(
            "session.edit",
            "Edit session…",
            "Sessions",
            {
                let state = state.clone();
                let ui = ui.clone();
                move |_| {
                    if let Some(id) = state.active_profile_id() {
                        ui.send(UiCommand::EditSession(id));
                    }
                }
            },
        )));
        handles.push(commands.register(Command::new(
            "palette.open",
            "Open command palette",
            "View",
            {
                let ui = ui.clone();
                move |_| ui.send(UiCommand::OpenPalette)
            },
        )));
        handles.push(commands.register(Command::new(
            "plugins.open",
            "Manage plugins…",
            "View",
            {
                let ui = ui.clone();
                move |_| ui.send(UiCommand::ShowPlugins)
            },
        )));
        handles.push(commands.register(Command::new(
            "settings.open",
            "Open settings…",
            "View",
            {
                let ui = ui.clone();
                move |_| ui.send(UiCommand::ShowSettings)
            },
        )));
        handles.push(commands.register(Command::new(
            "compose.focus",
            "Focus compose bar",
            "Terminal",
            {
                let ui = ui.clone();
                move |_| ui.send(UiCommand::FocusCompose)
            },
        )));

        ctx.effect("unregister workspace commands", move || {
            for handle in handles {
                commands.unregister(handle);
            }
        });

        Ok(())
    }
}

struct OutputCapture {
    buffer: Vec<u8>,
    started: bool,
}

struct TabState {
    id: TerminalId,
    profile: SessionProfile,
    terminal: Terminal,
    transport: Option<Box<dyn Transport>>,
    status: TabStatus,
    host_key: Option<HostKeySummary>,
    scroll: usize,
    bell: bool,
    bells: u64,
    credential: Option<String>,
    server: Option<String>,
    last_title: String,
    zmodem: Option<crate::zmodem::Session>,
}

/// The concrete workspace state. Public so the crate's test modules can use it,
/// but consumers only see the [`WorkspaceService`] trait.
pub struct WorkspaceState {
    ctx: Context,
    runtime: RuntimeHandle,
    transports: Rc<TransportRegistry>,
    sessions: Rc<dyn SessionService>,
    tabs: RefCell<Vec<TabState>>,
    active: Cell<Option<TerminalId>>,
    next_id: Cell<u64>,
    next_test_id: Cell<u64>,
    output_captures: RefCell<HashMap<TerminalId, OutputCapture>>,
    events: broadcast::Sender<WorkspaceEvent>,
}

impl WorkspaceState {
    pub fn new(
        ctx: Context,
        runtime: RuntimeHandle,
        transports: Rc<TransportRegistry>,
        sessions: Rc<dyn SessionService>,
    ) -> Self {
        Self {
            ctx,
            runtime,
            transports,
            sessions,
            tabs: RefCell::new(Vec::new()),
            active: Cell::new(None),
            next_id: Cell::new(0),
            next_test_id: Cell::new(0),
            output_captures: RefCell::new(HashMap::new()),
            events: broadcast::channel(2048).0,
        }
    }

    fn emit(&self, event: WorkspaceEvent) {
        let _ = self.events.send(event);
    }

    fn next_id(&self) -> TerminalId {
        let id = self.next_id.get() + 1;
        self.next_id.set(id);
        TerminalId::new(id)
    }

    fn find_index(&self, id: TerminalId) -> Option<usize> {
        self.tabs.borrow().iter().position(|tab| tab.id == id)
    }

    fn build_spec(
        &self,
        profile: &SessionProfile,
        credential: Option<&str>,
        cols: u16,
        rows: u16,
    ) -> std::result::Result<ConnectSpec, String> {
        if profile.host.trim().is_empty() {
            return Err("Host is required.".into());
        }
        let auth = match &profile.auth {
            AuthKind::Password { .. } => {
                let password = credential
                    .filter(|value| !value.is_empty())
                    .ok_or_else(|| "A password is required for this session.".to_string())?;
                AuthRequest::Password(password.to_string())
            }
            AuthKind::PublicKey { path, .. } => {
                if path.trim().is_empty() {
                    return Err("Choose a private key file for this session.".into());
                }
                AuthRequest::PublicKey {
                    path: PathBuf::from(path),
                    passphrase: credential.map(str::to_string),
                }
            }
            AuthKind::Agent => {
                return Err("SSH agent authentication is not implemented yet.".into())
            }
            AuthKind::None => AuthRequest::None,
        };

        Ok(ConnectSpec {
            host: profile.host.trim().to_string(),
            port: profile.port,
            user: profile.user.clone(),
            auth,
            cols,
            rows,
            term: if profile.term.trim().is_empty() {
                "xterm-256color".into()
            } else {
                profile.term.clone()
            },
            command: profile.startup_command.clone(),
            connect_timeout: Duration::from_secs(20),
            keepalive: Some(Duration::from_secs(profile.keepalive_secs.max(5))),
            accept_new_host_keys: self.sessions.settings().host_key_policy
                == HostKeyPolicy::AcceptNew,
            known_hosts: None,
        })
    }

    fn test_connection_impl(&self, request: OpenRequest) -> std::result::Result<u64, String> {
        let spec = self.build_spec(&request.profile, request.credential.as_deref(), 80, 24)?;
        let provider = self
            .transports
            .get("ssh")
            .ok_or_else(|| "SSH transport is not loaded.".to_string())?;
        let (sender, receiver) = async_channel::bounded(16);
        provider.connect(spec, sender)?;

        let id = self.next_test_id.get() + 1;
        self.next_test_id.set(id);
        let events = self.events.clone();
        self.runtime.spawn(async move {
            let outcome = loop {
                let event =
                    match tokio::time::timeout(Duration::from_secs(25), receiver.recv()).await {
                        Ok(Ok(event)) => event,
                        Ok(Err(_)) => {
                            break Err("connection closed before the handshake finished".into())
                        }
                        Err(_) => break Err("connection timed out after 25s".into()),
                    };
                match event {
                    TransportEvent::Connected { transport, info } => {
                        transport.close();
                        break Ok(format!(
                            "Connected to {} ({}) · {} · {}",
                            info.server,
                            info.kind,
                            info.host_key.algorithm,
                            info.host_key.fingerprint
                        ));
                    }
                    TransportEvent::Error(message) => break Err(message),
                    TransportEvent::Closed { reason, .. } => break Err(reason),
                    TransportEvent::Output(_) => {}
                }
            };
            let _ = events.send(WorkspaceEvent::TestFinished(ConnectionTest {
                id,
                ok: outcome.is_ok(),
                message: outcome.unwrap_or_else(|error| error),
            }));
        });
        Ok(id)
    }

    fn open_impl(&self, request: OpenRequest) -> std::result::Result<TerminalId, String> {
        let profile = request.profile;
        let cols = request.cols.max(20);
        let rows = request.rows.max(5);
        let spec = self.build_spec(&profile, request.credential.as_deref(), cols, rows)?;

        let provider = self
            .transports
            .get("ssh")
            .ok_or_else(|| "SSH transport is not loaded.".to_string())?;

        let id = self.next_id();
        let scrollback = self.sessions.settings().scrollback.max(100);
        let terminal = Terminal::new(cols as usize, rows as usize, scrollback);
        let title = profile.name.clone();

        {
            let mut tabs = self.tabs.borrow_mut();
            tabs.push(TabState {
                id,
                profile: profile.clone(),
                terminal,
                transport: None,
                status: TabStatus::Connecting,
                host_key: None,
                scroll: 0,
                bell: false,
                bells: 0,
                credential: request.credential,
                server: None,
                last_title: title,
                zmodem: None,
            });
        }
        self.active.set(Some(id));
        self.emit(WorkspaceEvent::TabsChanged);
        self.emit(WorkspaceEvent::ActiveChanged(Some(id)));

        let (sender, receiver) = async_channel::bounded(1024);
        let office = self.runtime.office.clone();
        self.runtime.spawn(async move {
            while let Ok(event) = receiver.recv().await {
                let posted = office.post(move |ctx| {
                    if let Some(state) = ctx.service::<Rc<WorkspaceState>>(INTERNAL) {
                        state.on_transport_event(id, event);
                    }
                });
                if !posted {
                    break;
                }
            }
        });

        if let Err(error) = provider.connect(spec, sender) {
            self.fail(id, &error);
        }

        Ok(id)
    }

    fn fail(&self, id: TerminalId, message: &str) {
        {
            let mut tabs = self.tabs.borrow_mut();
            if let Some(tab) = tabs.iter_mut().find(|tab| tab.id == id) {
                tab.status = TabStatus::Failed(message.to_string());
                tab.transport = None;
                feed_notice(&mut tab.terminal, message, true);
            }
        }
        self.emit(WorkspaceEvent::StatusChanged(id));
        self.emit(WorkspaceEvent::Dirty(id));
        self.emit(WorkspaceEvent::TabsChanged);
        self.emit(WorkspaceEvent::Notice {
            level: xcord_plugin_api::NoticeLevel::Error,
            message: message.to_string(),
        });
    }

    fn on_transport_event(&self, id: TerminalId, event: TransportEvent) {
        let mut tabs_changed = false;
        let mut status_changed = false;
        let mut bell = false;
        let mut dirty = false;

        match event {
            TransportEvent::Connected { transport, info } => {
                let mut tabs = self.tabs.borrow_mut();
                if let Some(tab) = tabs.iter_mut().find(|tab| tab.id == id) {
                    let key = info.host_key.clone();
                    tab.host_key = Some(key.clone());
                    tab.transport = Some(transport);
                    tab.status = TabStatus::Connected;
                    tab.server = Some(info.server.clone());
                    // The UI may have resized the grid while the connection
                    // was being established; align the remote PTY now.
                    let (cols, rows) = tab.terminal.size();
                    if let Some(transport) = &tab.transport {
                        transport.resize(cols as u16, rows as u16);
                    }
                    let message = if !key.known {
                        format!(
                            "connected to {} — new host key {} {}",
                            info.server, key.algorithm, key.fingerprint
                        )
                    } else {
                        format!(
                            "connected to {} — host key verified ({})",
                            info.server, key.fingerprint
                        )
                    };
                    feed_notice(&mut tab.terminal, &message, false);
                    tabs_changed = true;
                    status_changed = true;
                    dirty = true;
                }
            }
            TransportEvent::Output(data) => {
                let mut raw = None;
                let mut upload_requested = false;
                let mut saved_paths = Vec::new();
                {
                    let mut tabs = self.tabs.borrow_mut();
                    if let Some(tab) = tabs.iter_mut().find(|tab| tab.id == id) {
                        let mut zmodem_notices = Vec::new();
                        let mut transfer = tab.zmodem.is_some();
                        let mut done = false;
                        if let Some(session) = tab.zmodem.as_mut() {
                            done =
                                session.feed(&data, tab.transport.as_deref(), &mut zmodem_notices);
                        } else if let Some((index, start)) = crate::zmodem::detect(&data) {
                            tab.terminal.feed(&data[..index]);
                            transfer = true;
                            match start {
                                crate::zmodem::Start::Receive => {
                                    let settings = self.sessions.settings();
                                    let dir = if settings.download_dir.trim().is_empty() {
                                        crate::zmodem::default_download_dir()
                                    } else {
                                        PathBuf::from(settings.download_dir.trim())
                                    };
                                    let _ = std::fs::create_dir_all(&dir);
                                    let mut session = crate::zmodem::Session::receiving(dir);
                                    done = session.feed(
                                        &data[index..],
                                        tab.transport.as_deref(),
                                        &mut zmodem_notices,
                                    );
                                    if !done {
                                        tab.zmodem = Some(session);
                                    }
                                }
                                crate::zmodem::Start::Upload => {
                                    tab.zmodem = Some(crate::zmodem::Session::upload_pending(
                                        data[index..].to_vec(),
                                    ));
                                    upload_requested = true;
                                    zmodem_notices.push(
                                        "ZMODEM remote rz is waiting; choose files to upload."
                                            .to_string(),
                                    );
                                }
                            }
                        }
                        if !transfer {
                            tab.terminal.feed(&data);
                        }
                        for message in &zmodem_notices {
                            if let Some(path) = message.strip_prefix("ZMODEM saved ") {
                                saved_paths.push(path.to_string());
                            }
                        }
                        for message in zmodem_notices {
                            feed_notice(&mut tab.terminal, &message, false);
                        }
                        if done {
                            tab.zmodem = None;
                        }
                        let bells = tab.terminal.bell_count();
                        if bells > tab.bells {
                            tab.bells = bells;
                            tab.bell = true;
                            bell = true;
                        }
                        if tab.scroll > 0 {
                            // Keep the scrollback anchored while the user is
                            // inspecting history.
                        } else {
                            tab.scroll = 0;
                        }
                        let title = tab.terminal.title().to_string();
                        if !title.is_empty() && title != tab.last_title {
                            tab.last_title = title;
                            tabs_changed = true;
                        }
                        if !transfer {
                            raw = Some((
                                tab.profile.id.clone(),
                                tab.profile.name.clone(),
                                tab.profile.logging,
                                data.clone(),
                            ));
                        }
                    }
                }
                if upload_requested {
                    self.emit(WorkspaceEvent::ZmodemUploadRequested { terminal: id });
                }
                for path in saved_paths {
                    self.emit(WorkspaceEvent::FileSaved { terminal: id, path });
                }
                if let Some(capture) = self.output_captures.borrow_mut().get_mut(&id) {
                    if capture.started {
                        capture.buffer.extend_from_slice(&data);
                    }
                }
                if let Some((profile_id, profile_name, logging, data)) = raw {
                    self.ctx.emit(&TerminalOutputEvent {
                        terminal: id,
                        profile_id,
                        profile_name,
                        logging,
                        data,
                    });
                }
                dirty = true;
            }
            TransportEvent::Error(message) => {
                {
                    let mut tabs = self.tabs.borrow_mut();
                    if let Some(tab) = tabs.iter_mut().find(|tab| tab.id == id) {
                        if !matches!(tab.status, TabStatus::Failed(_)) {
                            tab.status = TabStatus::Failed(message.clone());
                        }
                        feed_notice(&mut tab.terminal, &message, true);
                    }
                }
                status_changed = true;
                dirty = true;
            }
            TransportEvent::Closed {
                reason,
                exit_status,
            } => {
                {
                    let mut tabs = self.tabs.borrow_mut();
                    if let Some(tab) = tabs.iter_mut().find(|tab| tab.id == id) {
                        tab.transport = None;
                        if !matches!(tab.status, TabStatus::Failed(_)) {
                            tab.status = TabStatus::Closed;
                        }
                        let message = match exit_status {
                            Some(code) => format!("{reason} (exit {code})"),
                            None => reason,
                        };
                        feed_notice(&mut tab.terminal, &message, false);
                    }
                }
                status_changed = true;
                dirty = true;
                tabs_changed = true;
                self.emit(WorkspaceEvent::Closed(id));
            }
        }

        if status_changed {
            self.emit(WorkspaceEvent::StatusChanged(id));
        }
        if tabs_changed {
            self.emit(WorkspaceEvent::TabsChanged);
        }
        if dirty {
            self.emit(WorkspaceEvent::Dirty(id));
        }
        if bell {
            self.emit(WorkspaceEvent::Bell(id));
        }
    }

    fn close_active(&self) {
        if let Some(id) = self.active.get() {
            self.close_impl(id);
        }
    }

    fn close_impl(&self, id: TerminalId) {
        self.output_captures.borrow_mut().remove(&id);
        let (removed, new_active) = {
            let mut tabs = self.tabs.borrow_mut();
            let Some(index) = tabs.iter().position(|tab| tab.id == id) else {
                return;
            };
            let tab = tabs.remove(index);
            drop(tab);
            let new_active = if self.active.get() == Some(id) {
                tabs.get(index.min(tabs.len().saturating_sub(1)))
                    .map(|tab| tab.id)
            } else {
                self.active.get()
            };
            (true, new_active)
        };
        if removed {
            self.active.set(new_active);
            self.emit(WorkspaceEvent::Closed(id));
            self.emit(WorkspaceEvent::TabsChanged);
            self.emit(WorkspaceEvent::ActiveChanged(new_active));
        }
    }

    fn reconnect_active(&self) -> std::result::Result<TerminalId, String> {
        let id = self
            .active
            .get()
            .ok_or_else(|| "No active tab.".to_string())?;
        self.reconnect_impl(id)
    }

    fn reconnect_impl(&self, id: TerminalId) -> std::result::Result<TerminalId, String> {
        let (profile, credential, cols, rows) = {
            let tabs = self.tabs.borrow();
            let tab = tabs
                .iter()
                .find(|tab| tab.id == id)
                .ok_or_else(|| "Unknown tab.".to_string())?;
            let snapshot = tab.terminal.snapshot(0);
            (
                tab.profile.clone(),
                tab.credential.clone(),
                snapshot.cols as u16,
                snapshot.rows as u16,
            )
        };
        let new_id = self.open_impl(OpenRequest {
            profile,
            credential,
            cols,
            rows,
        })?;
        self.close_impl(id);
        Ok(new_id)
    }

    fn clear_active(&self) {
        if let Some(id) = self.active.get() {
            if let Some(tab) = self.tabs.borrow_mut().iter_mut().find(|tab| tab.id == id) {
                tab.terminal.feed(b"\x1b[3J\x1b[2J\x1b[H");
                tab.scroll = 0;
            }
            self.emit(WorkspaceEvent::Dirty(id));
        }
    }

    fn active_profile_id(&self) -> Option<String> {
        let id = self.active.get()?;
        let tabs = self.tabs.borrow();
        tabs.iter()
            .find(|tab| tab.id == id)
            .map(|tab| tab.profile.id.clone())
    }

    fn scroll_impl(&self, id: TerminalId, delta: i32) {
        {
            let mut tabs = self.tabs.borrow_mut();
            if let Some(tab) = tabs.iter_mut().find(|tab| tab.id == id) {
                let max = tab.terminal.scrollback_len() as i64;
                let next = (tab.scroll as i64 + delta as i64).clamp(0, max);
                tab.scroll = next as usize;
            }
        }
        self.emit(WorkspaceEvent::Dirty(id));
    }

    fn scroll_to_impl(&self, id: TerminalId, offset: usize) {
        {
            let mut tabs = self.tabs.borrow_mut();
            if let Some(tab) = tabs.iter_mut().find(|tab| tab.id == id) {
                tab.scroll = offset.min(tab.terminal.scrollback_len());
            }
        }
        self.emit(WorkspaceEvent::Dirty(id));
    }

    fn tabs_info(&self) -> Vec<TabInfo> {
        self.tabs
            .borrow()
            .iter()
            .map(|tab| {
                let title = {
                    let terminal_title = tab.terminal.title();
                    if terminal_title.is_empty() {
                        tab.profile.name.clone()
                    } else {
                        terminal_title.to_string()
                    }
                };
                TabInfo {
                    id: tab.id,
                    title,
                    profile_id: tab.profile.id.clone(),
                    profile_name: tab.profile.name.clone(),
                    host: tab.profile.host.clone(),
                    user: tab.profile.user.clone(),
                    port: tab.profile.port,
                    status: tab.status.clone(),
                    host_key: tab.host_key.clone(),
                    bell: tab.bell,
                }
            })
            .collect()
    }
}

fn feed_notice(terminal: &mut Terminal, message: &str, error: bool) {
    let color = if error { "\x1b[31m" } else { "\x1b[2m" };
    let text = format!("\r\n{color}[xcord] {message}\x1b[0m\r\n");
    terminal.feed(text.as_bytes());
}

impl WorkspaceService for WorkspaceState {
    fn tabs(&self) -> Vec<TabInfo> {
        self.tabs_info()
    }

    fn active(&self) -> Option<TerminalId> {
        self.active.get()
    }

    fn select(&self, id: TerminalId) {
        if self.find_index(id).is_some() {
            self.active.set(Some(id));
            if let Some(tab) = self.tabs.borrow_mut().iter_mut().find(|tab| tab.id == id) {
                tab.bell = false;
            }
            self.emit(WorkspaceEvent::ActiveChanged(Some(id)));
            self.emit(WorkspaceEvent::TabsChanged);
        }
    }

    fn select_next(&self) {
        let tabs = self.tabs.borrow();
        if tabs.is_empty() {
            return;
        }
        let index = self
            .active
            .get()
            .and_then(|id| tabs.iter().position(|tab| tab.id == id))
            .unwrap_or(0);
        let next = tabs[(index + 1) % tabs.len()].id;
        drop(tabs);
        self.select(next);
    }

    fn select_previous(&self) {
        let tabs = self.tabs.borrow();
        if tabs.is_empty() {
            return;
        }
        let index = self
            .active
            .get()
            .and_then(|id| tabs.iter().position(|tab| tab.id == id))
            .unwrap_or(0);
        let previous = tabs[(index + tabs.len() - 1) % tabs.len()].id;
        drop(tabs);
        self.select(previous);
    }

    fn move_tab(&self, id: TerminalId, target: TerminalId, after: bool) {
        if id == target {
            return;
        }
        let mut tabs = self.tabs.borrow_mut();
        let Some(index) = tabs.iter().position(|tab| tab.id == id) else {
            return;
        };
        let tab = tabs.remove(index);
        let insert_at = tabs
            .iter()
            .position(|tab| tab.id == target)
            .map(|index| if after { index + 1 } else { index })
            .unwrap_or(tabs.len())
            .min(tabs.len());
        tabs.insert(insert_at, tab);
        drop(tabs);
        self.emit(WorkspaceEvent::TabsChanged);
    }

    fn open(&self, request: OpenRequest) -> std::result::Result<TerminalId, String> {
        self.open_impl(request)
    }

    fn close(&self, id: TerminalId) {
        self.close_impl(id);
    }

    fn reconnect(&self, id: TerminalId) -> std::result::Result<TerminalId, String> {
        self.reconnect_impl(id)
    }

    fn test_connection(&self, request: OpenRequest) -> std::result::Result<u64, String> {
        self.test_connection_impl(request)
    }

    fn write(&self, id: TerminalId, data: &[u8]) {
        let submits = data.iter().any(|byte| *byte == b'\r' || *byte == b'\n');
        if submits {
            if let Some(capture) = self.output_captures.borrow_mut().get_mut(&id) {
                capture.buffer.clear();
                capture.started = true;
            }
        }
        {
            let tabs = self.tabs.borrow();
            if let Some(tab) = tabs.iter().find(|tab| tab.id == id) {
                if tab.zmodem.is_some() {
                    return;
                }
                if let Some(transport) = &tab.transport {
                    if let Err(error) = transport.write(data) {
                        drop(tabs);
                        self.fail(id, &error);
                        return;
                    }
                }
            }
        }
        if submits {
            self.emit(WorkspaceEvent::CommandSubmitted(id));
        }
    }

    fn begin_output_capture(&self, id: TerminalId) {
        self.output_captures.borrow_mut().insert(
            id,
            OutputCapture {
                buffer: Vec::new(),
                started: false,
            },
        );
    }

    fn take_output_capture(&self, id: TerminalId) -> Option<String> {
        self.output_captures
            .borrow_mut()
            .remove(&id)
            .map(|capture| String::from_utf8_lossy(&capture.buffer).into_owned())
    }

    fn zmodem_upload(&self, id: TerminalId, paths: Vec<String>) -> std::result::Result<(), String> {
        let mut tabs = self.tabs.borrow_mut();
        let tab = tabs
            .iter_mut()
            .find(|tab| tab.id == id)
            .ok_or_else(|| "Terminal is not open.".to_string())?;
        let mut notices = Vec::new();
        if let Some(session) = tab.zmodem.as_mut() {
            if let Err(error) = session.start_upload(&paths) {
                session.cancel(tab.transport.as_deref());
                return Err(error);
            }
            let done = session.feed(&[], tab.transport.as_deref(), &mut notices);
            for message in notices {
                feed_notice(&mut tab.terminal, &message, false);
            }
            if done {
                tab.zmodem = None;
            }
        } else {
            return Err("No remote rz is waiting for an upload.".to_string());
        }
        Ok(())
    }

    fn zmodem_cancel(&self, id: TerminalId) {
        let mut tabs = self.tabs.borrow_mut();
        if let Some(tab) = tabs.iter_mut().find(|tab| tab.id == id) {
            if let Some(mut session) = tab.zmodem.take() {
                session.cancel(tab.transport.as_deref());
                feed_notice(&mut tab.terminal, "ZMODEM transfer cancelled.", true);
            }
        }
    }

    fn resize(&self, id: TerminalId, cols: u16, rows: u16) {
        if cols == 0 || rows == 0 {
            return;
        }
        let mut resized = false;
        {
            let mut tabs = self.tabs.borrow_mut();
            if let Some(tab) = tabs.iter_mut().find(|tab| tab.id == id) {
                let (current_cols, current_rows) = tab.terminal.size();
                if current_cols != cols as usize || current_rows != rows as usize {
                    tab.terminal.resize(cols as usize, rows as usize);
                    if let Some(transport) = &tab.transport {
                        transport.resize(cols, rows);
                    }
                    tab.scroll = tab.scroll.min(tab.terminal.scrollback_len());
                    resized = true;
                }
            }
        }
        if resized {
            self.emit(WorkspaceEvent::Dirty(id));
        }
    }

    fn snapshot(&self, id: TerminalId) -> Option<xcord_vt::Snapshot> {
        let tabs = self.tabs.borrow();
        let tab = tabs.iter().find(|tab| tab.id == id)?;
        Some(tab.terminal.snapshot(tab.scroll))
    }

    fn history_text(&self, id: TerminalId) -> Option<String> {
        let tabs = self.tabs.borrow();
        let tab = tabs.iter().find(|tab| tab.id == id)?;
        Some(tab.terminal.history_text())
    }

    fn dimensions(&self, id: TerminalId) -> Option<(u16, u16)> {
        let tabs = self.tabs.borrow();
        let tab = tabs.iter().find(|tab| tab.id == id)?;
        let (cols, rows) = tab.terminal.size();
        Some((cols as u16, rows as u16))
    }

    fn scroll_by(&self, id: TerminalId, delta: i32) {
        self.scroll_impl(id, delta);
    }

    fn scroll_to(&self, id: TerminalId, offset: usize) {
        self.scroll_to_impl(id, offset);
    }

    fn scroll_to_bottom(&self, id: TerminalId) {
        self.scroll_to_impl(id, 0);
    }

    fn text_in_viewport(
        &self,
        id: TerminalId,
        start: (usize, usize),
        end: (usize, usize),
    ) -> String {
        let tabs = self.tabs.borrow();
        match tabs.iter().find(|tab| tab.id == id) {
            Some(tab) => tab.terminal.text_in_viewport(start, end, tab.scroll),
            None => String::new(),
        }
    }

    fn terminal_text(&self, id: TerminalId, max_lines: usize) -> Option<String> {
        let tabs = self.tabs.borrow();
        let tab = tabs.iter().find(|tab| tab.id == id)?;
        let max_lines = max_lines.clamp(20, 4000);
        let scrollback = tab.terminal.scrollback_text(max_lines);
        let screen = tab.terminal.screen_text();
        let combined = format!("{scrollback}{screen}");
        let lines: Vec<&str> = combined.lines().collect();
        let start = lines.len().saturating_sub(max_lines);
        Some(lines[start..].join("\n"))
    }

    fn subscribe(&self) -> broadcast::Receiver<WorkspaceEvent> {
        self.events.subscribe()
    }
}
