//! SFTP file-transfer facade (Xftp-style plugin surface).

use std::ops::Deref;
use std::rc::Rc;

use tokio::sync::broadcast;

use crate::ui::NoticeLevel;

/// One file or directory in either pane.
#[derive(Clone, Debug, Default)]
pub struct FtpEntry {
    pub name: String,
    pub path: String,
    pub is_dir: bool,
    pub size: u64,
    pub modified: Option<u64>,
}

/// Snapshot of one SFTP profile plus the shared local pane.
#[derive(Clone, Debug, Default)]
pub struct FtpSnapshot {
    pub profile_id: String,
    pub profile_name: String,
    pub host: String,
    pub connected: bool,
    pub loading: bool,
    pub busy: Option<String>,
    pub error: Option<String>,
    pub remote_path: String,
    pub remote_entries: Vec<FtpEntry>,
    pub local_path: String,
    pub local_entries: Vec<FtpEntry>,
}

#[derive(Clone, Debug)]
pub enum FtpEvent {
    Changed {
        profile_id: String,
    },
    Downloaded {
        profile_id: String,
        paths: Vec<String>,
    },
    Notice {
        level: NoticeLevel,
        message: String,
    },
}

pub trait FtpService {
    fn connect(&self, profile_id: &str) -> Result<(), String>;
    fn disconnect(&self, profile_id: &str);
    fn navigate_remote(&self, profile_id: &str, path: &str);
    fn navigate_local(&self, path: &str);
    fn refresh(&self, profile_id: &str);

    /// Upload selected local paths into the remote current directory.
    fn upload(&self, profile_id: &str, local_paths: Vec<String>) -> Result<(), String>;
    /// Download selected remote paths into the local current directory.
    fn download(&self, profile_id: &str, remote_paths: Vec<String>) -> Result<(), String>;
    fn create_dir(&self, profile_id: &str, name: &str) -> Result<(), String>;
    fn delete_remote(&self, profile_id: &str, paths: Vec<String>) -> Result<(), String>;

    fn snapshot(&self, profile_id: &str) -> Option<FtpSnapshot>;
    fn subscribe(&self) -> broadcast::Receiver<FtpEvent>;
}

/// Service wrapper: `ctx.require::<Ftp>(services::FTP)`.
#[derive(Clone)]
pub struct Ftp(pub Rc<dyn FtpService>);

impl Deref for Ftp {
    type Target = dyn FtpService;
    fn deref(&self) -> &Self::Target {
        &*self.0
    }
}
