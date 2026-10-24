//! Portable, human-readable configuration and an OS-backed secret vault.
//!
//! Xcord deliberately avoids a proprietary database: the workspace is a TOML
//! file users can read, diff and version. Passwords never live in that file;
//! they are stored in the platform keychain (Windows Credential Manager,
//! macOS Keychain, Secret Service on Linux) when available.

use std::cell::RefCell;
use std::fmt;
use std::fs;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use thiserror::Error;

#[derive(Debug, Error)]
pub enum StoreError {
    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),
    #[error("could not parse configuration: {0}")]
    Parse(#[from] toml::de::Error),
    #[error("could not serialize configuration: {0}")]
    Serialize(#[from] toml::ser::Error),
    #[error("secret vault error: {0}")]
    Vault(String),
}

pub type Result<T> = std::result::Result<T, StoreError>;

// ---------------------------------------------------------------------------
// Model

/// How to authenticate a session. `secret_id` references the OS keychain; it
/// is never the password itself.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum AuthKind {
    Password {
        #[serde(default)]
        save: bool,
        #[serde(default)]
        secret_id: Option<String>,
    },
    PublicKey {
        #[serde(default)]
        path: String,
        #[serde(default)]
        save_passphrase: bool,
        #[serde(default)]
        secret_id: Option<String>,
    },
    Agent,
    None,
}

impl Default for AuthKind {
    fn default() -> Self {
        Self::Password {
            save: false,
            secret_id: None,
        }
    }
}

impl AuthKind {
    pub fn label(&self) -> &'static str {
        match self {
            AuthKind::Password { .. } => "Password",
            AuthKind::PublicKey { .. } => "Public key",
            AuthKind::Agent => "SSH agent",
            AuthKind::None => "None",
        }
    }
}

#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq, Default)]
#[serde(rename_all = "snake_case")]
pub enum HostKeyPolicy {
    Strict,
    #[default]
    AcceptNew,
}

/// A saved connection.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct SessionProfile {
    pub id: String,
    pub name: String,
    #[serde(default)]
    pub group: String,
    pub host: String,
    #[serde(default = "default_port")]
    pub port: u16,
    #[serde(default)]
    pub user: String,
    #[serde(default)]
    pub auth: AuthKind,
    #[serde(default)]
    pub startup_command: Option<String>,
    #[serde(default = "default_term")]
    pub term: String,
    #[serde(default)]
    pub color_scheme: Option<String>,
    #[serde(default = "default_keepalive")]
    pub keepalive_secs: u64,
    /// Session id of a jump host. Forwarded through a direct-tcpip channel in a
    /// future release; stored now so profiles are portable.
    #[serde(default)]
    pub proxy_jump: Option<String>,
    #[serde(default)]
    pub logging: bool,
    #[serde(default)]
    pub tags: Vec<String>,
}

fn default_port() -> u16 {
    22
}
fn default_term() -> String {
    "xterm-256color".into()
}
fn default_keepalive() -> u64 {
    30
}

impl SessionProfile {
    pub fn new(name: impl Into<String>) -> Self {
        Self {
            id: uuid::Uuid::new_v4().to_string(),
            name: name.into(),
            group: "Sessions".into(),
            host: String::new(),
            port: 22,
            user: String::new(),
            auth: AuthKind::default(),
            startup_command: None,
            term: default_term(),
            color_scheme: None,
            keepalive_secs: 30,
            proxy_jump: None,
            logging: false,
            tags: Vec::new(),
        }
    }

    pub fn endpoint(&self) -> String {
        format!("{}@{}:{}", self.user, self.host, self.port)
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Snippet {
    pub id: String,
    pub name: String,
    pub command: String,
    #[serde(default = "default_true")]
    pub send_enter: bool,
}

fn default_true() -> bool {
    true
}

impl Snippet {
    pub fn new(name: impl Into<String>, command: impl Into<String>) -> Self {
        Self {
            id: uuid::Uuid::new_v4().to_string(),
            name: name.into(),
            command: command.into(),
            send_enter: true,
        }
    }
}

/// A regex highlight rule. Colors are `#rrggbb` strings so themes stay textual.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct HighlightRule {
    pub id: String,
    pub name: String,
    pub pattern: String,
    #[serde(default)]
    pub foreground: Option<String>,
    #[serde(default)]
    pub background: Option<String>,
    #[serde(default = "default_true")]
    pub enabled: bool,
}

impl HighlightRule {
    pub fn new(name: impl Into<String>, pattern: impl Into<String>) -> Self {
        Self {
            id: uuid::Uuid::new_v4().to_string(),
            name: name.into(),
            pattern: pattern.into(),
            foreground: Some("#f5c06f".into()),
            background: Some("#3a2a12".into()),
            enabled: true,
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(default)]
pub struct Settings {
    pub font_family: String,
    pub font_size: f32,
    pub line_height: f32,
    pub scrollback: usize,
    pub theme: String,
    pub copy_on_select: bool,
    pub confirm_close: bool,
    pub keepalive_secs: u64,
    pub host_key_policy: HostKeyPolicy,
    pub logging: bool,
    pub log_dir: String,
    pub sidebar_width: f32,
    pub compose_bar: bool,
    /// UI locale. Defaults to Simplified Chinese for existing configs.
    pub language: String,
    pub cursor_blink: bool,
    pub bell: bool,
    pub highlight: bool,
    pub last_session: Option<String>,

    // MCP server (localhost, JSON-RPC over HTTP)
    pub mcp_enabled: bool,
    pub mcp_port: u16,
    /// Generated on first launch. Empty means "generate and persist".
    pub mcp_token: String,

    // AI copilot (OpenAI-compatible chat completions)
    pub ai_base_url: String,
    pub ai_model: String,
    pub ai_allow_writes: bool,
    pub ai_include_terminal: bool,
    pub ai_system_prompt: String,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            font_family: default_font(),
            font_size: 14.0,
            line_height: 1.35,
            scrollback: 10_000,
            theme: "xcord-dark".into(),
            copy_on_select: false,
            confirm_close: false,
            keepalive_secs: 30,
            host_key_policy: HostKeyPolicy::AcceptNew,
            logging: false,
            log_dir: String::new(),
            sidebar_width: 260.0,
            compose_bar: true,
            language: "zh-CN".into(),
            cursor_blink: true,
            bell: true,
            highlight: true,
            last_session: None,
            mcp_enabled: true,
            mcp_port: 8765,
            mcp_token: String::new(),
            ai_base_url: "https://api.deepseek.com/v1".into(),
            ai_model: "deepseek-chat".into(),
            ai_allow_writes: false,
            ai_include_terminal: true,
            ai_system_prompt: default_ai_system_prompt(),
        }
    }
}

fn default_ai_system_prompt() -> String {
    "You are Xcord Copilot, an SSH operations assistant embedded in a terminal workspace. \
     Use the provided MCP tools to inspect sessions, tabs, terminal output and plugins before \
     making claims. Prefer read-only tools. Never run destructive commands unless the user \
     explicitly asks for them. Keep answers concise; put shell commands in fenced code blocks."
        .into()
}

fn default_font() -> String {
    if cfg!(windows) {
        "Consolas".into()
    } else if cfg!(target_os = "macos") {
        "Menlo".into()
    } else {
        "DejaVu Sans Mono".into()
    }
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct WorkspaceConfig {
    pub version: u32,
    pub sessions: Vec<SessionProfile>,
    pub groups: Vec<String>,
    pub snippets: Vec<Snippet>,
    pub highlights: Vec<HighlightRule>,
    pub settings: Settings,
}

impl WorkspaceConfig {
    pub fn defaults() -> Self {
        Self {
            version: 1,
            sessions: Vec::new(),
            groups: vec!["Sessions".into()],
            snippets: vec![
                Snippet::new("List files", "ls -lah"),
                Snippet::new("Disk usage", "df -h"),
                Snippet::new("Tail syslog", "tail -f /var/log/syslog"),
            ],
            highlights: vec![
                HighlightRule {
                    id: uuid::Uuid::new_v4().to_string(),
                    name: "Errors".into(),
                    pattern: "(?i)error|failed|fatal|panic".into(),
                    foreground: Some("#ff8f8f".into()),
                    background: Some("#3a1515".into()),
                    enabled: true,
                },
                HighlightRule {
                    id: uuid::Uuid::new_v4().to_string(),
                    name: "Warnings".into(),
                    pattern: "(?i)warn|deprecated".into(),
                    foreground: Some("#f5c06f".into()),
                    background: Some("#3a2a12".into()),
                    enabled: true,
                },
            ],
            settings: Settings::default(),
        }
    }

    pub fn find_session(&self, id: &str) -> Option<&SessionProfile> {
        self.sessions.iter().find(|session| session.id == id)
    }

    pub fn find_session_mut(&mut self, id: &str) -> Option<&mut SessionProfile> {
        self.sessions.iter_mut().find(|session| session.id == id)
    }
}

// ---------------------------------------------------------------------------
// Paths

#[derive(Clone, Debug)]
pub struct Paths {
    pub config_dir: PathBuf,
    pub config_file: PathBuf,
    pub logs_dir: PathBuf,
}

impl Paths {
    pub fn discover() -> Self {
        let config_dir = config_dir();
        Self {
            config_file: config_dir.join("config.toml"),
            logs_dir: config_dir.join("logs"),
            config_dir,
        }
    }

    pub fn with_root(root: impl Into<PathBuf>) -> Self {
        let config_dir = root.into();
        Self {
            config_file: config_dir.join("config.toml"),
            logs_dir: config_dir.join("logs"),
            config_dir,
        }
    }
}

pub fn config_dir() -> PathBuf {
    if let Ok(value) = std::env::var("XCORD_CONFIG_DIR") {
        return PathBuf::from(value);
    }
    if cfg!(windows) {
        if let Ok(appdata) = std::env::var("APPDATA") {
            return PathBuf::from(appdata).join("xcord");
        }
    }
    if cfg!(target_os = "macos") {
        if let Some(home) = home::home_dir() {
            return home.join("Library/Application Support/xcord");
        }
    }
    if let Ok(xdg) = std::env::var("XDG_CONFIG_HOME") {
        return PathBuf::from(xdg).join("xcord");
    }
    home::home_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join(".config/xcord")
}

// ---------------------------------------------------------------------------
// Store

/// The configuration store. Reads once at boot, writes atomically on save.
pub struct Store {
    paths: Paths,
    config: RefCell<WorkspaceConfig>,
    warnings: RefCell<Vec<String>>,
}

impl Store {
    /// Open the default location, creating a commented default config if this
    /// is the first launch.
    pub fn open() -> Self {
        Self::open_at(Paths::discover())
    }

    pub fn open_at(paths: Paths) -> Self {
        let mut warnings = Vec::new();
        let config = if paths.config_file.exists() {
            match fs::read_to_string(&paths.config_file) {
                Ok(text) => match toml::from_str::<WorkspaceConfig>(&text) {
                    Ok(mut config) => {
                        if config.version == 0 {
                            config.version = 1;
                        }
                        config
                    }
                    Err(error) => {
                        let backup = paths.config_file.with_extension("toml.corrupt");
                        let _ = fs::copy(&paths.config_file, &backup);
                        warnings.push(format!(
                            "config.toml could not be parsed ({error}); a backup was written to {}",
                            backup.display()
                        ));
                        WorkspaceConfig::defaults()
                    }
                },
                Err(error) => {
                    warnings.push(format!("could not read config.toml: {error}"));
                    WorkspaceConfig::defaults()
                }
            }
        } else {
            WorkspaceConfig::defaults()
        };

        let store = Self {
            paths,
            config: RefCell::new(config),
            warnings: RefCell::new(warnings),
        };
        if !store.paths.config_file.exists() {
            let _ = store.save();
        }
        store
    }

    pub fn paths(&self) -> &Paths {
        &self.paths
    }

    pub fn warnings(&self) -> Vec<String> {
        self.warnings.borrow().clone()
    }

    pub fn config(&self) -> WorkspaceConfig {
        self.config.borrow().clone()
    }

    pub fn settings(&self) -> Settings {
        self.config.borrow().settings.clone()
    }

    pub fn session(&self, id: &str) -> Option<SessionProfile> {
        self.config.borrow().find_session(id).cloned()
    }

    pub fn sessions(&self) -> Vec<SessionProfile> {
        self.config.borrow().sessions.clone()
    }

    pub fn snippets(&self) -> Vec<Snippet> {
        self.config.borrow().snippets.clone()
    }

    pub fn highlights(&self) -> Vec<HighlightRule> {
        self.config.borrow().highlights.clone()
    }

    pub fn update(&self, mutate: impl FnOnce(&mut WorkspaceConfig)) -> Result<()> {
        {
            let mut config = self.config.borrow_mut();
            mutate(&mut config);
        }
        self.save()
    }

    pub fn upsert_session(&self, session: SessionProfile) -> Result<()> {
        self.update(|config| {
            match config
                .sessions
                .iter_mut()
                .find(|existing| existing.id == session.id)
            {
                Some(existing) => *existing = session,
                None => config.sessions.push(session),
            }
        })
    }

    pub fn delete_session(&self, id: &str) -> Result<()> {
        self.update(|config| config.sessions.retain(|session| session.id != id))
    }

    pub fn upsert_snippet(&self, snippet: Snippet) -> Result<()> {
        self.update(
            |config| match config.snippets.iter_mut().find(|s| s.id == snippet.id) {
                Some(existing) => *existing = snippet,
                None => config.snippets.push(snippet),
            },
        )
    }

    pub fn delete_snippet(&self, id: &str) -> Result<()> {
        self.update(|config| config.snippets.retain(|snippet| snippet.id != id))
    }

    pub fn upsert_highlight(&self, rule: HighlightRule) -> Result<()> {
        self.update(
            |config| match config.highlights.iter_mut().find(|r| r.id == rule.id) {
                Some(existing) => *existing = rule,
                None => config.highlights.push(rule),
            },
        )
    }

    pub fn delete_highlight(&self, id: &str) -> Result<()> {
        self.update(|config| config.highlights.retain(|rule| rule.id != id))
    }

    /// Persist atomically: write a temporary file, keep a `.bak`, then rename.
    pub fn save(&self) -> Result<()> {
        fs::create_dir_all(&self.paths.config_dir)?;
        let text = toml::to_string_pretty(&*self.config.borrow())?;
        let temporary = self.paths.config_file.with_extension("toml.tmp");
        fs::write(&temporary, text)?;
        if self.paths.config_file.exists() {
            let backup = self.paths.config_file.with_extension("toml.bak");
            let _ = fs::copy(&self.paths.config_file, &backup);
        }
        // Windows does not replace an existing file with rename, so remove it
        // first. Losing the old file for a microsecond is acceptable because a
        // `.bak` exists.
        if self.paths.config_file.exists() {
            fs::remove_file(&self.paths.config_file)?;
        }
        fs::rename(&temporary, &self.paths.config_file)?;
        Ok(())
    }

    pub fn export_to(&self, path: &Path) -> Result<()> {
        let config = self.config.borrow();
        if path.extension().and_then(|value| value.to_str()) == Some("json") {
            let text = serde_json::to_string_pretty(&*config)
                .map_err(|error| StoreError::Vault(error.to_string()))?;
            fs::write(path, text)?;
        } else {
            fs::write(path, toml::to_string_pretty(&*config)?)?;
        }
        Ok(())
    }

    pub fn import_from(&self, path: &Path) -> Result<()> {
        let text = fs::read_to_string(path)?;
        let config = if path.extension().and_then(|value| value.to_str()) == Some("json") {
            serde_json::from_str::<WorkspaceConfig>(&text)
                .map_err(|error| StoreError::Vault(error.to_string()))?
        } else {
            toml::from_str::<WorkspaceConfig>(&text)?
        };
        *self.config.borrow_mut() = config;
        self.save()
    }
}

impl fmt::Debug for Store {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Store")
            .field("path", &self.paths.config_file)
            .field("sessions", &self.config.borrow().sessions.len())
            .finish()
    }
}

// ---------------------------------------------------------------------------
// Secret vault

/// OS keychain wrapper. On Windows this is Credential Manager.
pub struct Vault {
    service: String,
}

impl Default for Vault {
    fn default() -> Self {
        Self::new("xcord")
    }
}

impl Vault {
    pub fn new(service: impl Into<String>) -> Self {
        Self {
            service: service.into(),
        }
    }

    #[cfg(windows)]
    fn entry(&self, key: &str) -> std::result::Result<keyring::Entry, StoreError> {
        keyring::Entry::new(&self.service, key)
            .map_err(|error| StoreError::Vault(error.to_string()))
    }

    #[cfg(windows)]
    pub fn get(&self, key: &str) -> Result<Option<String>> {
        match self.entry(key)?.get_password() {
            Ok(secret) => Ok(Some(secret)),
            Err(keyring::Error::NoEntry) => Ok(None),
            Err(error) => Err(StoreError::Vault(error.to_string())),
        }
    }

    #[cfg(windows)]
    pub fn set(&self, key: &str, value: &str) -> Result<()> {
        self.entry(key)?
            .set_password(value)
            .map_err(|error| StoreError::Vault(error.to_string()))
    }

    #[cfg(windows)]
    pub fn delete(&self, key: &str) -> Result<()> {
        match self.entry(key)?.delete_credential() {
            Ok(()) | Err(keyring::Error::NoEntry) => Ok(()),
            Err(error) => Err(StoreError::Vault(error.to_string())),
        }
    }

    // Portable fallback. The file is permission-restricted, but not encrypted;
    // Linux/macOS builds should enable the platform keychain backend.
    #[cfg(not(windows))]
    pub fn get(&self, key: &str) -> Result<Option<String>> {
        Ok(self.read_file()?.remove(key))
    }

    #[cfg(not(windows))]
    pub fn set(&self, key: &str, value: &str) -> Result<()> {
        let mut map = self.read_file()?;
        map.insert(key.to_string(), value.to_string());
        self.write_file(&map)
    }

    #[cfg(not(windows))]
    pub fn delete(&self, key: &str) -> Result<()> {
        let mut map = self.read_file()?;
        map.remove(key);
        self.write_file(&map)
    }

    #[cfg(not(windows))]
    fn vault_file(&self) -> PathBuf {
        config_dir().join("secrets.toml")
    }

    #[cfg(not(windows))]
    fn read_file(&self) -> Result<std::collections::BTreeMap<String, String>> {
        let path = self.vault_file();
        if !path.exists() {
            return Ok(Default::default());
        }
        let text = fs::read_to_string(path)?;
        toml::from_str(&text).map_err(StoreError::Parse)
    }

    #[cfg(not(windows))]
    fn write_file(&self, map: &std::collections::BTreeMap<String, String>) -> Result<()> {
        let path = self.vault_file();
        fs::create_dir_all(path.parent().unwrap_or(Path::new(".")))?;
        fs::write(&path, toml::to_string(map)?)?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let _ = fs::set_permissions(&path, fs::Permissions::from_mode(0o600));
        }
        Ok(())
    }
}

impl fmt::Debug for Vault {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Vault")
            .field("service", &self.service)
            .finish()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roundtrip_workspace() {
        let dir = std::env::temp_dir().join(format!("xcord-test-{}", uuid::Uuid::new_v4()));
        let paths = Paths::with_root(&dir);
        let store = Store::open_at(paths);
        let mut session = SessionProfile::new("web");
        session.host = "example.com".into();
        session.user = "root".into();
        store.upsert_session(session.clone()).unwrap();

        let reopened = Store::open_at(Paths::with_root(&dir));
        assert_eq!(reopened.sessions().len(), 1);
        assert_eq!(reopened.sessions()[0].name, "web");
        assert_eq!(reopened.sessions()[0].host, "example.com");

        reopened.delete_session(&session.id).unwrap();
        assert!(reopened.sessions().is_empty());
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn corrupt_config_is_backed_up() {
        let dir = std::env::temp_dir().join(format!("xcord-test-{}", uuid::Uuid::new_v4()));
        fs::create_dir_all(&dir).unwrap();
        let paths = Paths::with_root(&dir);
        fs::write(&paths.config_file, "this is not toml !!!").unwrap();
        let store = Store::open_at(paths.clone());
        assert!(!store.warnings().is_empty());
        assert!(paths.config_file.with_extension("toml.corrupt").exists());
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn settings_roundtrip() {
        let dir = std::env::temp_dir().join(format!("xcord-test-{}", uuid::Uuid::new_v4()));
        let store = Store::open_at(Paths::with_root(&dir));
        store
            .update(|config| {
                config.settings.font_size = 18.0;
                config.settings.theme = "light".into();
            })
            .unwrap();
        let reopened = Store::open_at(Paths::with_root(&dir));
        assert_eq!(reopened.settings().font_size, 18.0);
        assert_eq!(reopened.settings().theme, "light");
        let _ = fs::remove_dir_all(&dir);
    }
}
