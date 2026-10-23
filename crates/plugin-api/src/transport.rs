use std::cell::RefCell;
use std::collections::HashMap;
use std::ops::Deref;
use std::path::PathBuf;
use std::rc::Rc;
use std::time::Duration;

use bytes::Bytes;
use serde::{Deserialize, Serialize};

/// Credentials for one connection attempt. Secrets come from the OS keychain
/// or from a prompt; they are never persisted in the workspace file.
#[derive(Clone, Debug)]
pub enum AuthRequest {
    Password(String),
    PublicKey {
        path: PathBuf,
        passphrase: Option<String>,
    },
    None,
}

impl AuthRequest {
    pub fn label(&self) -> &'static str {
        match self {
            AuthRequest::Password(_) => "password",
            AuthRequest::PublicKey { .. } => "publickey",
            AuthRequest::None => "none",
        }
    }
}

/// A protocol-independent connection request.
#[derive(Clone, Debug)]
pub struct ConnectSpec {
    pub host: String,
    pub port: u16,
    pub user: String,
    pub auth: AuthRequest,
    pub cols: u16,
    pub rows: u16,
    pub term: String,
    pub command: Option<String>,
    pub connect_timeout: Duration,
    pub keepalive: Option<Duration>,
    pub accept_new_host_keys: bool,
    pub known_hosts: Option<PathBuf>,
}

/// Summary of the server host key shown in the status bar.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct HostKeySummary {
    pub algorithm: String,
    pub fingerprint: String,
    pub known: bool,
    pub accepted: bool,
}

#[derive(Clone, Debug)]
pub struct ConnectionInfo {
    pub server: String,
    pub kind: String,
    pub host_key: HostKeySummary,
}

/// A connected bidirectional channel. Implementations are `Send` because they
/// are created on the I/O runtime and then moved to the UI thread.
pub trait Transport: Send + 'static {
    fn write(&self, data: &[u8]) -> Result<(), String>;
    fn resize(&self, cols: u16, rows: u16);
    fn close(&self);
    fn description(&self) -> String;
}

/// Events from a transport.
///
/// `Connected` carries the transport itself, so the UI thread can own the
/// handle without ever blocking on a future.
pub enum TransportEvent {
    Connected {
        transport: Box<dyn Transport>,
        info: ConnectionInfo,
    },
    Output(Bytes),
    Error(String),
    Closed {
        reason: String,
        exit_status: Option<u32>,
    },
}

impl std::fmt::Debug for TransportEvent {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            TransportEvent::Connected { info, .. } => f
                .debug_struct("Connected")
                .field("server", &info.server)
                .field("host_key", &info.host_key.fingerprint)
                .finish(),
            TransportEvent::Output(data) => f
                .debug_struct("Output")
                .field("bytes", &data.len())
                .finish(),
            TransportEvent::Error(message) => f.debug_tuple("Error").field(message).finish(),
            TransportEvent::Closed {
                reason,
                exit_status,
            } => f
                .debug_struct("Closed")
                .field("reason", reason)
                .field("exit_status", exit_status)
                .finish(),
        }
    }
}

/// A connection provider (SSH today, Telnet/serial/local later).
///
/// `connect` must return quickly. The provider spawns work on the supplied
/// runtime and reports progress through `events`.
pub trait TransportProvider: 'static {
    fn kind(&self) -> &'static str;
    fn connect(
        &self,
        spec: ConnectSpec,
        events: async_channel::Sender<TransportEvent>,
    ) -> Result<(), String>;
}

/// Registry of transport providers, provided by the `transports` plugin.
#[derive(Default)]
pub struct TransportRegistry {
    providers: RefCell<HashMap<&'static str, Rc<dyn TransportProvider>>>,
}

impl TransportRegistry {
    pub fn register(&self, provider: Rc<dyn TransportProvider>) -> Result<(), String> {
        let kind = provider.kind();
        let mut providers = self.providers.borrow_mut();
        if providers.contains_key(kind) {
            return Err(format!("transport `{kind}` is already registered"));
        }
        providers.insert(kind, provider);
        Ok(())
    }

    pub fn unregister(&self, kind: &'static str) {
        self.providers.borrow_mut().remove(kind);
    }

    pub fn get(&self, kind: &str) -> Option<Rc<dyn TransportProvider>> {
        self.providers.borrow().get(kind).cloned()
    }

    pub fn kinds(&self) -> Vec<&'static str> {
        self.providers.borrow().keys().copied().collect()
    }

    pub fn has(&self, kind: &str) -> bool {
        self.providers.borrow().contains_key(kind)
    }
}

/// Service wrapper: `ctx.require::<Transports>(services::TRANSPORTS)`.
#[derive(Clone)]
pub struct Transports(pub Rc<TransportRegistry>);

impl Deref for Transports {
    type Target = TransportRegistry;
    fn deref(&self) -> &Self::Target {
        &self.0
    }
}
