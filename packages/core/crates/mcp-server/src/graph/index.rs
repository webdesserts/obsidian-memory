use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};

use tokio::fs;
use wiki_links::extract_linked_notes;

/// Tracks forward links and backlinks between notes in the vault.
///
/// The graph index maintains a bidirectional view of wiki-link connections:
/// - Forward links: which notes does a given note link TO
/// - Backlinks: which notes link TO a given note
#[derive(Debug, Default)]
pub struct GraphIndex {
    /// Map from note name (without extension) to its forward links
    forward_links: HashMap<String, HashSet<String>>,
    /// Map from note name to notes that link TO it
    backlinks: HashMap<String, HashSet<String>>,
    /// Map from note name to its file path (relative to vault root)
    paths: HashMap<String, PathBuf>,
}

impl GraphIndex {
    pub fn new() -> Self {
        Self::default()
    }

    /// Initialize the graph index by scanning the vault.
    ///
    /// Recursively scans all markdown files in the vault, extracts wiki-links,
    /// and builds the forward links and backlinks graph.
    pub async fn initialize(&mut self, vault_path: &Path) -> Result<(), std::io::Error> {
        tracing::info!("Scanning vault for notes...");

        let files = Self::get_all_markdown_files(vault_path).await?;
        tracing::info!("Found {} markdown files", files.len());

        for file_path in files {
            if let Err(e) = self.index_file(vault_path, &file_path).await {
                tracing::warn!("Failed to index {}: {}", file_path.display(), e);
            }
        }

        tracing::info!(
            "Indexed {} notes with {} total links",
            self.paths.len(),
            self.get_total_links()
        );

        Ok(())
    }

    /// Recursively get all markdown files in a directory.
    async fn get_all_markdown_files(dir: &Path) -> Result<Vec<PathBuf>, std::io::Error> {
        let mut files = Vec::new();
        let mut entries = fs::read_dir(dir).await?;

        while let Some(entry) = entries.next_entry().await? {
            let path = entry.path();
            let file_name = entry.file_name();
            let file_name_str = file_name.to_string_lossy();

            // Skip hidden directories (.obsidian, .git, .trash, etc.)
            if file_name_str.starts_with('.') {
                continue;
            }

            let file_type = entry.file_type().await?;
            if file_type.is_dir() {
                // Recursively scan subdirectories
                let sub_files = Box::pin(Self::get_all_markdown_files(&path)).await?;
                files.extend(sub_files);
            } else if file_type.is_file() && file_name_str.ends_with(".md") {
                files.push(path);
            }
        }

        Ok(files)
    }

    /// Index a single file (extract links and update graph).
    async fn index_file(&mut self, vault_path: &Path, file_path: &Path) -> Result<(), std::io::Error> {
        let content = fs::read_to_string(file_path).await?;

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

        // Extract linked notes using wiki-links crate
        let linked_notes = extract_linked_notes(&content);
        let links: HashSet<String> = linked_notes.into_iter().collect();

        // Update the graph
        self.update_note(&note_name, relative_path, links);

        Ok(())
    }

    /// Get total number of links in the graph.
    fn get_total_links(&self) -> usize {
        self.forward_links.values().map(|links| links.len()).sum()
    }

    /// Add or update a note's links in the index.
    ///
    /// This removes any existing links for the note and replaces them with the new set.
    pub fn update_note(&mut self, note_name: &str, path: PathBuf, links: HashSet<String>) {
        // Remove old backlinks for this note
        if let Some(old_links) = self.forward_links.get(note_name) {
            for target in old_links.iter() {
                if let Some(backlink_set) = self.backlinks.get_mut(target) {
                    backlink_set.remove(note_name);
                }
            }
        }

        // Add new backlinks
        for target in &links {
            self.backlinks
                .entry(target.clone())
                .or_default()
                .insert(note_name.to_string());
        }

        // Update forward links and path
        self.forward_links.insert(note_name.to_string(), links);
        self.paths.insert(note_name.to_string(), path);
    }

    /// Remove a note from the index entirely.
    pub fn remove_note(&mut self, note_name: &str) {
        // Remove backlinks pointing to other notes
        if let Some(links) = self.forward_links.remove(note_name) {
            for target in links {
                if let Some(backlink_set) = self.backlinks.get_mut(&target) {
                    backlink_set.remove(note_name);
                }
            }
        }

        // Remove backlinks pointing to this note
        self.backlinks.remove(note_name);

        // Remove from all other notes' backlinks
        for backlink_set in self.backlinks.values_mut() {
            backlink_set.remove(note_name);
        }

        self.paths.remove(note_name);
    }

    /// Get forward links for a note (notes this note links TO).
    pub fn get_forward_links(&self, note_name: &str) -> Option<&HashSet<String>> {
        self.forward_links.get(note_name)
    }

    /// Get backlinks for a note (notes that link TO this note).
    pub fn get_backlinks(&self, note_name: &str) -> Option<&HashSet<String>> {
        self.backlinks.get(note_name)
    }

    /// Get the file path for a note.
    pub fn get_path(&self, note_name: &str) -> Option<&PathBuf> {
        self.paths.get(note_name)
    }

    /// Get all note names in the index.
    pub fn note_names(&self) -> impl Iterator<Item = &String> {
        self.paths.keys()
    }

    /// Get the total number of notes in the index.
    pub fn len(&self) -> usize {
        self.paths.len()
    }

    /// Check if the index is empty.
    pub fn is_empty(&self) -> bool {
        self.paths.is_empty()
    }

    /// Get all links (both forward and back) for a note - its "neighborhood".
    pub fn get_neighborhood(&self, note_name: &str) -> HashSet<String> {
        let mut neighborhood = HashSet::new();

        if let Some(forward) = self.get_forward_links(note_name) {
            neighborhood.extend(forward.iter().cloned());
        }

        if let Some(back) = self.get_backlinks(note_name) {
            neighborhood.extend(back.iter().cloned());
        }

        neighborhood
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_new_index_is_empty() {
        let index = GraphIndex::new();
        assert!(index.is_empty());
        assert_eq!(index.len(), 0);
    }

    #[test]
    fn test_add_note_with_links() {
        let mut index = GraphIndex::new();

        let links: HashSet<String> = ["Note B", "Note C"].iter().map(|s| s.to_string()).collect();
        index.update_note("Note A", PathBuf::from("Note A.md"), links);

        assert_eq!(index.len(), 1);
        assert!(!index.is_empty());

        let forward = index.get_forward_links("Note A").unwrap();
        assert!(forward.contains("Note B"));
        assert!(forward.contains("Note C"));
        assert_eq!(forward.len(), 2);
    }

    #[test]
    fn test_backlinks_are_created() {
        let mut index = GraphIndex::new();

        let links: HashSet<String> = ["Note B"].iter().map(|s| s.to_string()).collect();
        index.update_note("Note A", PathBuf::from("Note A.md"), links);

        // Note B should have a backlink from Note A
        let backlinks = index.get_backlinks("Note B").unwrap();
        assert!(backlinks.contains("Note A"));
    }

    #[test]
    fn test_update_note_removes_old_backlinks() {
        let mut index = GraphIndex::new();

        // First, Note A links to Note B
        let links1: HashSet<String> = ["Note B"].iter().map(|s| s.to_string()).collect();
        index.update_note("Note A", PathBuf::from("Note A.md"), links1);

        // Verify backlink exists
        assert!(index.get_backlinks("Note B").unwrap().contains("Note A"));

        // Now update Note A to link to Note C instead
        let links2: HashSet<String> = ["Note C"].iter().map(|s| s.to_string()).collect();
        index.update_note("Note A", PathBuf::from("Note A.md"), links2);

        // Note B should no longer have backlink from Note A
        let backlinks_b = index.get_backlinks("Note B");
        assert!(backlinks_b.is_none() || !backlinks_b.unwrap().contains("Note A"));

        // Note C should have backlink from Note A
        assert!(index.get_backlinks("Note C").unwrap().contains("Note A"));
    }

    #[test]
    fn test_remove_note() {
        let mut index = GraphIndex::new();

        let links: HashSet<String> = ["Note B"].iter().map(|s| s.to_string()).collect();
        index.update_note("Note A", PathBuf::from("Note A.md"), links);

        index.remove_note("Note A");

        assert!(index.is_empty());
        assert!(index.get_forward_links("Note A").is_none());

        // Backlink from Note B should also be gone
        let backlinks = index.get_backlinks("Note B");
        assert!(backlinks.is_none() || backlinks.unwrap().is_empty());
    }

    #[test]
    fn test_get_neighborhood() {
        let mut index = GraphIndex::new();

        // Note A links to Note B and Note C
        let links_a: HashSet<String> = ["Note B", "Note C"].iter().map(|s| s.to_string()).collect();
        index.update_note("Note A", PathBuf::from("Note A.md"), links_a);

        // Note D links to Note A
        let links_d: HashSet<String> = ["Note A"].iter().map(|s| s.to_string()).collect();
        index.update_note("Note D", PathBuf::from("Note D.md"), links_d);

        let neighborhood = index.get_neighborhood("Note A");

        // Should include forward links (B, C) and backlinks (D)
        assert!(neighborhood.contains("Note B"));
        assert!(neighborhood.contains("Note C"));
        assert!(neighborhood.contains("Note D"));
        assert_eq!(neighborhood.len(), 3);
    }

    #[test]
    fn test_get_path() {
        let mut index = GraphIndex::new();

        index.update_note(
            "Note A",
            PathBuf::from("folder/Note A.md"),
            HashSet::new(),
        );

        let path = index.get_path("Note A").unwrap();
        assert_eq!(path, &PathBuf::from("folder/Note A.md"));
    }

    #[test]
    fn test_note_names_iterator() {
        let mut index = GraphIndex::new();

        index.update_note("Note A", PathBuf::from("Note A.md"), HashSet::new());
        index.update_note("Note B", PathBuf::from("Note B.md"), HashSet::new());

        let names: HashSet<_> = index.note_names().cloned().collect();
        assert!(names.contains("Note A"));
        assert!(names.contains("Note B"));
        assert_eq!(names.len(), 2);
    }
}
