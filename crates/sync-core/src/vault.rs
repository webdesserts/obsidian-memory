//! Vault: Manages a collection of NoteDocuments and syncs with peers.

use crate::document::NoteDocument;
use crate::fs::{FileSystem, FsError};

use loro::LoroDoc;
use std::collections::HashMap;
use thiserror::Error;

/// Directory for sync state
const SYNC_DIR: &str = ".sync";
/// File registry document
const REGISTRY_FILE: &str = ".sync/registry.loro";

#[derive(Debug, Error)]
pub enum VaultError {
    #[error("Filesystem error: {0}")]
    Fs(#[from] FsError),

    #[error("Document error: {0}")]
    Document(#[from] crate::document::DocumentError),

    #[error("Vault not initialized")]
    NotInitialized,

    #[error("Vault error: {0}")]
    Other(String),
}

pub type Result<T> = std::result::Result<T, VaultError>;

/// A detected file move
#[derive(Debug, Clone)]
pub struct FileMove {
    /// Original path (from Loro metadata)
    pub from: String,
    /// New path (current filesystem location)
    pub to: String,
}

/// Report from reconciliation process
#[derive(Debug, Default)]
pub struct ReconcileReport {
    /// Files that were newly indexed (no .loro existed, not a move)
    pub indexed: Vec<String>,
    /// Files that were re-indexed (markdown differed from Loro)
    pub reindexed: Vec<String>,
    /// Files that were moved/renamed (detected via content matching)
    pub moved: Vec<FileMove>,
    /// Orphaned .loro hashes (file was deleted, not moved)
    pub orphaned: Vec<String>,
}

impl ReconcileReport {
    /// Check if any changes were made
    pub fn has_changes(&self) -> bool {
        !self.indexed.is_empty() || !self.reindexed.is_empty() || !self.moved.is_empty()
    }
    
    /// Total number of files processed
    pub fn total_processed(&self) -> usize {
        self.indexed.len() + self.reindexed.len() + self.moved.len()
    }
}

/// Manages a vault of documents
pub struct Vault<F: FileSystem> {
    /// File registry (tracks all files in vault)
    pub(crate) registry: LoroDoc,

    /// Loaded documents
    pub(crate) documents: HashMap<String, NoteDocument>,

    /// Filesystem abstraction
    pub(crate) fs: F,

    /// Our peer ID
    peer_id: String,
}

impl<F: FileSystem> Vault<F> {
    /// Initialize a new vault (creates .sync directory)
    pub async fn init(fs: F, peer_id: String) -> Result<Self> {
        // Create .sync directory
        fs.mkdir(SYNC_DIR).await?;
        fs.mkdir(&format!("{}/documents", SYNC_DIR)).await?;

        let registry = LoroDoc::new();

        // Save initial registry
        let registry_bytes = registry.export(loro::ExportMode::Snapshot).unwrap();
        fs.write(REGISTRY_FILE, &registry_bytes).await?;

        let mut vault = Self {
            registry,
            documents: HashMap::new(),
            fs,
            peer_id,
        };

        // Scan and index all existing markdown files
        vault.index_existing_files().await?;

        Ok(vault)
    }

    /// Load an existing vault and reconcile with filesystem.
    /// 
    /// Reconciliation ensures the Loro state matches the filesystem:
    /// - New files (no .loro) → index them
    /// - Modified files (markdown ≠ Loro) → re-index from markdown  
    /// - Orphaned .loro files → logged for future cleanup
    pub async fn load(fs: F, peer_id: String) -> Result<Self> {
        // Check if vault is initialized
        if !fs.exists(SYNC_DIR).await? {
            return Err(VaultError::NotInitialized);
        }

        // Load registry
        let registry = if fs.exists(REGISTRY_FILE).await? {
            let bytes = fs.read(REGISTRY_FILE).await?;
            let doc = LoroDoc::new();
            doc.import(&bytes).ok();
            doc
        } else {
            LoroDoc::new()
        };

        let mut vault = Self {
            registry,
            documents: HashMap::new(),
            fs,
            peer_id,
        };
        
        // Reconcile filesystem with Loro state
        vault.reconcile().await?;

        Ok(vault)
    }
    
    /// Reconcile filesystem state with Loro documents.
    /// 
    /// This is called on load to handle changes made while the plugin was off:
    /// - External file additions → create new Loro docs
    /// - External file modifications → re-create Loro docs from markdown
    /// - External file moves → migrate Loro doc to new path hash
    /// - External file deletions → orphaned .loro files (logged, not deleted)
    /// 
    /// The filesystem (markdown) is always the source of truth.
    pub async fn reconcile(&mut self) -> Result<ReconcileReport> {
        let mut report = ReconcileReport::default();
        
        // Get all markdown files in the vault
        let md_files: std::collections::HashSet<String> = 
            self.list_files().await?.into_iter().collect();
        
        // Get all .loro files in .sync/documents/
        let loro_hashes = self.list_loro_documents().await?;
        
        // Build mapping: path hash → path
        let path_to_hash: HashMap<String, String> = md_files.iter()
            .map(|path| (path.clone(), simple_hash(path)))
            .collect();
        let hash_to_path: HashMap<String, String> = path_to_hash.iter()
            .map(|(path, hash)| (hash.clone(), path.clone()))
            .collect();
        
        // Track which new files we've already matched to moved files
        let mut matched_new_files: std::collections::HashSet<String> = std::collections::HashSet::new();
        
        // First pass: identify orphaned .loro files and try to match them to new files
        let mut orphaned_docs: Vec<(String, NoteDocument)> = Vec::new();
        
        for hash in &loro_hashes {
            if !hash_to_path.contains_key(hash) {
                // This .loro has no matching markdown file - could be deleted or moved
                let sync_path = format!("{}/documents/{}.loro", SYNC_DIR, hash);
                if let Ok(bytes) = self.fs.read(&sync_path).await {
                    let mut doc = NoteDocument::new("");
                    if doc.import(&bytes).is_ok() {
                        orphaned_docs.push((hash.clone(), doc));
                    }
                }
            }
        }
        
        // Collect new files (markdown exists but no .loro)
        let mut new_files: Vec<String> = Vec::new();
        for path in &md_files {
            let hash = simple_hash(path);
            if !loro_hashes.contains(&hash) {
                new_files.push(path.clone());
            }
        }
        
        // Try to match orphaned .loro files to new markdown files by content
        for (old_hash, orphaned_doc) in &orphaned_docs {
            let orphaned_content_hash = orphaned_doc.content_hash();
            let old_path = orphaned_doc.stored_path().unwrap_or_default();
            
            for new_path in &new_files {
                if matched_new_files.contains(new_path) {
                    continue;
                }
                
                // Read new file content and compute hash
                if let Ok(bytes) = self.fs.read(new_path).await {
                    let content = String::from_utf8_lossy(&bytes);
                    if let Ok(new_doc) = NoteDocument::from_markdown(new_path, &content) {
                        if new_doc.content_hash() == orphaned_content_hash {
                            // Content matches - this is a move!
                            tracing::info!("File move detected: {} -> {}", old_path, new_path);
                            
                            // Migrate the Loro doc to the new path
                            self.migrate_document(old_hash, new_path).await?;
                            
                            report.moved.push(FileMove {
                                from: old_path.clone(),
                                to: new_path.clone(),
                            });
                            matched_new_files.insert(new_path.clone());
                            break;
                        }
                    }
                }
            }
        }
        
        // Process remaining markdown files
        for path in &md_files {
            if matched_new_files.contains(path) {
                // Already handled as a move target
                continue;
            }
            
            let hash = simple_hash(path);
            let sync_path = format!("{}/documents/{}.loro", SYNC_DIR, hash);
            
            if loro_hashes.contains(&hash) {
                // Both exist - check if markdown was modified externally
                if self.needs_reindex(path, &sync_path).await? {
                    tracing::info!("File modified externally, re-indexing: {}", path);
                    self.reindex_file(path).await?;
                    report.reindexed.push(path.clone());
                }
            } else {
                // Truly new file (not a move target)
                tracing::info!("New file detected, indexing: {}", path);
                self.on_file_changed(path).await?;
                report.indexed.push(path.clone());
            }
        }
        
        // Report orphaned .loro files that weren't matched to moves
        for (hash, doc) in &orphaned_docs {
            let old_path = doc.stored_path().unwrap_or_else(|| hash.clone());
            let was_moved = report.moved.iter().any(|m| m.from == old_path);
            if !was_moved {
                tracing::warn!("Orphaned .loro file (deleted?): {}", old_path);
                report.orphaned.push(old_path);
            }
        }
        
        Ok(report)
    }
    
    /// Migrate a Loro document from old path hash to new path.
    /// 
    /// This preserves the CRDT history when a file is moved/renamed.
    async fn migrate_document(&mut self, old_hash: &str, new_path: &str) -> Result<()> {
        let old_sync_path = format!("{}/documents/{}.loro", SYNC_DIR, old_hash);
        let new_hash = simple_hash(new_path);
        let new_sync_path = format!("{}/documents/{}.loro", SYNC_DIR, new_hash);
        
        // Load the old document
        let bytes = self.fs.read(&old_sync_path).await?;
        let mut doc = NoteDocument::new(new_path);
        doc.import(&bytes)?;
        
        // Update the path in metadata
        doc.update_path(new_path)?;
        
        // Save to new location
        let snapshot = doc.export_snapshot();
        self.fs.write(&new_sync_path, &snapshot).await?;
        
        // Delete old file
        self.fs.delete(&old_sync_path).await?;
        
        // Update cache
        self.documents.insert(new_path.to_string(), doc);
        
        Ok(())
    }
    
    /// List all .loro document hashes in .sync/documents/
    async fn list_loro_documents(&self) -> Result<std::collections::HashSet<String>> {
        let mut hashes = std::collections::HashSet::new();
        let docs_dir = format!("{}/documents", SYNC_DIR);
        
        if !self.fs.exists(&docs_dir).await? {
            return Ok(hashes);
        }
        
        let entries = self.fs.list(&docs_dir).await?;
        for entry in entries {
            if !entry.is_dir && entry.name.ends_with(".loro") {
                // Extract hash from filename (remove .loro extension)
                let hash = entry.name.trim_end_matches(".loro").to_string();
                hashes.insert(hash);
            }
        }
        
        Ok(hashes)
    }
    
    /// Check if a file needs re-indexing (markdown content differs from Loro state)
    async fn needs_reindex(&self, md_path: &str, loro_path: &str) -> Result<bool> {
        // Read markdown content
        let md_bytes = self.fs.read(md_path).await?;
        let md_content = String::from_utf8_lossy(&md_bytes);
        
        // Load Loro doc and convert to markdown
        let loro_bytes = self.fs.read(loro_path).await?;
        let mut doc = NoteDocument::new(md_path);
        if doc.import(&loro_bytes).is_err() {
            // Corrupted Loro doc - needs reindex
            return Ok(true);
        }
        let loro_content = doc.to_markdown();
        
        // Compare (normalize line endings)
        let md_normalized = md_content.replace("\r\n", "\n");
        let loro_normalized = loro_content.replace("\r\n", "\n");
        
        Ok(md_normalized != loro_normalized)
    }
    
    /// Re-index a file by creating a fresh Loro doc from markdown.
    /// 
    /// This is used when external modifications are detected.
    /// Creates a new Loro doc with a fresh version vector.
    async fn reindex_file(&mut self, path: &str) -> Result<()> {
        let bytes = self.fs.read(path).await?;
        let content = String::from_utf8_lossy(&bytes);
        
        // Create fresh Loro doc from markdown
        let new_doc = NoteDocument::from_markdown(path, &content)?;
        
        // Save to .sync
        let sync_path = self.document_sync_path(path);
        let snapshot = new_doc.export_snapshot();
        self.fs.write(&sync_path, &snapshot).await?;
        
        // Update cache
        self.documents.insert(path.to_string(), new_doc);
        
        Ok(())
    }

    /// Get our peer ID
    pub fn peer_id(&self) -> &str {
        &self.peer_id
    }

    /// Get the version vector for a document as encoded bytes.
    ///
    /// Returns None if the document hasn't been loaded.
    /// Use this for tracking which version was synced to detect if a local
    /// modification contains only changes we just received from sync.
    pub async fn get_document_version(&mut self, path: &str) -> Result<Option<Vec<u8>>> {
        if !self.documents.contains_key(path) {
            // Try to load the document
            let sync_path = self.document_sync_path(path);
            if !self.fs.exists(&sync_path).await? {
                return Ok(None);
            }
            let doc = self.load_document(path).await?;
            self.documents.insert(path.to_string(), doc);
        }

        Ok(self.documents.get(path).map(|doc| doc.version().encode()))
    }

    /// Check if a document's current version includes all operations from a previous version.
    ///
    /// Returns true if `current_version` contains all operations from `synced_version`.
    /// This is used to detect if a local file modification is purely from sync
    /// (version unchanged) or includes local edits (new operations added).
    ///
    /// Version vectors use causal ordering - if A includes B, then A has seen
    /// all operations that B has seen.
    pub fn version_includes(current_version: &[u8], synced_version: &[u8]) -> bool {
        let Ok(current) = loro::VersionVector::decode(current_version) else {
            return false;
        };
        let Ok(synced) = loro::VersionVector::decode(synced_version) else {
            return false;
        };
        current.includes_vv(&synced)
    }

    /// Check if vault is initialized
    pub async fn is_initialized(&self) -> Result<bool> {
        Ok(self.fs.exists(SYNC_DIR).await?)
    }

    /// Get or load a document
    pub async fn get_document(&mut self, path: &str) -> Result<&NoteDocument> {
        if !self.documents.contains_key(path) {
            let doc = self.load_document(path).await?;
            self.documents.insert(path.to_string(), doc);
        }
        Ok(self.documents.get(path).unwrap())
    }

    /// Get a mutable reference to a document
    pub async fn get_document_mut(&mut self, path: &str) -> Result<&mut NoteDocument> {
        if !self.documents.contains_key(path) {
            let doc = self.load_document(path).await?;
            self.documents.insert(path.to_string(), doc);
        }
        Ok(self.documents.get_mut(path).unwrap())
    }

    /// Load a document from disk
    async fn load_document(&self, path: &str) -> Result<NoteDocument> {
        // Try to load from .sync first (for Loro state)
        let sync_path = self.document_sync_path(path);
        if self.fs.exists(&sync_path).await? {
            let bytes = self.fs.read(&sync_path).await?;
            let mut doc = NoteDocument::new(path);
            doc.import(&bytes)?;
            return Ok(doc);
        }

        // Otherwise load from markdown file
        if self.fs.exists(path).await? {
            let bytes = self.fs.read(path).await?;
            let content = String::from_utf8_lossy(&bytes);
            return Ok(NoteDocument::from_markdown(path, &content)?);
        }

        // New document
        Ok(NoteDocument::new(path))
    }

    /// Get the sync storage path for a document
    fn document_sync_path(&self, path: &str) -> String {
        // Simple hash-based naming
        let hash = simple_hash(path);
        format!("{}/documents/{}.loro", SYNC_DIR, hash)
    }

    /// Handle a file change (from file watcher or Obsidian event).
    ///
    /// Uses diff-and-merge to update existing documents, preserving peer ID.
    /// Only creates a new document if one doesn't exist yet.
    pub async fn on_file_changed(&mut self, path: &str) -> Result<()> {
        // Skip non-markdown files and .sync directory
        if !path.ends_with(".md") || path.starts_with(SYNC_DIR) {
            return Ok(());
        }

        // Load the current file content
        let bytes = self.fs.read(path).await?;
        let content = String::from_utf8_lossy(&bytes);
        let parsed = crate::markdown::parse(&content);

        // If document exists, diff-and-merge (preserves peer ID)
        if self.documents.contains_key(path) {
            let existing_doc = self.documents.get(path).unwrap();
            let body_changed = existing_doc.update_body(&parsed.body)?;
            let fm_changed = existing_doc.update_frontmatter(parsed.frontmatter.as_ref())?;

            if body_changed || fm_changed {
                // Single commit for all changes
                existing_doc.commit();

                // Save updated sync state
                let sync_path = self.document_sync_path(path);
                let snapshot = existing_doc.export_snapshot();
                self.fs.write(&sync_path, &snapshot).await?;
                tracing::debug!("Updated document via diff: {}", path);
            } else {
                tracing::debug!("No changes detected (sync echo): {}", path);
            }
            return Ok(());
        }

        // Document doesn't exist - create new (this is the only time we need new peer ID)
        let new_doc = NoteDocument::from_markdown(path, &content)?;
        let sync_path = self.document_sync_path(path);
        let snapshot = new_doc.export_snapshot();
        self.fs.write(&sync_path, &snapshot).await?;
        self.documents.insert(path.to_string(), new_doc);
        tracing::debug!("Created new document: {}", path);

        Ok(())
    }

    /// Save a document to disk (both markdown and sync state)
    pub async fn save_document(&self, path: &str) -> Result<()> {
        if let Some(doc) = self.documents.get(path) {
            // Save markdown
            let markdown = doc.to_markdown();
            self.fs.write(path, markdown.as_bytes()).await?;

            // Save sync state
            let sync_path = self.document_sync_path(path);
            let snapshot = doc.export_snapshot();
            self.fs.write(&sync_path, &snapshot).await?;
        }
        Ok(())
    }

    /// List all markdown files in the vault
    pub async fn list_files(&self) -> Result<Vec<String>> {
        let mut files = Vec::new();
        let mut dirs_to_visit = vec![String::new()]; // Start with root

        while let Some(dir) = dirs_to_visit.pop() {
            let entries = self.fs.list(&dir).await?;

            for entry in entries {
                let path = if dir.is_empty() {
                    entry.name.clone()
                } else {
                    format!("{}/{}", dir, entry.name)
                };

                // Skip .sync directory and hidden files
                if path.starts_with(SYNC_DIR) || path.starts_with('.') {
                    continue;
                }

                if entry.is_dir {
                    dirs_to_visit.push(path);
                } else if path.ends_with(".md") {
                    files.push(path);
                }
            }
        }

        Ok(files)
    }

    /// Index all existing markdown files in the vault.
    ///
    /// Called during initialization to ensure all files are tracked
    /// by the CRDT before any sync operations.
    async fn index_existing_files(&mut self) -> Result<()> {
        let files = self.list_files().await?;
        
        for path in files {
            // Process each file as if it was just changed
            // This creates the Loro document and saves the sync state
            if let Err(e) = self.on_file_changed(&path).await {
                // Log but don't fail - some files might have issues
                tracing::warn!("Failed to index file {}: {}", path, e);
            }
        }

        Ok(())
    }
}

/// Simple string hash for deterministic file naming
fn simple_hash(s: &str) -> String {
    use std::collections::hash_map::DefaultHasher;
    use std::hash::{Hash, Hasher};

    let mut hasher = DefaultHasher::new();
    s.hash(&mut hasher);
    format!("{:016x}", hasher.finish())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fs::InMemoryFs;

    #[tokio::test]
    async fn test_vault_init() {
        let fs = InMemoryFs::new();
        let vault = Vault::init(fs, "peer1".to_string()).await.unwrap();

        assert!(vault.is_initialized().await.unwrap());
    }

    #[tokio::test]
    async fn test_vault_file_change() {
        let fs = InMemoryFs::new();

        // Create a markdown file
        fs.write("test.md", b"# Hello\n\nWorld").await.unwrap();

        // Init vault
        let mut vault = Vault::init(fs, "peer1".to_string()).await.unwrap();

        // Handle file change
        vault.on_file_changed("test.md").await.unwrap();

        // Get document
        let doc = vault.get_document("test.md").await.unwrap();
        assert!(doc.to_markdown().contains("Hello"));
    }
    
    #[tokio::test]
    async fn test_reconcile_detects_new_files() {
        use std::sync::Arc;
        
        let fs = Arc::new(InMemoryFs::new());
        
        // Initialize vault with one file
        fs.write("existing.md", b"# Existing").await.unwrap();
        let _vault = Vault::init(Arc::clone(&fs), "peer1".to_string()).await.unwrap();
        
        // Simulate adding a new file while plugin was off
        fs.write("new_file.md", b"# New File").await.unwrap();
        
        // Load vault - should detect and index the new file
        let mut vault = Vault::load(Arc::clone(&fs), "peer1".to_string()).await.unwrap();
        
        // The new file should be accessible
        let doc = vault.get_document("new_file.md").await.unwrap();
        assert!(doc.to_markdown().contains("New File"));
    }
    
    #[tokio::test]
    async fn test_reconcile_detects_modified_files() {
        use std::sync::Arc;
        
        let fs = Arc::new(InMemoryFs::new());
        
        // Initialize vault with one file
        fs.write("note.md", b"# Original Content").await.unwrap();
        let _vault = Vault::init(Arc::clone(&fs), "peer1".to_string()).await.unwrap();
        
        // Simulate modifying the file while plugin was off
        fs.write("note.md", b"# Modified Content").await.unwrap();
        
        // Load vault - should detect modification and re-index
        let mut vault = Vault::load(Arc::clone(&fs), "peer1".to_string()).await.unwrap();
        
        // The document should have the new content
        let doc = vault.get_document("note.md").await.unwrap();
        assert!(doc.to_markdown().contains("Modified Content"));
    }
    
    #[tokio::test]
    async fn test_reconcile_detects_deleted_files() {
        use std::sync::Arc;
        
        let fs = Arc::new(InMemoryFs::new());
        
        // Initialize vault with two files
        fs.write("keep.md", b"# Keep this").await.unwrap();
        fs.write("delete.md", b"# Delete this").await.unwrap();
        let _vault = Vault::init(Arc::clone(&fs), "peer1".to_string()).await.unwrap();
        
        // Simulate deleting a file while plugin was off
        fs.delete("delete.md").await.unwrap();
        
        // Load vault - should detect orphaned .loro file
        let vault = Vault::load(Arc::clone(&fs), "peer1".to_string()).await.unwrap();
        
        // list_files should not include the deleted file
        let files = vault.list_files().await.unwrap();
        assert!(!files.contains(&"delete.md".to_string()));
        assert!(files.contains(&"keep.md".to_string()));
    }
    
    #[tokio::test]
    async fn test_reconcile_detects_file_move() {
        use std::sync::Arc;
        
        let fs = Arc::new(InMemoryFs::new());
        
        // Initialize vault with a file
        fs.write("old_name.md", b"# Unique Content ABC123").await.unwrap();
        let _vault = Vault::init(Arc::clone(&fs), "peer1".to_string()).await.unwrap();
        
        // Simulate renaming the file while plugin was off
        let content = fs.read("old_name.md").await.unwrap();
        fs.write("new_name.md", &content).await.unwrap();
        fs.delete("old_name.md").await.unwrap();
        
        // Load vault - should detect move and migrate .loro file
        let mut vault = Vault::load(Arc::clone(&fs), "peer1".to_string()).await.unwrap();
        
        // The new file should be accessible with the same content
        let doc = vault.get_document("new_name.md").await.unwrap();
        assert!(doc.to_markdown().contains("Unique Content ABC123"));
        
        // The old file should not be in the list
        let files = vault.list_files().await.unwrap();
        assert!(!files.contains(&"old_name.md".to_string()));
        assert!(files.contains(&"new_name.md".to_string()));
        
        // Check that the .loro file was migrated (old one deleted, new one exists)
        let old_hash = simple_hash("old_name.md");
        let new_hash = simple_hash("new_name.md");
        assert!(!fs.exists(&format!("{}/documents/{}.loro", SYNC_DIR, old_hash)).await.unwrap());
        assert!(fs.exists(&format!("{}/documents/{}.loro", SYNC_DIR, new_hash)).await.unwrap());
    }
    
    #[tokio::test]
    async fn test_reconcile_detects_file_move_to_subfolder() {
        use std::sync::Arc;
        
        let fs = Arc::new(InMemoryFs::new());
        
        // Initialize vault with a file at root
        fs.write("note.md", b"# My Note XYZ789").await.unwrap();
        let _vault = Vault::init(Arc::clone(&fs), "peer1".to_string()).await.unwrap();
        
        // Simulate moving file to subfolder while plugin was off
        let content = fs.read("note.md").await.unwrap();
        fs.mkdir("knowledge").await.unwrap();
        fs.write("knowledge/note.md", &content).await.unwrap();
        fs.delete("note.md").await.unwrap();
        
        // Load vault - should detect move
        let mut vault = Vault::load(Arc::clone(&fs), "peer1".to_string()).await.unwrap();
        
        // The moved file should be accessible
        let doc = vault.get_document("knowledge/note.md").await.unwrap();
        assert!(doc.to_markdown().contains("My Note XYZ789"));
        
        // Only the new path should exist
        let files = vault.list_files().await.unwrap();
        assert!(!files.contains(&"note.md".to_string()));
        assert!(files.contains(&"knowledge/note.md".to_string()));
    }
}
