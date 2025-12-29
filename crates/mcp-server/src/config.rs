use std::path::PathBuf;

/// Server configuration loaded from environment variables.
#[derive(Debug, Clone)]
pub struct Config {
    /// Path to the Obsidian vault root directory
    pub vault_path: PathBuf,
    /// Name of the vault (derived from vault_path)
    pub vault_name: String,
}

impl Config {
    /// Load configuration from environment variables.
    ///
    /// Required environment variables:
    /// - `OBSIDIAN_VAULT_PATH`: Path to the Obsidian vault root
    pub fn from_env() -> Result<Self, ConfigError> {
        let vault_path: PathBuf = std::env::var("OBSIDIAN_VAULT_PATH")
            .map_err(|_| ConfigError::MissingVaultPath)?
            .into();

        // Derive vault name from path
        let vault_name = vault_path
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or("vault")
            .to_string();

        Ok(Self {
            vault_path,
            vault_name,
        })
    }
}

#[derive(Debug, thiserror::Error)]
pub enum ConfigError {
    #[error("OBSIDIAN_VAULT_PATH environment variable not set")]
    MissingVaultPath,
}
