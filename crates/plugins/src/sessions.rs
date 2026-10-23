use std::path::PathBuf;
use std::rc::Rc;

use xcord_cordis::{Context, Plugin, PluginMeta, Result};
use xcord_plugin_api::{services, SessionService, Sessions};
use xcord_store::{AuthKind, HighlightRule, SessionProfile, Settings, Snippet, Store, Vault};

/// The concrete `SessionService`: TOML workspace + OS keychain.
pub struct SessionManager {
    store: Rc<Store>,
    vault: Rc<Vault>,
}

impl SessionManager {
    pub fn new() -> Self {
        Self {
            store: Rc::new(Store::open()),
            vault: Rc::new(Vault::new("xcord")),
        }
    }

    pub fn store(&self) -> Rc<Store> {
        self.store.clone()
    }

    fn secret_key(profile_id: &str, kind: &str) -> String {
        format!("session:{profile_id}:{kind}")
    }

    fn keyring_key(&self, profile: &SessionProfile, kind: &str) -> String {
        match (&profile.auth, kind) {
            (
                AuthKind::Password {
                    secret_id: Some(id),
                    ..
                },
                "password",
            ) => id.clone(),
            (
                AuthKind::PublicKey {
                    secret_id: Some(id),
                    ..
                },
                "passphrase",
            ) => id.clone(),
            _ => Self::secret_key(&profile.id, kind),
        }
    }
}

impl Default for SessionManager {
    fn default() -> Self {
        Self::new()
    }
}

impl SessionService for SessionManager {
    fn list(&self) -> Vec<SessionProfile> {
        self.store.sessions()
    }

    fn groups(&self) -> Vec<String> {
        self.store.config().groups
    }

    fn get(&self, id: &str) -> Option<SessionProfile> {
        self.store.session(id)
    }

    fn save(&self, profile: SessionProfile) -> std::result::Result<(), String> {
        self.store
            .upsert_session(profile)
            .map_err(|error| error.to_string())
    }

    fn delete(&self, id: &str) -> std::result::Result<(), String> {
        self.store
            .delete_session(id)
            .map_err(|error| error.to_string())
    }

    fn snippets(&self) -> Vec<Snippet> {
        self.store.snippets()
    }

    fn save_snippet(&self, snippet: Snippet) -> std::result::Result<(), String> {
        self.store
            .upsert_snippet(snippet)
            .map_err(|error| error.to_string())
    }

    fn delete_snippet(&self, id: &str) -> std::result::Result<(), String> {
        self.store
            .delete_snippet(id)
            .map_err(|error| error.to_string())
    }

    fn highlights(&self) -> Vec<HighlightRule> {
        self.store.highlights()
    }

    fn save_highlight(&self, rule: HighlightRule) -> std::result::Result<(), String> {
        self.store
            .upsert_highlight(rule)
            .map_err(|error| error.to_string())
    }

    fn delete_highlight(&self, id: &str) -> std::result::Result<(), String> {
        self.store
            .delete_highlight(id)
            .map_err(|error| error.to_string())
    }

    fn settings(&self) -> Settings {
        self.store.settings()
    }

    fn save_settings(&self, settings: Settings) -> std::result::Result<(), String> {
        self.store
            .update(|config| config.settings = settings)
            .map_err(|error| error.to_string())
    }

    fn password(&self, profile: &SessionProfile) -> Option<String> {
        let key = self.keyring_key(profile, "password");
        self.vault.get(&key).ok().flatten()
    }

    fn store_password(
        &self,
        profile: &SessionProfile,
        password: &str,
    ) -> std::result::Result<(), String> {
        let key = Self::secret_key(&profile.id, "password");
        self.vault
            .set(&key, password)
            .map_err(|error| error.to_string())?;
        let mut updated = profile.clone();
        if let AuthKind::Password { save, secret_id } = &mut updated.auth {
            *save = true;
            *secret_id = Some(key);
        }
        self.store
            .upsert_session(updated)
            .map_err(|error| error.to_string())
    }

    fn clear_password(&self, profile: &SessionProfile) -> std::result::Result<(), String> {
        let key = self.keyring_key(profile, "password");
        self.vault.delete(&key).map_err(|error| error.to_string())?;
        let mut updated = profile.clone();
        if let AuthKind::Password { save, secret_id } = &mut updated.auth {
            *save = false;
            *secret_id = None;
        }
        self.store
            .upsert_session(updated)
            .map_err(|error| error.to_string())
    }

    fn passphrase(&self, profile: &SessionProfile) -> Option<String> {
        let key = self.keyring_key(profile, "passphrase");
        self.vault.get(&key).ok().flatten()
    }

    fn store_passphrase(
        &self,
        profile: &SessionProfile,
        passphrase: &str,
    ) -> std::result::Result<(), String> {
        let key = Self::secret_key(&profile.id, "passphrase");
        self.vault
            .set(&key, passphrase)
            .map_err(|error| error.to_string())?;
        let mut updated = profile.clone();
        if let AuthKind::PublicKey {
            save_passphrase,
            secret_id,
            ..
        } = &mut updated.auth
        {
            *save_passphrase = true;
            *secret_id = Some(key);
        }
        self.store
            .upsert_session(updated)
            .map_err(|error| error.to_string())
    }

    fn clear_passphrase(&self, profile: &SessionProfile) -> std::result::Result<(), String> {
        let key = self.keyring_key(profile, "passphrase");
        self.vault.delete(&key).map_err(|error| error.to_string())?;
        let mut updated = profile.clone();
        if let AuthKind::PublicKey {
            save_passphrase,
            secret_id,
            ..
        } = &mut updated.auth
        {
            *save_passphrase = false;
            *secret_id = None;
        }
        self.store
            .upsert_session(updated)
            .map_err(|error| error.to_string())
    }

    fn export(&self, path: &str) -> std::result::Result<(), String> {
        self.store
            .export_to(&PathBuf::from(path))
            .map_err(|error| error.to_string())
    }

    fn import(&self, path: &str) -> std::result::Result<(), String> {
        self.store
            .import_from(&PathBuf::from(path))
            .map_err(|error| error.to_string())
    }
}

/// Provides the `sessions` service.
#[derive(Clone)]
pub struct SessionsPlugin;

impl Plugin for SessionsPlugin {
    fn meta(&self) -> PluginMeta {
        PluginMeta::new("sessions", "Session Store")
            .description("TOML workspace, profiles, snippets and keychain secrets")
            .author("xcord")
            .capabilities(&["core", "sessions", "secrets"])
    }

    fn apply(&self, ctx: Context, _config: serde_json::Value) -> Result<()> {
        let manager = Rc::new(SessionManager::new());
        for warning in manager.store().warnings() {
            log::warn!("{warning}");
        }
        let service: Rc<dyn SessionService> = manager.clone();
        ctx.provide(services::SESSIONS, Sessions(service))?;

        let store = manager.store();
        ctx.effect("flush workspace config", move || {
            if let Err(error) = store.save() {
                log::error!("could not save workspace on unload: {error}");
            }
        });
        Ok(())
    }
}
