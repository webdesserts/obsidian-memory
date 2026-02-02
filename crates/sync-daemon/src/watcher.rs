//! File watcher with debouncing for vault changes.
//!
//! Uses notify-debouncer-mini for efficient file change detection.

use anyhow::Result;
use notify::RecursiveMode;
use notify_debouncer_mini::{new_debouncer, DebouncedEvent, DebouncedEventKind};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime};
use tokio::sync::mpsc;
use tracing::{debug, error};

/// File event from the watcher.
#[derive(Debug, Clone)]
pub struct FileEvent {
    /// Path relative to vault root
    pub path: String,
    /// Type of event
    pub kind: FileEventKind,
}

/// Type of file event.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FileEventKind {
    /// File was created or modified
    Modified,
    /// File was deleted
    Deleted,
}

/// File watcher that monitors the vault directory.
pub struct FileWatcher {
    /// Vault base path
    vault_path: PathBuf,
    /// Debouncer handle (must keep alive)
    _debouncer: notify_debouncer_mini::Debouncer<notify::RecommendedWatcher>,
    /// Receiver for file events
    event_rx: mpsc::UnboundedReceiver<FileEvent>,
}

/// Track last seen mtime to filter spurious events (Docker volume bug workaround)
type MtimeCache = Arc<Mutex<HashMap<PathBuf, SystemTime>>>;

impl FileWatcher {
    /// Create a new file watcher for the vault.
    ///
    /// Uses 200ms debounce period to avoid rapid-fire events during saves.
    pub fn new(vault_path: PathBuf) -> Result<Self> {
        // Canonicalize the path to resolve symlinks. On macOS, /var/folders/...
        // is actually /private/var/folders/..., and FSEvents needs the real path.
        let vault_path = vault_path.canonicalize().unwrap_or(vault_path);

        // Create tokio channel for async event delivery
        let (event_tx, event_rx) = mpsc::unbounded_channel();
        let vault_path_clone = vault_path.clone();

        // Mtime cache to filter spurious events (Docker volume workaround)
        let mtime_cache: MtimeCache = Arc::new(Mutex::new(HashMap::new()));
        let mtime_cache_clone = Arc::clone(&mtime_cache);

        // Create debouncer with callback (notify-debouncer-mini 0.6 API)
        let mut debouncer = new_debouncer(
            Duration::from_millis(200),
            move |result: Result<Vec<DebouncedEvent>, notify::Error>| {
                match result {
                    Ok(events) => {
                        for event in events {
                            if let Some(file_event) =
                                Self::process_event(&event, &vault_path_clone, &mtime_cache_clone)
                            {
                                if event_tx.send(file_event).is_err() {
                                    // Receiver dropped
                                    return;
                                }
                            }
                        }
                    }
                    Err(e) => {
                        error!("File watcher error: {}", e);
                    }
                }
            },
        )?;

        // Watch vault directory recursively
        debouncer
            .watcher()
            .watch(&vault_path, RecursiveMode::Recursive)?;

        Ok(Self {
            vault_path,
            _debouncer: debouncer,
            event_rx,
        })
    }

    /// Process a single debounced event, returning a FileEvent if relevant.
    fn process_event(
        event: &DebouncedEvent,
        vault_path: &Path,
        mtime_cache: &MtimeCache,
    ) -> Option<FileEvent> {
        let path = &event.path;

        // Get path relative to vault
        let relative = path.strip_prefix(vault_path).ok()?;
        let relative_str = relative.to_str()?;

        // Skip .sync directory
        if relative_str.starts_with(".sync") || relative_str.contains("/.sync/") {
            return None;
        }

        // Skip hidden files and directories
        if relative_str.starts_with('.') || relative_str.contains("/.") {
            return None;
        }

        // Only process .md files
        if !relative_str.ends_with(".md") {
            return None;
        }

        let kind = match event.kind {
            DebouncedEventKind::Any | DebouncedEventKind::AnyContinuous => {
                // Check if file exists to determine if modified or deleted
                if path.exists() {
                    FileEventKind::Modified
                } else {
                    FileEventKind::Deleted
                }
            }
            // Handle any future event kinds (non-exhaustive enum)
            _ => {
                if path.exists() {
                    FileEventKind::Modified
                } else {
                    FileEventKind::Deleted
                }
            }
        };

        // For modifications, check mtime to filter spurious events (Docker volume workaround)
        // Uses relative path as key so cache is bounded by vault size
        let relative_path = relative.to_path_buf();
        if kind == FileEventKind::Modified {
            if let Ok(metadata) = std::fs::metadata(path) {
                if let Ok(mtime) = metadata.modified() {
                    let mut cache = mtime_cache
                        .lock()
                        .expect("mtime cache mutex poisoned");
                    if let Some(last_mtime) = cache.get(&relative_path) {
                        if *last_mtime == mtime {
                            // Mtime unchanged - spurious event, skip it
                            return None;
                        }
                    }
                    cache.insert(relative_path, mtime);
                }
            }
        } else if kind == FileEventKind::Deleted {
            // Remove from cache when file is deleted
            let mut cache = mtime_cache
                .lock()
                .expect("mtime cache mutex poisoned");
            cache.remove(&relative_path);
        }

        debug!("File event: {:?} - {}", kind, relative_str);

        Some(FileEvent {
            path: relative_str.to_string(),
            kind,
        })
    }

    /// Get the receiver for file events.
    pub fn event_rx(&mut self) -> &mut mpsc::UnboundedReceiver<FileEvent> {
        &mut self.event_rx
    }

    /// Get the vault path.
    pub fn vault_path(&self) -> &Path {
        &self.vault_path
    }
}
