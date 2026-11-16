use std::ops::Deref;
use std::rc::Rc;

use xcord_store::{HighlightRule, SessionProfile, Settings, Snippet};

/// CRUD and secret-management facade used by the UI and by workspace plugins.
///
/// The implementation lives in the `sessions` plugin and is backed by
/// `xcord-store`; plugins only see this trait.
pub trait SessionService {
    fn list(&self) -> Vec<SessionProfile>;
    fn groups(&self) -> Vec<String>;
    fn create_group(&self, name: &str) -> Result<(), String>;
    fn rename_group(&self, old: &str, new: &str) -> Result<(), String>;
    fn move_group(&self, name: &str, target: Option<&str>, after: bool) -> Result<(), String>;
    fn move_session(
        &self,
        id: &str,
        group: &str,
        target: Option<&str>,
        after: bool,
    ) -> Result<(), String>;
    fn get(&self, id: &str) -> Option<SessionProfile>;
    fn save(&self, profile: SessionProfile) -> Result<(), String>;
    fn delete(&self, id: &str) -> Result<(), String>;

    fn quick_access(&self) -> Vec<String>;
    fn pin_quick_access(&self, id: &str) -> Result<(), String>;
    fn unpin_quick_access(&self, id: &str) -> Result<(), String>;
    fn move_quick_access(&self, id: &str, target: Option<&str>, after: bool) -> Result<(), String>;

    fn snippets(&self) -> Vec<Snippet>;
    fn save_snippet(&self, snippet: Snippet) -> Result<(), String>;
    fn delete_snippet(&self, id: &str) -> Result<(), String>;

    fn highlights(&self) -> Vec<HighlightRule>;
    fn save_highlight(&self, rule: HighlightRule) -> Result<(), String>;
    fn delete_highlight(&self, id: &str) -> Result<(), String>;

    fn settings(&self) -> Settings;
    fn save_settings(&self, settings: Settings) -> Result<(), String>;

    /// Retrieve a stored password for a profile (OS keychain).
    fn password(&self, profile: &SessionProfile) -> Option<String>;
    /// Store a password and update the profile's `secret_id`.
    fn store_password(&self, profile: &SessionProfile, password: &str) -> Result<(), String>;
    fn clear_password(&self, profile: &SessionProfile) -> Result<(), String>;

    /// Retrieve a stored passphrase for a public-key profile.
    fn passphrase(&self, profile: &SessionProfile) -> Option<String>;
    fn store_passphrase(&self, profile: &SessionProfile, passphrase: &str) -> Result<(), String>;
    fn clear_passphrase(&self, profile: &SessionProfile) -> Result<(), String>;

    /// API key used by the AI copilot, stored in the platform vault.
    fn ai_api_key(&self) -> Option<String>;
    fn store_ai_api_key(&self, key: &str) -> Result<(), String>;
    fn clear_ai_api_key(&self) -> Result<(), String>;

    fn export(&self, path: &str) -> Result<(), String>;
    fn import(&self, path: &str) -> Result<(), String>;
}

/// Service wrapper: `ctx.require::<Sessions>(services::SESSIONS)`.
#[derive(Clone)]
pub struct Sessions(pub Rc<dyn SessionService>);

impl Deref for Sessions {
    type Target = dyn SessionService;
    fn deref(&self) -> &Self::Target {
        &*self.0
    }
}
