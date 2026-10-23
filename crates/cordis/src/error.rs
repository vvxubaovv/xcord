use std::cell::RefCell;
use std::fmt;

/// Errors produced by the plugin kernel.
#[derive(Debug, Clone)]
pub enum Error {
    /// A plugin asked for a service that is not registered.
    MissingService {
        name: &'static str,
        plugin: Option<&'static str>,
    },
    /// A service exists under this name but with a different Rust type.
    ServiceTypeMismatch {
        name: &'static str,
        expected: &'static str,
    },
    /// Two plugins tried to own the same service name.
    DuplicateService { name: &'static str, owner: String },
    /// A plugin tried to withdraw a service another plugin owns.
    NotServiceOwner { name: &'static str },
    /// A plugin id no longer exists.
    UnknownPlugin(u64),
    /// Plugin application failed.
    PluginFailed { name: &'static str, message: String },
    /// The plugin scope has already been disposed.
    Disposed,
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Error::MissingService { name, plugin } => match plugin {
                Some(plugin) => write!(
                    f,
                    "plugin `{plugin}` requires service `{name}`, which is not registered"
                ),
                None => write!(f, "service `{name}` is not registered"),
            },
            Error::ServiceTypeMismatch { name, expected } => {
                write!(f, "service `{name}` is not of type `{expected}`")
            }
            Error::DuplicateService { name, owner } => {
                write!(f, "service `{name}` is already owned by `{owner}`")
            }
            Error::NotServiceOwner { name } => {
                write!(f, "service `{name}` is owned by another plugin")
            }
            Error::UnknownPlugin(id) => write!(f, "unknown plugin id {id}"),
            Error::PluginFailed { name, message } => {
                write!(f, "plugin `{name}` failed: {message}")
            }
            Error::Disposed => write!(f, "plugin scope is disposed"),
        }
    }
}

impl std::error::Error for Error {}

pub type Result<T> = std::result::Result<T, Error>;

/// A typed event carried by the [`EventBus`](crate::EventBus).
///
/// Any `'static` type is an event; this trait only documents the contract and
/// provides the blanket implementation.
pub trait Event: 'static {}

impl<T: 'static> Event for T {}

/// Severity of a kernel diagnostic.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum DiagnosticLevel {
    Info,
    Warn,
    Error,
}

impl fmt::Display for DiagnosticLevel {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            DiagnosticLevel::Info => f.write_str("info"),
            DiagnosticLevel::Warn => f.write_str("warn"),
            DiagnosticLevel::Error => f.write_str("error"),
        }
    }
}

/// A timestamped message from the kernel. Diagnostics are kept in memory so a
/// plugin-manager UI can show why a plugin did not start.
#[derive(Debug, Clone)]
pub struct Diagnostic {
    pub level: DiagnosticLevel,
    pub plugin: Option<String>,
    pub message: String,
    pub sequence: u64,
}

/// Emitted whenever a plugin changes lifecycle state.
#[derive(Debug, Clone)]
pub struct PluginStatusEvent {
    pub id: crate::PluginId,
    pub name: &'static str,
    pub state: crate::PluginState,
    pub error: Option<String>,
}

/// Emitted when a service is provided or withdrawn.
#[derive(Debug, Clone)]
pub struct ServiceEvent {
    pub name: &'static str,
    pub registered: bool,
    pub owner: crate::PluginId,
}

thread_local! {
    static DIAGNOSTIC_SEQUENCE: RefCell<u64> = const { RefCell::new(0) };
}

pub(crate) fn next_diagnostic_sequence() -> u64 {
    DIAGNOSTIC_SEQUENCE.with(|seq| {
        let mut seq = seq.borrow_mut();
        *seq += 1;
        *seq
    })
}
