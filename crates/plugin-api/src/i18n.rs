//! Localization contracts.
//!
//! The `i18n` plugin owns the current locale and per-domain catalogs. Plugins
//! register translations during `apply` and dispose the returned handle with an
//! effect, so unloading a plugin also removes its strings. The shell resolves
//! labels at render time, which makes switching locales immediate.

use std::collections::BTreeMap;
use std::ops::Deref;
use std::rc::Rc;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LocaleInfo {
    pub id: String,
    pub name: String,
}

pub trait LocalizationService {
    fn current_locale(&self) -> String;
    fn set_locale(&self, locale: &str) -> Result<(), String>;
    fn available_locales(&self) -> Vec<LocaleInfo>;

    /// Register a catalog slice and return a handle for [`Self::unregister`].
    fn register(&self, domain: &str, locale: &str, entries: BTreeMap<String, String>) -> u64;
    fn unregister(&self, handle: u64);

    /// Look up `key` in the current locale, then the fallback locales.
    fn translate(&self, domain: &str, key: &str) -> Option<String>;

    fn register_pairs(&self, domain: &str, locale: &str, entries: &[(&str, &str)]) -> u64 {
        self.register(
            domain,
            locale,
            entries
                .iter()
                .map(|(key, value)| (key.to_string(), value.to_string()))
                .collect(),
        )
    }
}

/// Service wrapper cloned by the shell and plugins.
#[derive(Clone)]
pub struct I18n(pub Rc<dyn LocalizationService>);

impl Deref for I18n {
    type Target = dyn LocalizationService;
    fn deref(&self) -> &Self::Target {
        &*self.0
    }
}

impl I18n {
    pub fn locale(&self) -> String {
        self.0.current_locale()
    }

    pub fn set_locale(&self, locale: &str) -> Result<(), String> {
        self.0.set_locale(locale)
    }

    /// Translate a key, falling back to the key itself.
    pub fn t(&self, domain: &str, key: &str) -> String {
        self.0
            .translate(domain, key)
            .unwrap_or_else(|| key.to_string())
    }

    /// Translate a key, falling back to an English source string.
    pub fn t_or(&self, domain: &str, key: &str, fallback: &str) -> String {
        self.0
            .translate(domain, key)
            .unwrap_or_else(|| fallback.to_string())
    }

    /// Translate and substitute named `{placeholders}`.
    pub fn t_args(&self, domain: &str, key: &str, args: &[(&str, String)]) -> String {
        let mut text = self.t(domain, key);
        for (name, value) in args {
            text = text.replace(&format!("{{{name}}}"), value);
        }
        text
    }

    pub fn t_or_args(
        &self,
        domain: &str,
        key: &str,
        fallback: &str,
        args: &[(&str, String)],
    ) -> String {
        let mut text = self.t_or(domain, key, fallback);
        for (name, value) in args {
            text = text.replace(&format!("{{{name}}}"), value);
        }
        text
    }

    pub fn register_pairs(&self, domain: &str, locale: &str, entries: &[(&str, &str)]) -> u64 {
        self.0.register_pairs(domain, locale, entries)
    }

    pub fn unregister(&self, handle: u64) {
        self.0.unregister(handle);
    }
}
