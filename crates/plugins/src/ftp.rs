//! Xftp-style SFTP file transfer plugin.
//!
//! The plugin owns SFTP connections and all blocking/async I/O; the shell only
//! renders snapshots and sends simple commands. Connections are independent of
//! terminal tabs, so files can be browsed while a session is open.

use std::collections::HashMap;
use std::future::Future;
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::rc::Rc;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use tokio::sync::{broadcast, Mutex as AsyncMutex};
use xcord_cordis::{Context, Plugin, PluginMeta, Result};
use xcord_plugin_api::{
    services, Command, Commands, Ftp, FtpEntry, FtpEvent, FtpService, FtpSnapshot, I18n,
    NoticeLevel, Runtime, RuntimeHandle, Sessions, Ui, UiCommand,
};
use xcord_ssh::{AuthMethod, HostKeyPolicy as SshHostKeyPolicy, SftpClient, SshOptions};
use xcord_store::{AuthKind, SessionProfile};

#[derive(Clone)]
pub struct FtpPlugin;

const FTP_ZH: &[(&str, &str)] = &[
    ("Connecting…", "正在连接…"),
    ("Session no longer exists.", "会话已不存在。"),
    (
        "Enter the password for this session first.",
        "请先为会话输入密码。",
    ),
    ("Connect to the server first.", "请先连接服务器。"),
    ("Folder name cannot be empty.", "文件夹名称不能为空。"),
    ("Transferring…", "正在传输…"),
];

impl Plugin for FtpPlugin {
    fn meta(&self) -> PluginMeta {
        PluginMeta::new("ftp", "SFTP File Transfer")
            .description("Xftp-style SFTP browser, upload and download")
            .author("xcord")
            .capabilities(&["files", "sftp", "transfer"])
    }

    fn inject(&self) -> &'static [&'static str] {
        &[
            services::RUNTIME,
            services::SESSIONS,
            services::COMMANDS,
            services::UI_HOST,
            services::I18N,
        ]
    }

    fn apply(&self, ctx: Context, _config: serde_json::Value) -> Result<()> {
        let runtime = ctx.require::<Runtime>(services::RUNTIME)?.handle();
        let sessions = (*ctx.require::<Sessions>(services::SESSIONS)?).clone();
        let commands = (*ctx.require::<Commands>(services::COMMANDS)?).clone();
        let ui = (*ctx.require::<Ui>(services::UI_HOST)?).clone();
        let i18n = (*ctx.require::<I18n>(services::I18N)?).clone();
        let translations = i18n.clone();
        let translation_handle = i18n.register_pairs("ftp", "zh-CN", FTP_ZH);
        ctx.effect("unregister ftp translations", move || {
            translations.unregister(translation_handle);
        });
        let shared = Arc::new(FtpShared::new(runtime, sessions, i18n.clone()));
        ctx.provide(
            services::FTP,
            Ftp(Rc::new(FtpState {
                shared: shared.clone(),
            })),
        )?;

        let handle = commands.register(Command::new(
            "ftp.open",
            i18n.t("app", "File transfer"),
            "Files",
            {
                let ui = ui.clone();
                move |_| {
                    ui.send(UiCommand::ToggleFtp);
                }
            },
        ));
        ctx.effect("unregister ftp command", move || {
            commands.unregister(handle)
        });

        Ok(())
    }
}

/// UI-thread service wrapper. Async tasks share the inner `Arc<FtpShared>`.
pub struct FtpState {
    shared: Arc<FtpShared>,
}

impl FtpService for FtpState {
    fn connect(&self, profile_id: &str) -> std::result::Result<(), String> {
        self.shared.connect(profile_id)
    }
    fn disconnect(&self, profile_id: &str) {
        self.shared.disconnect(profile_id)
    }
    fn navigate_remote(&self, profile_id: &str, path: &str) {
        self.shared.navigate_remote(profile_id, path)
    }
    fn navigate_local(&self, path: &str) {
        self.shared.navigate_local(path)
    }
    fn refresh(&self, profile_id: &str) {
        self.shared.refresh(profile_id)
    }
    fn upload(
        &self,
        profile_id: &str,
        local_paths: Vec<String>,
    ) -> std::result::Result<(), String> {
        self.shared.upload(profile_id, local_paths)
    }
    fn download(
        &self,
        profile_id: &str,
        remote_paths: Vec<String>,
    ) -> std::result::Result<(), String> {
        self.shared.download(profile_id, remote_paths)
    }
    fn create_dir(&self, profile_id: &str, name: &str) -> std::result::Result<(), String> {
        self.shared.create_dir(profile_id, name)
    }
    fn delete_remote(
        &self,
        profile_id: &str,
        paths: Vec<String>,
    ) -> std::result::Result<(), String> {
        self.shared.delete_remote(profile_id, paths)
    }
    fn snapshot(&self, profile_id: &str) -> Option<FtpSnapshot> {
        self.shared.snapshot(profile_id)
    }
    fn subscribe(&self) -> broadcast::Receiver<FtpEvent> {
        self.shared.data.events.subscribe()
    }
}

struct LocalPane {
    path: String,
    entries: Vec<FtpEntry>,
}

struct FtpData {
    events: broadcast::Sender<FtpEvent>,
    clients: Mutex<HashMap<String, Arc<AsyncMutex<SftpClient>>>>,
    snapshots: Mutex<HashMap<String, FtpSnapshot>>,
    local: Mutex<LocalPane>,
    profiles: Mutex<HashMap<String, SessionProfile>>,
}

struct FtpShared {
    runtime: RuntimeHandle,
    sessions: Sessions,
    i18n: I18n,
    data: Arc<FtpData>,
}

impl FtpData {
    fn emit(&self, profile_id: &str) {
        let _ = self.events.send(FtpEvent::Changed {
            profile_id: profile_id.to_string(),
        });
    }
}

impl FtpShared {
    fn new(runtime: RuntimeHandle, sessions: Sessions, i18n: I18n) -> Self {
        let path = default_local_path();
        let entries = read_local(&path).unwrap_or_default();
        Self {
            runtime,
            sessions,
            i18n,
            data: Arc::new(FtpData {
                events: broadcast::channel(256).0,
                clients: Mutex::new(HashMap::new()),
                snapshots: Mutex::new(HashMap::new()),
                local: Mutex::new(LocalPane { path, entries }),
                profiles: Mutex::new(HashMap::new()),
            }),
        }
    }

    fn connecting(&self, profile: &SessionProfile) -> FtpSnapshot {
        FtpSnapshot {
            profile_id: profile.id.clone(),
            profile_name: profile.name.clone(),
            host: profile.endpoint(),
            connected: false,
            loading: true,
            busy: Some(self.i18n.t("ftp", "Connecting…")),
            error: None,
            remote_path: "/".to_string(),
            remote_entries: Vec::new(),
            local_path: String::new(),
            local_entries: Vec::new(),
        }
    }

    fn connect(self: &Arc<Self>, profile_id: &str) -> std::result::Result<(), String> {
        let profile = self
            .sessions
            .get(profile_id)
            .ok_or_else(|| self.i18n.t("ftp", "Session no longer exists."))?;
        self.data
            .profiles
            .lock()
            .unwrap()
            .insert(profile_id.to_string(), profile.clone());
        let snapshot = self.connecting(&profile);
        self.data
            .snapshots
            .lock()
            .unwrap()
            .insert(profile_id.to_string(), snapshot);
        self.data.emit(profile_id);

        let credential = match &profile.auth {
            AuthKind::Password { .. } => self.sessions.password(&profile),
            AuthKind::PublicKey { .. } => self.sessions.passphrase(&profile),
            _ => None,
        };
        if matches!(profile.auth, AuthKind::Password { .. }) && credential.is_none() {
            let message = self
                .i18n
                .t("ftp", "Enter the password for this session first.");
            if let Some(snapshot) = self.data.snapshots.lock().unwrap().get_mut(profile_id) {
                snapshot.loading = false;
                snapshot.busy = None;
                snapshot.error = Some(message.clone());
            }
            self.data.emit(profile_id);
            return Err(message);
        }
        let settings = self.sessions.settings();
        let options = SshOptions {
            host: profile.host.clone(),
            port: profile.port,
            user: profile.user.clone(),
            auth: match &profile.auth {
                AuthKind::Password { .. } => AuthMethod::Password(credential.unwrap_or_default()),
                AuthKind::PublicKey { path, .. } => AuthMethod::PublicKey {
                    path: PathBuf::from(path),
                    passphrase: credential,
                },
                _ => AuthMethod::None,
            },
            cols: 80,
            rows: 24,
            term: profile.term.clone(),
            command: None,
            connect_timeout: Duration::from_secs(20),
            keepalive: Some(Duration::from_secs(settings.keepalive_secs.max(5))),
            host_key_policy: match settings.host_key_policy {
                xcord_store::HostKeyPolicy::Strict => SshHostKeyPolicy::Strict,
                xcord_store::HostKeyPolicy::AcceptNew => SshHostKeyPolicy::AcceptNew,
            },
            known_hosts: None,
        };
        let state = self.data.clone();
        let id = profile_id.to_string();
        self.runtime.spawn(async move {
            let result = xcord_ssh::connect_sftp(options).await;
            match result {
                Ok(client) => {
                    let path = client
                        .sftp
                        .canonicalize(".")
                        .await
                        .unwrap_or_else(|_| "/".into());
                    let (path, entries) = read_remote(&client, &path)
                        .await
                        .unwrap_or_else(|_| (path.clone(), Vec::new()));
                    state
                        .clients
                        .lock()
                        .unwrap()
                        .insert(id.clone(), Arc::new(AsyncMutex::new(client)));
                    if let Some(snapshot) = state.snapshots.lock().unwrap().get_mut(&id) {
                        snapshot.connected = true;
                        snapshot.loading = false;
                        snapshot.busy = None;
                        snapshot.remote_path = path;
                        snapshot.remote_entries = entries;
                    }
                }
                Err(error) => {
                    if let Some(snapshot) = state.snapshots.lock().unwrap().get_mut(&id) {
                        snapshot.loading = false;
                        snapshot.busy = None;
                        snapshot.error = Some(error.to_string());
                    }
                }
            }
            state.emit(&id);
        });
        Ok(())
    }

    fn disconnect(self: &Arc<Self>, profile_id: &str) {
        let _ = self.data.clients.lock().unwrap().remove(profile_id);
        if let Some(snapshot) = self.data.snapshots.lock().unwrap().get_mut(profile_id) {
            snapshot.connected = false;
            snapshot.loading = false;
            snapshot.busy = None;
            snapshot.remote_entries.clear();
        }
        self.data.emit(profile_id);
    }

    fn navigate_remote(self: &Arc<Self>, profile_id: &str, path: &str) {
        let Some(client) = self.data.clients.lock().unwrap().get(profile_id).cloned() else {
            return;
        };
        let state = self.data.clone();
        let id = profile_id.to_string();
        let path = path.to_string();
        self.runtime.spawn(async move {
            let client = client.lock().await;
            match read_remote(&client, &path).await {
                Ok((path, entries)) => {
                    if let Some(snapshot) = state.snapshots.lock().unwrap().get_mut(&id) {
                        snapshot.remote_path = path;
                        snapshot.remote_entries = entries;
                        snapshot.error = None;
                    }
                }
                Err(error) => {
                    if let Some(snapshot) = state.snapshots.lock().unwrap().get_mut(&id) {
                        snapshot.error = Some(error);
                    }
                }
            }
            state.emit(&id);
        });
    }

    fn refresh(self: &Arc<Self>, profile_id: &str) {
        if let Some(path) = self
            .data
            .snapshots
            .lock()
            .unwrap()
            .get(profile_id)
            .map(|snapshot| snapshot.remote_path.clone())
        {
            self.navigate_remote(profile_id, &path);
        }
    }

    fn navigate_local(&self, path: &str) {
        match read_local(path) {
            Ok(entries) => {
                let mut local = self.data.local.lock().unwrap();
                local.path = path.to_string();
                local.entries = entries;
                drop(local);
                self.data.emit("*");
            }
            Err(error) => {
                let _ = self.data.events.send(FtpEvent::Notice {
                    level: NoticeLevel::Error,
                    message: error,
                });
            }
        }
    }

    fn upload(
        self: &Arc<Self>,
        profile_id: &str,
        local_paths: Vec<String>,
    ) -> std::result::Result<(), String> {
        let Some(client) = self.data.clients.lock().unwrap().get(profile_id).cloned() else {
            return Err(self.i18n.t("ftp", "Connect to the server first."));
        };
        let remote_dir = self
            .data
            .snapshots
            .lock()
            .unwrap()
            .get(profile_id)
            .map(|snapshot| snapshot.remote_path.clone())
            .unwrap_or_else(|| "/".into());
        if let Some(snapshot) = self.data.snapshots.lock().unwrap().get_mut(profile_id) {
            snapshot.busy = Some(self.i18n.t("ftp", "Transferring…"));
        }
        self.data.emit(profile_id);
        let state = self.data.clone();
        let id = profile_id.to_string();
        self.runtime.spawn(async move {
            {
                let client = client.lock().await;
                for path in local_paths {
                    let local = PathBuf::from(&path);
                    let name = local
                        .file_name()
                        .and_then(|name| name.to_str())
                        .unwrap_or("upload");
                    let remote = join_remote(&remote_dir, name);
                    if let Err(error) = upload_path(&client, &local, &remote).await {
                        if let Some(snapshot) = state.snapshots.lock().unwrap().get_mut(&id) {
                            snapshot.error = Some(error);
                        }
                        break;
                    }
                }
                if let Ok((path, entries)) = read_remote(&client, &remote_dir).await {
                    if let Some(snapshot) = state.snapshots.lock().unwrap().get_mut(&id) {
                        snapshot.remote_path = path;
                        snapshot.remote_entries = entries;
                        snapshot.busy = None;
                    }
                }
            }
            state.emit(&id);
        });
        Ok(())
    }

    fn download(
        self: &Arc<Self>,
        profile_id: &str,
        remote_paths: Vec<String>,
    ) -> std::result::Result<(), String> {
        let Some(client) = self.data.clients.lock().unwrap().get(profile_id).cloned() else {
            return Err(self.i18n.t("ftp", "Connect to the server first."));
        };
        let local_dir = self.data.local.lock().unwrap().path.clone();
        if let Some(snapshot) = self.data.snapshots.lock().unwrap().get_mut(profile_id) {
            snapshot.busy = Some(self.i18n.t("ftp", "Transferring…"));
        }
        self.data.emit(profile_id);
        let state = self.data.clone();
        let id = profile_id.to_string();
        self.runtime.spawn(async move {
            let mut downloaded = Vec::new();
            {
                let client = client.lock().await;
                for remote in remote_paths {
                    let name = remote.rsplit('/').next().unwrap_or("download");
                    let local = PathBuf::from(&local_dir).join(name);
                    if let Err(error) = download_path(&client, &remote, &local).await {
                        if let Some(snapshot) = state.snapshots.lock().unwrap().get_mut(&id) {
                            snapshot.error = Some(error);
                        }
                        break;
                    }
                    downloaded.push(local.to_string_lossy().into_owned());
                }
                if let Ok(entries) = read_local(&local_dir) {
                    state.local.lock().unwrap().entries = entries;
                }
                if let Some(snapshot) = state.snapshots.lock().unwrap().get_mut(&id) {
                    snapshot.busy = None;
                }
            }
            if !downloaded.is_empty() {
                let _ = state.events.send(FtpEvent::Downloaded {
                    profile_id: id.clone(),
                    paths: downloaded,
                });
            }
            state.emit(&id);
        });
        Ok(())
    }

    fn create_dir(
        self: &Arc<Self>,
        profile_id: &str,
        name: &str,
    ) -> std::result::Result<(), String> {
        let name = name.trim();
        if name.is_empty() {
            return Err(self.i18n.t("ftp", "Folder name cannot be empty."));
        }
        let Some(client) = self.data.clients.lock().unwrap().get(profile_id).cloned() else {
            return Err(self.i18n.t("ftp", "Connect to the server first."));
        };
        let remote_dir = self
            .data
            .snapshots
            .lock()
            .unwrap()
            .get(profile_id)
            .map(|snapshot| snapshot.remote_path.clone())
            .unwrap_or_else(|| "/".into());
        let remote = join_remote(&remote_dir, name);
        let state = self.data.clone();
        let id = profile_id.to_string();
        self.runtime.spawn(async move {
            let client = client.lock().await;
            if let Err(error) = client.sftp.create_dir(&remote).await {
                if let Some(snapshot) = state.snapshots.lock().unwrap().get_mut(&id) {
                    snapshot.error = Some(error.to_string());
                }
            } else if let Ok((path, entries)) = read_remote(&client, &remote_dir).await {
                if let Some(snapshot) = state.snapshots.lock().unwrap().get_mut(&id) {
                    snapshot.remote_path = path;
                    snapshot.remote_entries = entries;
                }
            }
            state.emit(&id);
        });
        Ok(())
    }

    fn delete_remote(
        self: &Arc<Self>,
        profile_id: &str,
        paths: Vec<String>,
    ) -> std::result::Result<(), String> {
        let Some(client) = self.data.clients.lock().unwrap().get(profile_id).cloned() else {
            return Err(self.i18n.t("ftp", "Connect to the server first."));
        };
        let remote_dir = self
            .data
            .snapshots
            .lock()
            .unwrap()
            .get(profile_id)
            .map(|snapshot| snapshot.remote_path.clone())
            .unwrap_or_else(|| "/".into());
        let state = self.data.clone();
        let id = profile_id.to_string();
        self.runtime.spawn(async move {
            let client = client.lock().await;
            for path in paths {
                let is_dir = client
                    .sftp
                    .metadata(&path)
                    .await
                    .map(|metadata| metadata.is_dir())
                    .unwrap_or(false);
                let result = if is_dir {
                    client.sftp.remove_dir(&path).await
                } else {
                    client.sftp.remove_file(&path).await
                };
                if let Err(error) = result {
                    if let Some(snapshot) = state.snapshots.lock().unwrap().get_mut(&id) {
                        snapshot.error = Some(error.to_string());
                    }
                    break;
                }
            }
            if let Ok((path, entries)) = read_remote(&client, &remote_dir).await {
                if let Some(snapshot) = state.snapshots.lock().unwrap().get_mut(&id) {
                    snapshot.remote_path = path;
                    snapshot.remote_entries = entries;
                }
            }
            state.emit(&id);
        });
        Ok(())
    }

    fn snapshot(&self, profile_id: &str) -> Option<FtpSnapshot> {
        let mut snapshot = self
            .data
            .snapshots
            .lock()
            .unwrap()
            .get(profile_id)
            .cloned()?;
        let local = self.data.local.lock().unwrap();
        snapshot.local_path = local.path.clone();
        snapshot.local_entries = local.entries.clone();
        Some(snapshot)
    }
}

async fn read_remote(
    client: &SftpClient,
    path: &str,
) -> std::result::Result<(String, Vec<FtpEntry>), String> {
    let path = client
        .sftp
        .canonicalize(path)
        .await
        .map_err(|error| error.to_string())?;
    let mut entries = Vec::new();
    for entry in client
        .sftp
        .read_dir(&path)
        .await
        .map_err(|error| error.to_string())?
    {
        let name = entry.file_name();
        if name == "." || name == ".." {
            continue;
        }
        let metadata = entry.metadata();
        entries.push(FtpEntry {
            path: join_remote(&path, &name),
            name,
            is_dir: metadata.is_dir(),
            size: metadata.size.unwrap_or(0),
            modified: metadata.mtime.map(u64::from),
        });
    }
    entries.sort_by(|a, b| {
        b.is_dir
            .cmp(&a.is_dir)
            .then_with(|| a.name.to_lowercase().cmp(&b.name.to_lowercase()))
    });
    Ok((path, entries))
}

fn read_local(path: &str) -> std::result::Result<Vec<FtpEntry>, String> {
    let mut entries = Vec::new();
    for entry in std::fs::read_dir(path).map_err(|error| error.to_string())? {
        let entry = entry.map_err(|error| error.to_string())?;
        let metadata = entry.metadata().map_err(|error| error.to_string())?;
        entries.push(FtpEntry {
            name: entry.file_name().to_string_lossy().into_owned(),
            path: entry.path().to_string_lossy().into_owned(),
            is_dir: metadata.is_dir(),
            size: metadata.len(),
            modified: metadata
                .modified()
                .ok()
                .and_then(|time| time.duration_since(std::time::UNIX_EPOCH).ok())
                .map(|duration| duration.as_secs()),
        });
    }
    entries.sort_by(|a, b| {
        b.is_dir
            .cmp(&a.is_dir)
            .then_with(|| a.name.to_lowercase().cmp(&b.name.to_lowercase()))
    });
    Ok(entries)
}

fn default_local_path() -> String {
    std::env::var("USERPROFILE")
        .or_else(|_| std::env::var("HOME"))
        .unwrap_or_else(|_| ".".to_string())
}

fn join_remote(base: &str, name: &str) -> String {
    format!("{}/{}", base.trim_end_matches('/'), name)
}

fn upload_path<'a>(
    client: &'a SftpClient,
    local: &'a Path,
    remote: &'a str,
) -> Pin<Box<dyn Future<Output = std::result::Result<(), String>> + Send + 'a>> {
    Box::pin(async move {
        let metadata = tokio::fs::metadata(local)
            .await
            .map_err(|error| error.to_string())?;
        if metadata.is_dir() {
            let _ = client.sftp.create_dir(remote).await;
            let mut dir = tokio::fs::read_dir(local)
                .await
                .map_err(|error| error.to_string())?;
            while let Some(entry) = dir.next_entry().await.map_err(|error| error.to_string())? {
                let name = entry.file_name().to_string_lossy().into_owned();
                let child = entry.path();
                upload_path(client, &child, &join_remote(remote, &name)).await?;
            }
        } else {
            let mut source = tokio::fs::File::open(local)
                .await
                .map_err(|error| error.to_string())?;
            let mut target = client
                .sftp
                .create(remote)
                .await
                .map_err(|error| error.to_string())?;
            tokio::io::copy(&mut source, &mut target)
                .await
                .map_err(|error| error.to_string())?;
            target.close().await.map_err(|error| error.to_string())?;
        }
        Ok(())
    })
}

fn download_path<'a>(
    client: &'a SftpClient,
    remote: &'a str,
    local: &'a Path,
) -> Pin<Box<dyn Future<Output = std::result::Result<(), String>> + Send + 'a>> {
    Box::pin(async move {
        let metadata = client
            .sftp
            .metadata(remote)
            .await
            .map_err(|error| error.to_string())?;
        if metadata.is_dir() {
            tokio::fs::create_dir_all(local)
                .await
                .map_err(|error| error.to_string())?;
            for entry in client
                .sftp
                .read_dir(remote)
                .await
                .map_err(|error| error.to_string())?
            {
                let name = entry.file_name();
                if name == "." || name == ".." {
                    continue;
                }
                download_path(client, &join_remote(remote, &name), &local.join(name)).await?;
            }
        } else {
            let mut source = client
                .sftp
                .open(remote)
                .await
                .map_err(|error| error.to_string())?;
            let mut target = tokio::fs::File::create(local)
                .await
                .map_err(|error| error.to_string())?;
            tokio::io::copy(&mut source, &mut target)
                .await
                .map_err(|error| error.to_string())?;
            source.close().await.map_err(|error| error.to_string())?;
        }
        Ok(())
    })
}
