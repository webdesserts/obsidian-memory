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
    /// - `OBSIDIAN_VAULT_PATH`: Path to the Obsidian vault root (supports ~ for home directory)
    pub fn from_env() -> Result<Self, ConfigError> {
        let vault_path_str = std::env::var("OBSIDIAN_VAULT_PATH")
            .map_err(|_| ConfigError::MissingVaultPath)?;
        
        // Expand tilde to home directory
        let vault_path = expand_tilde(&vault_path_str);

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

/// Expand ~ or ~/ prefix to the user's home directory.
fn expand_tilde(path: &str) -> PathBuf {
    if path == "~" {
        dirs::home_dir().unwrap_or_else(|| PathBuf::from("~"))
    } else if let Some(rest) = path.strip_prefix("~/") {
        dirs::home_dir()
            .map(|home| home.join(rest))
            .unwrap_or_else(|| PathBuf::from(path))
    } else {
        PathBuf::from(path)
    }
}

#[derive(Debug, thiserror::Error)]
pub enum ConfigError {
    #[error("OBSIDIAN_VAULT_PATH environment variable not set")]
    MissingVaultPath,
}
