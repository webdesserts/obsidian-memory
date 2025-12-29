use std::path::PathBuf;

/// Server configuration loaded from environment variables.
#[derive(Debug, Clone)]
pub struct Config {
    /// Path to the Obsidian vault root directory
    pub vault_path: PathBuf,
}

impl Config {
    /// Load configuration from environment variables.
    ///
    /// Required environment variables:
    /// - `OBSIDIAN_VAULT_PATH`: Path to the Obsidian vault root
    pub fn from_env() -> Result<Self, ConfigError> {
        let vault_path = std::env::var("OBSIDIAN_VAULT_PATH")
            .map_err(|_| ConfigError::MissingVaultPath)?
            .into();

        Ok(Self { vault_path })
    }
}

#[derive(Debug, thiserror::Error)]
pub enum ConfigError {
    #[error("OBSIDIAN_VAULT_PATH environment variable not set")]
    MissingVaultPath,
}
