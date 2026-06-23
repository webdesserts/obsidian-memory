//! Persistent app-level settings via `tauri-plugin-store`.
//!
//! App settings (vault path, autostart state) persist across runs in
//! `~/Library/Application Support/com.webdesserts.obsidian-memory/app-settings.json`.
//! This is distinct from per-vault config (`<vault>/.sync/daemon.toml`), which
//! covers vault-specific daemon options.

use std::path::{Path, PathBuf};

use anyhow::{Result, anyhow};
use serde_json::json;
use tauri::AppHandle;
use tauri_plugin_store::StoreExt;

/// Filename for the persistent app settings store (resolved into the Tauri app
/// data directory by `tauri-plugin-store`).
const STORE_PATH: &str = "app-settings.json";

/// JSON key for the stored vault path.
const KEY_VAULT_PATH: &str = "vault_path";

/// JSON key for the stored relay URL.
const KEY_RELAY_URL: &str = "relay_url";

/// JSON key for the stored autostart-enabled flag.
const KEY_AUTOSTART_ENABLED: &str = "autostart_enabled";

/// Persistent app-level settings.
///
/// Loaded from and saved to the `tauri-plugin-store` JSON file at startup and
/// whenever a value changes. All fields have safe defaults (no vault path, no
/// relay URL, no autostart).
pub struct AppSettings {
    vault_path: Option<PathBuf>,
    relay_url: Option<String>,
    autostart_enabled: bool,
}

impl AppSettings {
    /// Load settings from the on-disk store.
    ///
    /// If the store file does not exist yet (first run), returns default settings.
    /// The store plugin must be registered in the Tauri builder chain before this
    /// is called from `setup()`.
    pub fn load(app: &AppHandle) -> Result<Self> {
        let store = app
            .store(STORE_PATH)
            .map_err(|e| anyhow!("Failed to open app-settings store: {e}"))?;

        let vault_path = store
            .get(KEY_VAULT_PATH)
            .and_then(|v| v.as_str().map(PathBuf::from));

        // Treat a stored empty string as absent so a corrupted/blank write never
        // surfaces as `Some("")` to the relay-URL resolution path.
        let relay_url = store
            .get(KEY_RELAY_URL)
            .and_then(|v| v.as_str().map(str::to_owned))
            .filter(|s| !s.is_empty());

        let autostart_enabled = store
            .get(KEY_AUTOSTART_ENABLED)
            .and_then(|v| v.as_bool())
            .unwrap_or(false);

        Ok(AppSettings {
            vault_path,
            relay_url,
            autostart_enabled,
        })
    }

    /// Persist the current settings to disk.
    ///
    /// All calls are synchronous: `store.set()` and `store.save()` both operate
    /// synchronously (the plugin updates in-memory state and flushes to disk in the
    /// same call).
    pub fn save(&self, app: &AppHandle) -> Result<()> {
        let store = app
            .store(STORE_PATH)
            .map_err(|e| anyhow!("Failed to open app-settings store: {e}"))?;

        if let Some(path) = &self.vault_path {
            store.set(KEY_VAULT_PATH, json!(path.to_string_lossy().as_ref()));
        } else {
            store.delete(KEY_VAULT_PATH);
        }

        if let Some(url) = &self.relay_url {
            store.set(KEY_RELAY_URL, json!(url));
        } else {
            store.delete(KEY_RELAY_URL);
        }

        store.set(KEY_AUTOSTART_ENABLED, json!(self.autostart_enabled));

        store
            .save()
            .map_err(|e| anyhow!("Failed to save app-settings: {e}"))
    }

    pub fn vault_path(&self) -> Option<&Path> {
        self.vault_path.as_deref()
    }

    pub fn relay_url(&self) -> Option<&str> {
        self.relay_url.as_deref()
    }

    // Read by a future settings panel (design steer #8); startup autostart truth
    // comes from `autolaunch().is_enabled()`, so nothing reads this yet.
    #[allow(dead_code)]
    pub fn autostart_enabled(&self) -> bool {
        self.autostart_enabled
    }

    pub fn set_vault_path(&mut self, path: PathBuf) {
        self.vault_path = Some(path);
    }

    /// Set the relay URL, normalizing an empty string to `None`.
    ///
    /// Unlike `set_vault_path`, this collapses empty→`None` here so that clearing
    /// the field in the settings panel deletes the stored key rather than persisting
    /// a blank value. (The vault path relies on `resolve_vault_path` to filter empty
    /// strings at read time; the relay URL is read directly via `relay_url()`, so the
    /// guard lives at the write boundary instead.)
    pub fn set_relay_url(&mut self, url: Option<String>) {
        self.relay_url = url.filter(|s| !s.is_empty());
    }

    pub fn set_autostart_enabled(&mut self, enabled: bool) {
        self.autostart_enabled = enabled;
    }
}

/// Resolve the vault path from the environment variable or a stored fallback.
///
/// Priority (highest to lowest):
/// 1. `env_vault` — the value of `OBSIDIAN_MEMORY_VAULT` if set and non-empty.
///    Takes precedence so the env var always works as an override on dev machines.
/// 2. `stored_vault` — the path previously persisted in `app-settings.json`.
///    Used by autostarted instances that launch without the env var.
/// 3. `None` — neither source has a vault path; the caller should fail with a
///    useful error message.
///
/// Extracted as a pure function so it can be unit-tested without launching Tauri.
pub fn resolve_vault_path(env_vault: Option<&str>, stored_vault: Option<&str>) -> Option<PathBuf> {
    if let Some(v) = env_vault.filter(|v| !v.is_empty()) {
        return Some(PathBuf::from(v));
    }
    stored_vault.filter(|v| !v.is_empty()).map(PathBuf::from)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn env_vault_present_returns_env_path() {
        assert_eq!(
            resolve_vault_path(Some("/Users/michael/notes"), None),
            Some(PathBuf::from("/Users/michael/notes")),
        );
    }

    #[test]
    fn env_vault_absent_stored_present_returns_stored_path() {
        assert_eq!(
            resolve_vault_path(None, Some("/Users/michael/notes")),
            Some(PathBuf::from("/Users/michael/notes")),
        );
    }

    #[test]
    fn env_vault_takes_priority_over_stored() {
        assert_eq!(
            resolve_vault_path(
                Some("/Users/michael/notes-env"),
                Some("/Users/michael/notes-stored")
            ),
            Some(PathBuf::from("/Users/michael/notes-env")),
        );
    }

    #[test]
    fn neither_source_returns_none() {
        assert_eq!(resolve_vault_path(None, None), None);
    }

    #[test]
    fn empty_env_string_falls_through_to_stored() {
        // An empty env var (e.g. `OBSIDIAN_MEMORY_VAULT=`) is treated as unset,
        // mirroring how the relay-URL env var is empty-filtered before use.
        assert_eq!(
            resolve_vault_path(Some(""), Some("/Users/michael/notes")),
            Some(PathBuf::from("/Users/michael/notes")),
        );
    }

    #[test]
    fn empty_stored_path_treated_as_absent() {
        // A stored empty string (e.g. corrupted write) must not produce a blank path.
        assert_eq!(resolve_vault_path(None, Some("")), None);
    }

    /// Build an `AppSettings` with all fields at their defaults, for exercising the
    /// relay-URL setter/getter without a Tauri `AppHandle`. The store-backed
    /// `load`/`save` paths require a running Tauri runtime, so the empty→None guard
    /// (the only non-trivial behavior over a plain mirror of `vault_path`) is pinned
    /// here at the in-memory boundary instead.
    fn default_settings() -> AppSettings {
        AppSettings {
            vault_path: None,
            relay_url: None,
            autostart_enabled: false,
        }
    }

    #[test]
    fn relay_url_absent_by_default() {
        assert_eq!(default_settings().relay_url(), None);
    }

    #[test]
    fn relay_url_round_trips_through_setter() {
        let mut settings = default_settings();
        settings.set_relay_url(Some("https://umbra.computer/".to_string()));
        assert_eq!(settings.relay_url(), Some("https://umbra.computer/"));
    }

    #[test]
    fn set_relay_url_none_clears_value() {
        let mut settings = default_settings();
        settings.set_relay_url(Some("https://umbra.computer/".to_string()));
        settings.set_relay_url(None);
        assert_eq!(settings.relay_url(), None);
    }

    #[test]
    fn set_relay_url_empty_string_normalizes_to_none() {
        // Clearing the field in the settings panel sends an empty string; it must
        // collapse to None so `save` deletes the key rather than persisting a blank
        // URL that the relay-resolution path would then read back as `Some("")`.
        let mut settings = default_settings();
        settings.set_relay_url(Some(String::new()));
        assert_eq!(settings.relay_url(), None);
    }
}
