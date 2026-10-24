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

    fn create_group(&self, name: &str) -> std::result::Result<(), String> {
        let name = name.trim();
        if name.is_empty() {
            return Err("Group name cannot be empty.".to_string());
        }
        if self.store.config().groups.iter().any(|group| group == name) {
            return Ok(());
        }
        let name = name.to_string();
        self.store
            .update(|config| config.groups.push(name))
            .map_err(|error| error.to_string())
    }

    fn rename_group(&self, old: &str, new: &str) -> std::result::Result<(), String> {
        let new = new.trim();
        if new.is_empty() {
            return Err("Group name cannot be empty.".to_string());
        }
        if old == new {
            return Ok(());
        }
        if self.store.config().groups.iter().any(|group| group == new) {
            return Err("A group with that name already exists.".to_string());
        }
        let old = old.to_string();
        let new = new.to_string();
        self.store
            .update(|config| {
                for group in &mut config.groups {
                    if group == &old {
                        *group = new.clone();
                    }
                }
                for session in &mut config.sessions {
                    if session.group == old {
                        session.group = new.clone();
                    }
                }
            })
            .map_err(|error| error.to_string())
    }

    fn move_group(
        &self,
        name: &str,
        target: Option<&str>,
        after: bool,
    ) -> std::result::Result<(), String> {
        let mut groups = self.store.config().groups;
        let Some(index) = groups.iter().position(|group| group == name) else {
            return Err("Group no longer exists.".to_string());
        };
        let name = groups.remove(index);
        let insert_at = target
            .and_then(|target| groups.iter().position(|group| group == target))
            .map(|index| if after { index + 1 } else { index })
            .unwrap_or(groups.len())
            .min(groups.len());
        groups.insert(insert_at, name);
        self.store
            .update(|config| config.groups = groups)
            .map_err(|error| error.to_string())
    }

    fn move_session(
        &self,
        id: &str,
        group: &str,
        target: Option<&str>,
        after: bool,
    ) -> std::result::Result<(), String> {
        if group.trim().is_empty() {
            return Err("Group name cannot be empty.".to_string());
        }
        let mut sessions = self.store.sessions();
        let Some(index) = sessions.iter().position(|session| session.id == id) else {
            return Err("Session no longer exists.".to_string());
        };
        let mut session = sessions.remove(index);
        session.group = group.to_string();
        let insert_at = target
            .and_then(|target| sessions.iter().position(|session| session.id == target))
            .map(|index| if after { index + 1 } else { index })
            .unwrap_or(sessions.len())
            .min(sessions.len());
        sessions.insert(insert_at, session);
        let group = group.to_string();
        self.store
            .update(|config| {
                config.sessions = sessions;
                if !config.groups.iter().any(|existing| existing == &group) {
                    config.groups.push(group);
                }
            })
            .map_err(|error| error.to_string())
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

    fn ai_api_key(&self) -> Option<String> {
        self.vault.get("ai:api_key").ok().flatten()
    }

    fn store_ai_api_key(&self, key: &str) -> std::result::Result<(), String> {
        self.vault
            .set("ai:api_key", key)
            .map_err(|error| error.to_string())
    }

    fn clear_ai_api_key(&self) -> std::result::Result<(), String> {
        self.vault
            .delete("ai:api_key")
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
