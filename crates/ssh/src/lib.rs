//! Pure-Rust SSH transport for xcord, built on [`russh`].
//!
//! This crate knows nothing about plugins or GPUI. It exposes an async
//! [`connect`] function that returns a small, thread-safe [`SshTransport`] and
//! a channel of [`TransportEvent`]s. The plugin layer turns those events into
//! terminal updates on the UI thread.

use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_channel::Sender;
use bytes::Bytes;
use russh::client::{self, AuthResult, Handle, Msg};
use russh::keys::known_hosts::{check_known_hosts, check_known_hosts_path, learn_known_hosts_path};
use russh::keys::{HashAlg, PublicKeyOrCertificate};
use russh::{Channel, ChannelMsg};
use serde::{Deserialize, Serialize};
use thiserror::Error;
use tokio::sync::mpsc;
use tokio::time::timeout;

#[derive(Debug, Error)]
pub enum SshError {
    #[error("connection timed out after {0:?}")]
    Timeout(Duration),
    #[error("SSH protocol error: {0}")]
    Protocol(#[from] russh::Error),
    #[error("key error: {0}")]
    Key(#[from] russh::keys::Error),
    #[error("authentication failed")]
    AuthenticationFailed,
    #[error("host key verification failed: {0}")]
    HostKey(String),
    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),
    #[error("SFTP error: {0}")]
    Sftp(String),
    #[error("session is closed")]
    Closed,
}

/// How strictly to verify server host keys.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum HostKeyPolicy {
    /// Reject unknown hosts. Existing entries must match.
    Strict,
    /// Trust on first use: unknown hosts are appended to `known_hosts`, but a
    /// changed key is always rejected.
    AcceptNew,
}

impl Default for HostKeyPolicy {
    fn default() -> Self {
        Self::AcceptNew
    }
}

/// SSH authentication methods supported by the client.
#[derive(Clone, Debug)]
pub enum AuthMethod {
    Password(String),
    PublicKey {
        path: PathBuf,
        passphrase: Option<String>,
    },
    /// Try an SSH agent later; currently behaves like no authentication.
    None,
}

impl AuthMethod {
    pub fn label(&self) -> &'static str {
        match self {
            AuthMethod::Password(_) => "password",
            AuthMethod::PublicKey { .. } => "publickey",
            AuthMethod::None => "none",
        }
    }
}

#[derive(Clone, Debug)]
pub struct SshOptions {
    pub host: String,
    pub port: u16,
    pub user: String,
    pub auth: AuthMethod,
    pub cols: u16,
    pub rows: u16,
    pub term: String,
    /// Optional command to execute instead of an interactive shell.
    pub command: Option<String>,
    pub connect_timeout: Duration,
    pub keepalive: Option<Duration>,
    pub host_key_policy: HostKeyPolicy,
    /// Override the OpenSSH known_hosts location (useful for portable setups).
    pub known_hosts: Option<PathBuf>,
}

impl Default for SshOptions {
    fn default() -> Self {
        Self {
            host: String::new(),
            port: 22,
            user: String::new(),
            auth: AuthMethod::None,
            cols: 80,
            rows: 24,
            term: "xterm-256color".into(),
            command: None,
            connect_timeout: Duration::from_secs(20),
            keepalive: Some(Duration::from_secs(30)),
            host_key_policy: HostKeyPolicy::default(),
            known_hosts: None,
        }
    }
}

/// Information about the server identity, surfaced in the UI after connecting.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct HostKeyInfo {
    pub algorithm: String,
    pub fingerprint: String,
    /// Whether the key was already present in known_hosts.
    pub known: bool,
    pub accepted: bool,
}

#[derive(Clone, Debug)]
pub enum TransportEvent {
    Connected {
        host_key: HostKeyInfo,
        server: String,
    },
    Output(Bytes),
    Error(String),
    Closed {
        reason: String,
        exit_status: Option<u32>,
    },
}

enum Control {
    Data(Vec<u8>),
    Resize { cols: u32, rows: u32 },
    Close,
}

/// A live SSH session. Cheap to move around; all methods are synchronous and
/// send commands to the background session task.
pub struct SshTransport {
    control: mpsc::UnboundedSender<Control>,
    abort: tokio::task::AbortHandle,
    host_key: HostKeyInfo,
    server: String,
}

impl SshTransport {
    pub fn write(&self, data: &[u8]) -> Result<(), SshError> {
        self.control
            .send(Control::Data(data.to_vec()))
            .map_err(|_| SshError::Closed)
    }

    pub fn resize(&self, cols: u16, rows: u16) -> Result<(), SshError> {
        self.control
            .send(Control::Resize {
                cols: cols as u32,
                rows: rows as u32,
            })
            .map_err(|_| SshError::Closed)
    }

    pub fn close(&self) {
        let _ = self.control.send(Control::Close);
    }

    pub fn host_key(&self) -> &HostKeyInfo {
        &self.host_key
    }

    pub fn server(&self) -> &str {
        &self.server
    }
}

/// A live SFTP subsystem connection. The inner session must stay alive while
/// the SFTP session is used, so both are kept together.
pub struct SftpClient {
    pub sftp: russh_sftp::client::SftpSession,
    _session: Handle<SshHandler>,
    host_key: HostKeyInfo,
    server: String,
}

impl SftpClient {
    pub fn host_key(&self) -> &HostKeyInfo {
        &self.host_key
    }

    pub fn server(&self) -> &str {
        &self.server
    }
}

impl Drop for SshTransport {
    fn drop(&mut self) {
        let _ = self.control.send(Control::Close);
        self.abort.abort();
    }
}

struct SshHandler {
    host: String,
    port: u16,
    policy: HostKeyPolicy,
    known_hosts: Option<PathBuf>,
    record: Arc<Mutex<Option<HostKeyInfo>>>,
    failure: Arc<Mutex<Option<String>>>,
}

impl client::Handler for SshHandler {
    type Error = russh::Error;

    async fn check_server_key(
        &mut self,
        server_public_key: &PublicKeyOrCertificate,
    ) -> Result<bool, Self::Error> {
        let public_key = server_public_key.public_key();
        let fingerprint = public_key.fingerprint(HashAlg::Sha256).to_string();
        let algorithm = public_key.algorithm().to_string();

        let checked = match &self.known_hosts {
            Some(path) => check_known_hosts_path(&self.host, self.port, &public_key, path),
            None => check_known_hosts(&self.host, self.port, &public_key),
        };

        let (accepted, known, failure) = match checked {
            Ok(true) => (true, true, None),
            Ok(false) => match self.policy {
                HostKeyPolicy::Strict => (
                    false,
                    false,
                    Some(format!(
                        "unknown host key for {}:{} ({fingerprint}); add it to known_hosts or enable trust-on-first-use",
                        self.host, self.port
                    )),
                ),
                HostKeyPolicy::AcceptNew => {
                    let learn = match &self.known_hosts {
                        Some(path) => {
                            learn_known_hosts_path(&self.host, self.port, &public_key, path)
                        }
                        None => russh::keys::known_hosts::learn_known_hosts(
                            &self.host,
                            self.port,
                            &public_key,
                        ),
                    };
                    match learn {
                        Ok(()) => (true, false, None),
                        Err(error) => (
                            false,
                            false,
                            Some(format!("could not record host key: {error}")),
                        ),
                    }
                }
            },
            Err(error) => (
                false,
                true,
                Some(format!("host key verification failed: {error}")),
            ),
        };

        if let Some(message) = &failure {
            *self.failure.lock().unwrap() = Some(message.clone());
        }
        *self.record.lock().unwrap() = Some(HostKeyInfo {
            algorithm,
            fingerprint,
            known,
            accepted,
        });
        Ok(accepted)
    }
}

/// Connect to an SSH server and open an interactive session.
///
/// The returned transport owns a Tokio task; dropping it closes the session.
pub async fn connect(
    options: SshOptions,
    events: Sender<TransportEvent>,
) -> Result<SshTransport, SshError> {
    let record = Arc::new(Mutex::new(None));
    let failure = Arc::new(Mutex::new(None));

    let config = client::Config {
        keepalive_interval: options.keepalive,
        inactivity_timeout: None,
        nodelay: true,
        ..Default::default()
    };

    let handler = SshHandler {
        host: options.host.clone(),
        port: options.port,
        policy: options.host_key_policy,
        known_hosts: options.known_hosts.clone(),
        record: record.clone(),
        failure: failure.clone(),
    };

    let connect_future = client::connect(
        Arc::new(config),
        (options.host.as_str(), options.port),
        handler,
    );
    let mut session: Handle<SshHandler> =
        timeout(options.connect_timeout, connect_future)
            .await
            .map_err(|_| SshError::Timeout(options.connect_timeout))??;

    authenticate(&mut session, &options).await?;

    let channel = session.channel_open_session().await?;
    channel
        .request_pty(
            true,
            &options.term,
            options.cols as u32,
            options.rows as u32,
            0,
            0,
            &[],
        )
        .await?;

    // Ask the server to export a truecolor-capable TERM.
    let _ = channel.set_env(false, "COLORTERM", "truecolor").await;

    if let Some(command) = &options.command {
        channel.exec(true, command.as_bytes().to_vec()).await?;
    } else {
        channel.request_shell(true).await?;
    }

    let host_key = record.lock().unwrap().clone().unwrap_or_default();
    if !host_key.accepted {
        let message = failure
            .lock()
            .unwrap()
            .clone()
            .unwrap_or_else(|| "host key was rejected".to_string());
        return Err(SshError::HostKey(message));
    }

    let server = format!("{}@{}:{}", options.user, options.host, options.port);
    events
        .send(TransportEvent::Connected {
            host_key: host_key.clone(),
            server: server.clone(),
        })
        .await
        .ok();

    let (control_tx, control_rx) = mpsc::unbounded_channel();
    let task = tokio::spawn(run_session(
        session,
        channel,
        control_rx,
        events,
        options.user.clone(),
    ));

    Ok(SshTransport {
        control: control_tx,
        abort: task.abort_handle(),
        host_key,
        server,
    })
}

/// Open an SFTP subsystem connection using the same host-key and auth policy
/// as the interactive transport.
pub async fn connect_sftp(options: SshOptions) -> Result<SftpClient, SshError> {
    let record = Arc::new(Mutex::new(None));
    let failure = Arc::new(Mutex::new(None));
    let config = client::Config {
        keepalive_interval: options.keepalive,
        inactivity_timeout: None,
        nodelay: true,
        ..Default::default()
    };
    let handler = SshHandler {
        host: options.host.clone(),
        port: options.port,
        policy: options.host_key_policy,
        known_hosts: options.known_hosts.clone(),
        record: record.clone(),
        failure: failure.clone(),
    };
    let connect_future = client::connect(
        Arc::new(config),
        (options.host.as_str(), options.port),
        handler,
    );
    let mut session: Handle<SshHandler> =
        timeout(options.connect_timeout, connect_future)
            .await
            .map_err(|_| SshError::Timeout(options.connect_timeout))??;
    authenticate(&mut session, &options).await?;

    let host_key = record.lock().unwrap().clone().unwrap_or_default();
    if !host_key.accepted {
        let message = failure
            .lock()
            .unwrap()
            .clone()
            .unwrap_or_else(|| "host key was rejected".to_string());
        return Err(SshError::HostKey(message));
    }
    let server = format!("{}@{}:{}", options.user, options.host, options.port);
    let channel = session.channel_open_session().await?;
    channel.request_subsystem(true, "sftp").await?;
    let sftp = russh_sftp::client::SftpSession::new(channel.into_stream())
        .await
        .map_err(|error| SshError::Sftp(error.to_string()))?;
    Ok(SftpClient {
        sftp,
        _session: session,
        host_key,
        server,
    })
}

async fn authenticate(
    session: &mut Handle<SshHandler>,
    options: &SshOptions,
) -> Result<(), SshError> {
    let result = match &options.auth {
        AuthMethod::Password(password) => {
            session
                .authenticate_password(options.user.clone(), password.clone())
                .await?
        }
        AuthMethod::PublicKey { path, passphrase } => {
            let key = russh::keys::load_secret_key(path, passphrase.as_deref())?;
            let hash = session.best_supported_rsa_hash().await?.flatten();
            session
                .authenticate_publickey(
                    options.user.clone(),
                    russh::keys::PrivateKeyWithHashAlg::new(Arc::new(key), hash),
                )
                .await?
        }
        AuthMethod::None => AuthResult::Success,
    };

    match result {
        AuthResult::Success => Ok(()),
        AuthResult::Failure { .. } => Err(SshError::AuthenticationFailed),
    }
}

async fn run_session(
    session: Handle<SshHandler>,
    mut channel: Channel<Msg>,
    mut control: mpsc::UnboundedReceiver<Control>,
    events: Sender<TransportEvent>,
    _user: String,
) {
    let mut exit_status = None;

    loop {
        tokio::select! {
            command = control.recv() => {
                match command {
                    Some(Control::Data(data)) => {
                        if let Err(error) = channel.data_bytes(data).await {
                            let _ = events.send(TransportEvent::Error(error.to_string())).await;
                            break;
                        }
                    }
                    Some(Control::Resize { cols, rows }) => {
                        if let Err(error) = channel.window_change(cols, rows, 0, 0).await {
                            let _ = events.send(TransportEvent::Error(error.to_string())).await;
                        }
                    }
                    Some(Control::Close) | None => {
                        let _ = channel.close().await;
                        break;
                    }
                }
            }
            message = channel.wait() => {
                match message {
                    Some(ChannelMsg::Data { data }) => {
                        if events.send(TransportEvent::Output(data)).await.is_err() {
                            break;
                        }
                    }
                    Some(ChannelMsg::ExtendedData { data, .. }) => {
                        if events.send(TransportEvent::Output(data)).await.is_err() {
                            break;
                        }
                    }
                    Some(ChannelMsg::ExitStatus { exit_status: status }) => {
                        exit_status = Some(status);
                    }
                    Some(ChannelMsg::Eof) => {
                        // `exit`/Ctrl+D can deliver Eof before ExitStatus.
                        // Drain briefly so a clean exit is distinguishable
                        // from a dropped connection.
                        let deadline = tokio::time::sleep(Duration::from_millis(350));
                        tokio::pin!(deadline);
                        loop {
                            tokio::select! {
                                _ = &mut deadline => break,
                                message = channel.wait() => match message {
                                    Some(ChannelMsg::ExitStatus { exit_status: status }) => {
                                        exit_status = Some(status);
                                        break;
                                    }
                                    Some(ChannelMsg::Close) | None => break,
                                    _ => {}
                                }
                            }
                        }
                        break;
                    }
                    Some(ChannelMsg::Close) | None => break,
                    _ => {}
                }
            }
        }
    }

    let _ = session
        .disconnect(russh::Disconnect::ByApplication, "", "en")
        .await;
    let _ = events
        .send(TransportEvent::Closed {
            reason: "connection closed".into(),
            exit_status,
        })
        .await;
}
