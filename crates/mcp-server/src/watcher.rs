//! File watcher for keeping the graph index up to date.
//!
//! Watches the vault directory for changes to markdown files and updates
//! the graph index accordingly. Uses debouncing to batch rapid changes.

use notify::RecommendedWatcher;
use notify_debouncer_mini::{new_debouncer, DebouncedEventKind, Debouncer};
use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::{mpsc, RwLock};
use wiki_links::extract_linked_notes;

use crate::graph::GraphIndex;

/// Watches vault directory and updates graph index on file changes.
pub struct VaultWatcher {
    _debouncer: Debouncer<RecommendedWatcher>,
}

impl VaultWatcher {
    /// Start watching the vault directory.
    ///
    /// Spawns a background task that listens for file system events and
    /// updates the graph index when markdown files change.
    pub fn start(
        vault_path: PathBuf,
        graph: Arc<RwLock<GraphIndex>>,
    ) -> Result<Self, notify::Error> {
        let (tx, rx) = mpsc::channel(100);
        let vault_path_clone = vault_path.clone();

        // Create debouncer that sends events to our channel
        let mut debouncer = new_debouncer(
            Duration::from_millis(500),
            move |result: Result<Vec<notify_debouncer_mini::DebouncedEvent>, notify::Error>| {
                if let Ok(events) = result {
                    // Filter for markdown files only
                    let md_events: Vec<_> = events
                        .into_iter()
                        .filter(|e| {
                            e.path
                                .extension()
                                .map(|ext| ext == "md")
                                .unwrap_or(false)
                        })
                        .collect();

                    if !md_events.is_empty() {
                        let _ = tx.blocking_send(md_events);
                    }
                }
            },
        )?;

        // Start watching the vault
        debouncer
            .watcher()
            .watch(&vault_path, notify::RecursiveMode::Recursive)?;

        tracing::info!("Started file watcher for {}", vault_path.display());

        // Spawn background task to process events
        tokio::spawn(process_events(rx, vault_path_clone, graph));

        Ok(Self {
            _debouncer: debouncer,
        })
    }
}

/// Process file system events and update the graph index.
async fn process_events(
    mut rx: mpsc::Receiver<Vec<notify_debouncer_mini::DebouncedEvent>>,
    vault_path: PathBuf,
    graph: Arc<RwLock<GraphIndex>>,
) {
    while let Some(events) = rx.recv().await {
        for event in events {
            let path = &event.path;

            // Skip hidden files and directories
            if path
                .components()
                .any(|c| c.as_os_str().to_string_lossy().starts_with('.'))
            {
                continue;
            }

            match event.kind {
                DebouncedEventKind::Any => {
                    // File created or modified - re-index it
                    if path.exists() {
                        if let Err(e) = update_file(&vault_path, path, &graph).await {
                            tracing::warn!("Failed to update index for {}: {}", path.display(), e);
                        }
                    } else {
                        // File was deleted
                        remove_file(path, &graph).await;
                    }
                }
                DebouncedEventKind::AnyContinuous => {
                    // Continuous events (like ongoing writes) - skip until settled
                }
                _ => {
                    // Handle any future event kinds
                }
            }
        }
    }
}

/// Update the graph index for a single file.
async fn update_file(
    vault_path: &Path,
    file_path: &Path,
    graph: &Arc<RwLock<GraphIndex>>,
) -> Result<(), std::io::Error> {
    let content = tokio::fs::read_to_string(file_path).await?;

    // Get note name (filename without .md extension)
    let note_name = file_path
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or_default()
        .to_string();

    // Get relative path from vault root
    let relative_path = file_path
        .strip_prefix(vault_path)
        .unwrap_or(file_path)
        .to_path_buf();

    // Extract linked notes
    let linked_notes = extract_linked_notes(&content);
    let links: HashSet<String> = linked_notes.into_iter().collect();

    // Update the graph
    let mut graph = graph.write().await;
    graph.update_note(&note_name, relative_path.clone(), links);

    tracing::debug!("Updated index for: {}", relative_path.display());

    Ok(())
}

/// Remove a file from the graph index.
async fn remove_file(file_path: &Path, graph: &Arc<RwLock<GraphIndex>>) {
    let note_name = file_path
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or_default()
        .to_string();

    if !note_name.is_empty() {
        let mut graph = graph.write().await;
        graph.remove_note(&note_name);
        tracing::debug!("Removed from index: {}", note_name);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use tempfile::TempDir;

    #[tokio::test]
    async fn test_watcher_starts_successfully() {
        let temp_dir = TempDir::new().unwrap();
        let graph = Arc::new(RwLock::new(GraphIndex::new()));

        let watcher = VaultWatcher::start(temp_dir.path().to_path_buf(), graph);
        assert!(watcher.is_ok());
    }

    #[tokio::test]
    async fn test_update_file_indexes_links() {
        let temp_dir = TempDir::new().unwrap();
        let file_path = temp_dir.path().join("test.md");
        fs::write(&file_path, "Links to [[Note A]] and [[Note B]]").unwrap();

        let graph = Arc::new(RwLock::new(GraphIndex::new()));

        update_file(temp_dir.path(), &file_path, &graph)
            .await
            .unwrap();

        let graph = graph.read().await;
        let links = graph.get_forward_links("test").unwrap();
        assert!(links.contains("Note A"));
        assert!(links.contains("Note B"));
    }

    #[tokio::test]
    async fn test_remove_file_clears_index() {
        let temp_dir = TempDir::new().unwrap();
        let file_path = temp_dir.path().join("test.md");

        let graph = Arc::new(RwLock::new(GraphIndex::new()));

        // First add the note
        {
            let mut g = graph.write().await;
            g.update_note(
                "test",
                PathBuf::from("test.md"),
                ["Note A"].iter().map(|s| s.to_string()).collect(),
            );
        }

        // Verify it exists
        {
            let g = graph.read().await;
            assert!(g.get_forward_links("test").is_some());
        }

        // Remove it
        remove_file(&file_path, &graph).await;

        // Verify it's gone
        let g = graph.read().await;
        assert!(g.get_forward_links("test").is_none());
    }
}
