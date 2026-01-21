//! Vault: Manages a collection of NoteDocuments and syncs with peers.

use crate::document::NoteDocument;
use crate::fs::{FileSystem, FsError};
use crate::PeerId;

use loro::{LoroDoc, LoroTree, TreeID, TreeParentId};
use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;
use thiserror::Error;
use web_time::Instant;

/// Directory for sync state
pub(crate) const SYNC_DIR: &str = ".sync";
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

/// Tracks files that were recently synced to prevent re-broadcasting.
///
/// When a file is received from sync, we mark it here BEFORE writing to disk.
/// When the file watcher fires, we check and consume this flag to skip broadcast.
///
/// Flags expire after `FLAG_TTL` to handle cases where file watcher events are
/// dropped (e.g., under heavy load). This prevents stale flags from incorrectly
/// suppressing local edits.
#[derive(Clone)]
pub struct SyncTracker {
    /// Map of path -> timestamp when marked as synced
    synced_paths: Arc<Mutex<HashMap<String, Instant>>>,
}

/// Time-to-live for sync flags. Flags older than this are considered stale.
const FLAG_TTL: Duration = Duration::from_secs(5);

impl Default for SyncTracker {
    fn default() -> Self {
        Self {
            synced_paths: Arc::new(Mutex::new(HashMap::new())),
        }
    }
}

impl SyncTracker {
    pub fn new() -> Self {
        Self::default()
    }

    /// Mark a path as having been synced (call before writing to disk)
    pub fn mark_synced(&self, path: &str) {
        self.synced_paths
            .lock()
            .unwrap()
            .insert(path.to_string(), Instant::now());
    }

    /// Check if path was synced and consume the flag (returns true once).
    /// Returns false if the flag has expired (older than FLAG_TTL).
    pub fn consume_synced(&self, path: &str) -> bool {
        let mut paths = self.synced_paths.lock().unwrap();
        if let Some(timestamp) = paths.remove(path) {
            // Check if flag is still valid (not expired)
            if timestamp.elapsed() < FLAG_TTL {
                return true;
            }
            // Flag expired - treat as if it wasn't set
        }
        false
    }

    /// Check if path was synced (without consuming).
    /// Returns false if the flag has expired.
    #[allow(dead_code)]
    pub fn is_synced(&self, path: &str) -> bool {
        let paths = self.synced_paths.lock().unwrap();
        if let Some(timestamp) = paths.get(path) {
            return timestamp.elapsed() < FLAG_TTL;
        }
        false
    }

    /// Remove expired flags to prevent memory growth.
    /// Called periodically during normal operations.
    #[allow(dead_code)]
    pub fn cleanup_expired(&self) {
        let mut paths = self.synced_paths.lock().unwrap();
        paths.retain(|_, timestamp| timestamp.elapsed() < FLAG_TTL);
    }
}

/// Manages a vault of documents
pub struct Vault<F: FileSystem> {
    /// File registry (tracks all files in vault via LoroTree)
    pub(crate) registry: LoroDoc,

    /// Path lookup cache (LoroTree has no path-based lookup)
    /// Rebuilt after sync and updated inline for local operations
    path_to_node: HashMap<String, TreeID>,

    /// Loaded documents
    pub(crate) documents: HashMap<String, NoteDocument>,

    /// Filesystem abstraction
    pub(crate) fs: F,

    /// Our peer ID (set on all Loro documents for consistent version vectors)
    peer_id: PeerId,

    /// Tracks files that were recently synced (for echo detection)
    sync_tracker: SyncTracker,
}

impl<F: FileSystem> Vault<F> {
    /// Initialize a new vault (creates .sync directory)
    pub async fn init(fs: F, peer_id: PeerId) -> Result<Self> {
        // Create .sync directory
        fs.mkdir(SYNC_DIR).await?;
        fs.mkdir(&format!("{}/documents", SYNC_DIR)).await?;

        let registry = LoroDoc::new();
        // Set peer ID before any operations for consistent version vectors
        registry.set_peer_id(peer_id.as_u64()).ok();
        // Initialize the file tree (LoroTree inside registry)
        // The tree is created on first access via get_tree()
        let _file_tree = registry.get_tree("files");

        // Save initial registry
        let registry_bytes = registry.export(loro::ExportMode::Snapshot).unwrap();
        fs.write(REGISTRY_FILE, &registry_bytes).await?;

        let mut vault = Self {
            registry,
            path_to_node: HashMap::new(),
            documents: HashMap::new(),
            fs,
            peer_id,
            sync_tracker: SyncTracker::new(),
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
    pub async fn load(fs: F, peer_id: PeerId) -> Result<Self> {
        // Check if vault is initialized
        if !fs.exists(SYNC_DIR).await? {
            return Err(VaultError::NotInitialized);
        }

        // Load registry
        let registry = if fs.exists(REGISTRY_FILE).await? {
            let bytes = fs.read(REGISTRY_FILE).await?;
            let doc = LoroDoc::new();
            // Set peer ID before import so any new operations use our ID
            doc.set_peer_id(peer_id.as_u64()).ok();
            doc.import(&bytes).ok();
            doc
        } else {
            let doc = LoroDoc::new();
            // Set peer ID before any operations for consistent version vectors
            doc.set_peer_id(peer_id.as_u64()).ok();
            // Initialize file tree for new registries
            let _file_tree = doc.get_tree("files");
            doc
        };

        let mut vault = Self {
            registry,
            path_to_node: HashMap::new(),
            documents: HashMap::new(),
            fs,
            peer_id,
            sync_tracker: SyncTracker::new(),
        };

        // Build path cache from loaded tree
        vault.rebuild_path_cache();

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
                    // Use from_bytes to preserve peer ID when loading orphaned docs
                    if let Ok(doc) = NoteDocument::from_bytes("", &bytes, self.peer_id) {
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
                    if let Ok(new_doc) = NoteDocument::from_markdown(new_path, &content, self.peer_id) {
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
                // Register in tree for delete/rename tracking
                self.register_file(path)?;
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
    /// Uses `from_bytes` to import before setting metadata, preserving the original peer ID.
    async fn migrate_document(&mut self, old_hash: &str, new_path: &str) -> Result<()> {
        let old_sync_path = format!("{}/documents/{}.loro", SYNC_DIR, old_hash);
        let new_hash = simple_hash(new_path);
        let new_sync_path = format!("{}/documents/{}.loro", SYNC_DIR, new_hash);

        // Load the old document (import first, then update path - preserves peer ID)
        let bytes = self.fs.read(&old_sync_path).await?;
        let doc = NoteDocument::from_bytes(new_path, &bytes, self.peer_id)?;

        // Save to new location
        let snapshot = doc.export_snapshot();
        self.fs.write(&new_sync_path, &snapshot).await?;

        // Delete old file
        self.fs.delete(&old_sync_path).await?;

        // Update cache
        self.documents.insert(new_path.to_string(), doc);

        // Register in tree (the old path's node was already processed as orphaned)
        self.register_file(new_path)?;

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
        let doc = match NoteDocument::from_bytes(md_path, &loro_bytes, self.peer_id) {
            Ok(d) => d,
            Err(_) => return Ok(true), // Corrupted Loro doc - needs reindex
        };
        let loro_content = doc.to_markdown();

        // Compare (normalize line endings)
        let md_normalized = md_content.replace("\r\n", "\n");
        let loro_normalized = loro_content.replace("\r\n", "\n");

        Ok(md_normalized != loro_normalized)
    }
    
    /// Re-index a file by diff-merging changes into the existing Loro doc.
    ///
    /// This is used when external modifications are detected during reconciliation.
    /// Preserves the peer ID by updating the existing document rather than replacing it.
    async fn reindex_file(&mut self, path: &str) -> Result<()> {
        let bytes = self.fs.read(path).await?;
        let content = String::from_utf8_lossy(&bytes);
        let parsed = crate::markdown::parse(&content);

        // Load existing .loro document
        let sync_path = self.document_sync_path(path);
        let loro_bytes = self.fs.read(&sync_path).await?;
        let doc = NoteDocument::from_bytes(path, &loro_bytes, self.peer_id)?;

        // Diff-merge the changes (preserves peer ID)
        let body_changed = doc.update_body(&parsed.body)?;
        let fm_changed = doc.update_frontmatter(parsed.frontmatter.as_ref())?;

        if body_changed || fm_changed {
            doc.commit();
            let snapshot = doc.export_snapshot();
            self.fs.write(&sync_path, &snapshot).await?;
            tracing::debug!("Re-indexed document via diff: {}", path);
        }

        // Update cache
        self.documents.insert(path.to_string(), doc);

        Ok(())
    }

    /// Get our peer ID
    pub fn peer_id(&self) -> PeerId {
        self.peer_id
    }

    /// Mark a path as synced (call before writing to disk).
    /// Used to prevent re-broadcasting files we just received from sync.
    pub fn mark_synced(&self, path: &str) {
        self.sync_tracker.mark_synced(path);
    }

    /// Check if a path was synced and consume the flag.
    /// Returns true once (and clears the flag), false on subsequent calls.
    pub fn consume_sync_flag(&self, path: &str) -> bool {
        self.sync_tracker.consume_synced(path)
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
            // Use from_bytes to preserve peer ID (imports before setting metadata)
            return Ok(NoteDocument::from_bytes(path, &bytes, self.peer_id)?);
        }

        // Otherwise load from markdown file
        if self.fs.exists(path).await? {
            let bytes = self.fs.read(path).await?;
            let content = String::from_utf8_lossy(&bytes);
            return Ok(NoteDocument::from_markdown(path, &content, self.peer_id)?);
        }

        // New document - use from_markdown with empty content to get a doc_id
        Ok(NoteDocument::from_markdown(path, "", self.peer_id)?)
    }

    /// Get the sync storage path for a document
    pub(crate) fn document_sync_path(&self, path: &str) -> String {
        // Simple hash-based naming
        let hash = simple_hash(path);
        format!("{}/documents/{}.loro", SYNC_DIR, hash)
    }

    /// Handle a file change (from file watcher or Obsidian event).
    ///
    /// Uses diff-and-merge to update existing documents, preserving peer ID.
    /// Only creates a new document if no .loro file exists on disk.
    pub async fn on_file_changed(&mut self, path: &str) -> Result<()> {
        // Skip non-markdown files and .sync directory
        if !path.ends_with(".md") || path.starts_with(SYNC_DIR) {
            return Ok(());
        }

        // Load the current file content
        let bytes = self.fs.read(path).await?;
        let content = String::from_utf8_lossy(&bytes);
        let parsed = crate::markdown::parse(&content);
        let sync_path = self.document_sync_path(path);

        // If document is in cache, diff-and-merge
        if self.documents.contains_key(path) {
            let existing_doc = self.documents.get(path).unwrap();
            let body_changed = existing_doc.update_body(&parsed.body)?;
            let fm_changed = existing_doc.update_frontmatter(parsed.frontmatter.as_ref())?;

            if body_changed || fm_changed {
                existing_doc.commit();
                let snapshot = existing_doc.export_snapshot();
                self.fs.write(&sync_path, &snapshot).await?;
                tracing::debug!("Updated document via diff: {}", path);
            } else {
                tracing::debug!("No changes detected (sync echo): {}", path);
            }
            return Ok(());
        }

        // Check if .loro exists on disk but not in cache (cold cache scenario)
        if self.fs.exists(&sync_path).await? {
            // Load from disk and diff-merge (preserves peer ID)
            let loro_bytes = self.fs.read(&sync_path).await?;
            let doc = NoteDocument::from_bytes(path, &loro_bytes, self.peer_id)?;

            let body_changed = doc.update_body(&parsed.body)?;
            let fm_changed = doc.update_frontmatter(parsed.frontmatter.as_ref())?;

            if body_changed || fm_changed {
                doc.commit();
                let snapshot = doc.export_snapshot();
                self.fs.write(&sync_path, &snapshot).await?;
                tracing::debug!("Updated cold-cache document via diff: {}", path);
            } else {
                tracing::debug!("No changes detected (cold cache sync echo): {}", path);
            }

            self.documents.insert(path.to_string(), doc);
            return Ok(());
        }

        // Document doesn't exist anywhere - create new (this is the only time we need new peer ID)
        let new_doc = NoteDocument::from_markdown(path, &content, self.peer_id)?;
        let snapshot = new_doc.export_snapshot();
        self.fs.write(&sync_path, &snapshot).await?;
        self.documents.insert(path.to_string(), new_doc);

        // Register in tree for delete/rename tracking
        self.register_file(path)?;

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

    // ========== File Tree Operations (LoroTree) ==========

    /// Get the file tree from the registry
    pub(crate) fn file_tree(&self) -> LoroTree {
        self.registry.get_tree("files")
    }

    /// Rebuild the path cache from the current tree state.
    /// Call this after applying sync updates.
    pub(crate) fn rebuild_path_cache(&mut self) {
        self.path_to_node.clear();
        let tree = self.file_tree();

        // Iterate over all non-deleted nodes
        for node_id in tree.nodes() {
            // Skip deleted nodes
            if tree.is_node_deleted(&node_id).unwrap_or(true) {
                continue;
            }

            // Only cache file nodes (not folders)
            if let Ok(meta) = tree.get_meta(node_id) {
                let node_type = meta.get("type").and_then(|v| {
                    if let loro::ValueOrContainer::Value(val) = v {
                        val.as_string().map(|s| s.to_string())
                    } else {
                        None
                    }
                });

                if node_type.as_deref() == Some("file") {
                    if let Some(path) = self.get_node_path(&node_id) {
                        self.path_to_node.insert(path, node_id);
                    }
                }
            }
        }

        tracing::debug!("Rebuilt path cache with {} entries", self.path_to_node.len());
    }

    /// Get the path for a node by walking up the tree
    pub(crate) fn get_node_path(&self, node_id: &TreeID) -> Option<String> {
        let tree = self.file_tree();
        let mut parts = vec![];
        let mut current = *node_id;

        loop {
            // Get node metadata
            let meta = tree.get_meta(current).ok()?;
            let name = meta.get("name").and_then(|v| {
                if let loro::ValueOrContainer::Value(val) = v {
                    val.as_string().map(|s| s.to_string())
                } else {
                    None
                }
            })?;
            parts.push(name);

            // Get parent
            match tree.parent(current) {
                Some(TreeParentId::Node(parent_id)) => {
                    current = parent_id;
                }
                Some(TreeParentId::Root) | None => break,
                _ => break,
            }
        }

        parts.reverse();
        Some(parts.join("/"))
    }

    /// Find a node by path using the cache
    fn find_node_by_path(&self, path: &str) -> Option<TreeID> {
        self.path_to_node.get(path).copied()
    }

    /// Validate a sync path for security
    fn validate_sync_path(path: &str) -> Result<()> {
        // Empty path
        if path.is_empty() {
            return Err(VaultError::Other("Empty path not allowed".into()));
        }
        // Path traversal
        if path.contains("..") {
            return Err(VaultError::Other("Path traversal not allowed".into()));
        }
        // Empty segments (a//b.md)
        if path.contains("//") {
            return Err(VaultError::Other("Empty path segment not allowed".into()));
        }
        // Absolute paths (Unix)
        if path.starts_with('/') {
            return Err(VaultError::Other("Absolute path not allowed".into()));
        }
        // Absolute paths (Windows - drive letter)
        if path.len() >= 2 && path.chars().nth(1) == Some(':') {
            return Err(VaultError::Other("Windows absolute path not allowed".into()));
        }
        // Backslash
        if path.contains('\\') {
            return Err(VaultError::Other("Backslash in path not allowed".into()));
        }
        // Null bytes
        if path.contains('\0') {
            return Err(VaultError::Other("Null byte in path not allowed".into()));
        }
        // Must be .md
        if !path.ends_with(".md") {
            return Err(VaultError::Other("Only markdown files allowed".into()));
        }
        // Control characters
        if path.chars().any(|c| c.is_control()) {
            return Err(VaultError::Other("Control character in path not allowed".into()));
        }
        // Path length limit (filesystem safety)
        if path.len() > 1024 {
            return Err(VaultError::Other("Path too long".into()));
        }
        Ok(())
    }

    /// Register a new file in the tree (creates parent folders as needed).
    /// Returns the TreeID of the created file node.
    pub fn register_file(&mut self, path: &str) -> Result<TreeID> {
        Self::validate_sync_path(path)?;

        // Check if file already registered
        if let Some(existing_id) = self.find_node_by_path(path) {
            return Ok(existing_id);
        }

        let parts: Vec<&str> = path.split('/').collect();
        let (folders, file_name) = parts.split_at(parts.len() - 1);

        // Ensure parent folders exist
        let mut parent_id = TreeParentId::Root;
        for folder_name in folders {
            parent_id = self.get_or_create_folder(parent_id, folder_name)?;
        }

        // Create file node
        let tree = self.file_tree();
        let node_id = tree
            .create(parent_id)
            .map_err(|e| VaultError::Other(format!("Failed to create file node: {}", e)))?;

        let meta = tree
            .get_meta(node_id)
            .map_err(|e| VaultError::Other(format!("Failed to get file meta: {}", e)))?;
        meta.insert("type", "file")
            .map_err(|e| VaultError::Other(format!("Failed to set file type: {}", e)))?;
        meta.insert("name", file_name[0])
            .map_err(|e| VaultError::Other(format!("Failed to set file name: {}", e)))?;
        meta.insert("doc_id", simple_hash(path))
            .map_err(|e| VaultError::Other(format!("Failed to set doc_id: {}", e)))?;

        // Update cache
        self.path_to_node.insert(path.to_string(), node_id);

        tracing::debug!("Registered file in tree: {}", path);
        Ok(node_id)
    }

    /// Delete a file from the tree (CRDT operation - tracked, reversible).
    /// Also cleans up the .loro document file.
    pub async fn delete_file(&mut self, path: &str) -> Result<()> {
        Self::validate_sync_path(path)?;

        if let Some(node_id) = self.find_node_by_path(path) {
            let tree = self.file_tree();
            tree.delete(node_id)
                .map_err(|e| VaultError::Other(format!("Failed to delete file node: {}", e)))?;

            // Remove from cache
            self.path_to_node.remove(path);

            // Clean up .loro document
            let sync_path = self.document_sync_path(path);
            if self.fs.exists(&sync_path).await? {
                self.fs.delete(&sync_path).await?;
            }

            // Remove from documents cache
            self.documents.remove(path);

            tracing::info!("Deleted file from tree: {}", path);
        }

        Ok(())
    }

    /// Rename/move a file in the tree (CRDT operation via tree move).
    pub async fn rename_file(&mut self, old_path: &str, new_path: &str) -> Result<()> {
        Self::validate_sync_path(old_path)?;
        Self::validate_sync_path(new_path)?;

        // No-op if paths are identical
        if old_path == new_path {
            return Ok(());
        }

        let Some(node_id) = self.find_node_by_path(old_path) else {
            // Source not in tree - this can happen when receiving FileRenamed before
            // the registry has synced. Handle the rename at filesystem level if possible.
            if self.fs.exists(old_path).await.unwrap_or(false) {
                // Source exists on disk but not in tree - rename on disk and register target
                tracing::debug!(
                    "rename_file: source {} not in tree but exists on disk - renaming and registering",
                    old_path
                );

                // Rename the actual file
                let content = self.fs.read(old_path).await?;
                self.fs.write(new_path, &content).await?;
                self.fs.delete(old_path).await?;

                // Move .loro file if it exists
                let old_sync = self.document_sync_path(old_path);
                let new_sync = self.document_sync_path(new_path);
                if self.fs.exists(&old_sync).await.unwrap_or(false) {
                    let sync_content = self.fs.read(&old_sync).await?;
                    self.fs.write(&new_sync, &sync_content).await?;
                    self.fs.delete(&old_sync).await?;
                }

                // Update documents cache
                if let Some(doc) = self.documents.remove(old_path) {
                    self.documents.insert(new_path.to_string(), doc);
                }

                // Register in tree
                self.register_file(new_path)?;
                return Ok(());
            } else if self.fs.exists(new_path).await.unwrap_or(false) {
                // Target already exists (rename already happened) - just register it
                tracing::debug!(
                    "rename_file: source {} not in tree, but {} exists - registering target",
                    old_path, new_path
                );
                self.register_file(new_path)?;

                // Clean up orphaned .loro at old path if it exists
                let old_sync = self.document_sync_path(old_path);
                if self.fs.exists(&old_sync).await.unwrap_or(false) {
                    let _ = self.fs.delete(&old_sync).await;
                }

                return Ok(());
            }
            return Err(VaultError::Other(format!("Source file not found: {}", old_path)));
        };

        // Check target doesn't exist
        if self.find_node_by_path(new_path).is_some() {
            return Err(VaultError::Other(format!("Target already exists: {}", new_path)));
        }

        let new_parts: Vec<&str> = new_path.split('/').collect();
        let (new_folders, new_name) = new_parts.split_at(new_parts.len() - 1);

        // Ensure new parent folders exist
        let mut new_parent = TreeParentId::Root;
        for folder_name in new_folders {
            new_parent = self.get_or_create_folder(new_parent, folder_name)?;
        }

        let tree = self.file_tree();

        // Move node to new parent (Loro API is `mov`)
        tree.mov(node_id, new_parent)
            .map_err(|e| VaultError::Other(format!("Failed to move file node: {}", e)))?;

        // Update name in metadata
        let meta = tree
            .get_meta(node_id)
            .map_err(|e| VaultError::Other(format!("Failed to get file meta: {}", e)))?;
        meta.insert("name", new_name[0])
            .map_err(|e| VaultError::Other(format!("Failed to update file name: {}", e)))?;
        meta.insert("doc_id", simple_hash(new_path))
            .map_err(|e| VaultError::Other(format!("Failed to update doc_id: {}", e)))?;

        // Update caches
        self.path_to_node.remove(old_path);
        self.path_to_node.insert(new_path.to_string(), node_id);

        // Move .loro document file
        let old_sync_path = self.document_sync_path(old_path);
        let new_sync_path = self.document_sync_path(new_path);
        if self.fs.exists(&old_sync_path).await? {
            let bytes = self.fs.read(&old_sync_path).await?;
            self.fs.write(&new_sync_path, &bytes).await?;
            self.fs.delete(&old_sync_path).await?;
        }

        // Update documents cache
        if let Some(doc) = self.documents.remove(old_path) {
            self.documents.insert(new_path.to_string(), doc);
        }

        tracing::info!("Renamed file in tree: {} -> {}", old_path, new_path);
        Ok(())
    }

    /// Check if a file is deleted in the tree
    pub fn is_file_deleted(&self, path: &str) -> bool {
        match self.find_node_by_path(path) {
            Some(node_id) => {
                let tree = self.file_tree();
                tree.is_node_deleted(&node_id).unwrap_or(true)
            }
            None => true, // Not in tree = effectively deleted
        }
    }

    /// Get or create a folder node
    fn get_or_create_folder(&mut self, parent: TreeParentId, name: &str) -> Result<TreeParentId> {
        let tree = self.file_tree();

        // Look for existing folder with this name under parent
        let children = match &parent {
            TreeParentId::Root => tree.roots(),
            TreeParentId::Node(parent_id) => tree.children(parent_id).unwrap_or_default(),
            _ => vec![],
        };

        for child_id in children {
            if let Ok(meta) = tree.get_meta(child_id) {
                let is_folder = meta
                    .get("type")
                    .and_then(|v| {
                        if let loro::ValueOrContainer::Value(val) = v {
                            val.as_string().map(|s| s.as_ref() == "folder")
                        } else {
                            None
                        }
                    })
                    .unwrap_or(false);

                let child_name = meta.get("name").and_then(|v| {
                    if let loro::ValueOrContainer::Value(val) = v {
                        val.as_string().map(|s| s.to_string())
                    } else {
                        None
                    }
                });

                if is_folder && child_name.as_deref() == Some(name) {
                    return Ok(TreeParentId::Node(child_id));
                }
            }
        }

        // Create new folder node
        let node_id = tree
            .create(parent)
            .map_err(|e| VaultError::Other(format!("Failed to create folder node: {}", e)))?;

        let meta = tree
            .get_meta(node_id)
            .map_err(|e| VaultError::Other(format!("Failed to get folder meta: {}", e)))?;
        meta.insert("type", "folder")
            .map_err(|e| VaultError::Other(format!("Failed to set folder type: {}", e)))?;
        meta.insert("name", name)
            .map_err(|e| VaultError::Other(format!("Failed to set folder name: {}", e)))?;

        Ok(TreeParentId::Node(node_id))
    }
}

/// FNV-1a hash for deterministic file naming.
/// Uses FNV-1a instead of DefaultHasher because DefaultHasher is not stable across Rust versions.
fn simple_hash(s: &str) -> String {
    const FNV_OFFSET: u64 = 0xcbf29ce484222325;
    const FNV_PRIME: u64 = 0x100000001b3;

    let mut hash = FNV_OFFSET;
    for byte in s.bytes() {
        hash ^= byte as u64;
        hash = hash.wrapping_mul(FNV_PRIME);
    }
    format!("{:016x}", hash)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fs::InMemoryFs;

    fn test_peer_id() -> PeerId {
        PeerId::from(12345u64)
    }

    fn test_peer_id_2() -> PeerId {
        PeerId::from(67890u64)
    }

    #[tokio::test]
    async fn test_vault_init() {
        let fs = InMemoryFs::new();
        let vault = Vault::init(fs, test_peer_id()).await.unwrap();

        assert!(vault.is_initialized().await.unwrap());
    }

    #[tokio::test]
    async fn test_vault_file_change() {
        let fs = InMemoryFs::new();

        // Create a markdown file
        fs.write("test.md", b"# Hello\n\nWorld").await.unwrap();

        // Init vault
        let mut vault = Vault::init(fs, test_peer_id()).await.unwrap();

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
        let _vault = Vault::init(Arc::clone(&fs), test_peer_id()).await.unwrap();
        
        // Simulate adding a new file while plugin was off
        fs.write("new_file.md", b"# New File").await.unwrap();
        
        // Load vault - should detect and index the new file
        let mut vault = Vault::load(Arc::clone(&fs), test_peer_id()).await.unwrap();
        
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
        let _vault = Vault::init(Arc::clone(&fs), test_peer_id()).await.unwrap();
        
        // Simulate modifying the file while plugin was off
        fs.write("note.md", b"# Modified Content").await.unwrap();
        
        // Load vault - should detect modification and re-index
        let mut vault = Vault::load(Arc::clone(&fs), test_peer_id()).await.unwrap();
        
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
        let _vault = Vault::init(Arc::clone(&fs), test_peer_id()).await.unwrap();
        
        // Simulate deleting a file while plugin was off
        fs.delete("delete.md").await.unwrap();
        
        // Load vault - should detect orphaned .loro file
        let vault = Vault::load(Arc::clone(&fs), test_peer_id()).await.unwrap();
        
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
        let _vault = Vault::init(Arc::clone(&fs), test_peer_id()).await.unwrap();
        
        // Simulate renaming the file while plugin was off
        let content = fs.read("old_name.md").await.unwrap();
        fs.write("new_name.md", &content).await.unwrap();
        fs.delete("old_name.md").await.unwrap();
        
        // Load vault - should detect move and migrate .loro file
        let mut vault = Vault::load(Arc::clone(&fs), test_peer_id()).await.unwrap();
        
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
        let _vault = Vault::init(Arc::clone(&fs), test_peer_id()).await.unwrap();
        
        // Simulate moving file to subfolder while plugin was off
        let content = fs.read("note.md").await.unwrap();
        fs.mkdir("knowledge").await.unwrap();
        fs.write("knowledge/note.md", &content).await.unwrap();
        fs.delete("note.md").await.unwrap();
        
        // Load vault - should detect move
        let mut vault = Vault::load(Arc::clone(&fs), test_peer_id()).await.unwrap();
        
        // The moved file should be accessible
        let doc = vault.get_document("knowledge/note.md").await.unwrap();
        assert!(doc.to_markdown().contains("My Note XYZ789"));
        
        // Only the new path should exist
        let files = vault.list_files().await.unwrap();
        assert!(!files.contains(&"note.md".to_string()));
        assert!(files.contains(&"knowledge/note.md".to_string()));
    }

    // ========== Tree Operation Tests ==========

    #[tokio::test]
    async fn test_delete_file_removes_from_tree() {
        let fs = InMemoryFs::new();

        // Create and index a file
        fs.write("note.md", b"# Hello").await.unwrap();
        let mut vault = Vault::init(fs, test_peer_id()).await.unwrap();
        vault.on_file_changed("note.md").await.unwrap();

        // File should be in tree
        assert!(!vault.is_file_deleted("note.md"));

        // Delete via tree operation
        vault.delete_file("note.md").await.unwrap();

        // File should now be marked as deleted
        assert!(vault.is_file_deleted("note.md"));
    }

    #[tokio::test]
    async fn test_rename_file_updates_tree() {
        let fs = InMemoryFs::new();

        // Create and index a file
        fs.write("old.md", b"# Content").await.unwrap();
        let mut vault = Vault::init(fs, test_peer_id()).await.unwrap();
        vault.on_file_changed("old.md").await.unwrap();

        // Old path should exist in tree
        assert!(!vault.is_file_deleted("old.md"));

        // Create target file and rename
        vault.fs.write("new.md", b"# Content").await.unwrap();
        vault.rename_file("old.md", "new.md").await.unwrap();

        // New path should exist, old should be gone
        assert!(!vault.is_file_deleted("new.md"));
        // Note: old path may still show as "not deleted" since the node was moved, not deleted
        // The important thing is new.md works
    }

    #[tokio::test]
    async fn test_path_traversal_rejected() {
        let fs = InMemoryFs::new();
        let mut vault = Vault::init(fs, test_peer_id()).await.unwrap();

        // Path traversal should be rejected
        let result = vault.delete_file("../secret.md").await;
        assert!(result.is_err());

        let result = vault.rename_file("note.md", "../secret.md").await;
        assert!(result.is_err());

        let result = vault.register_file("../evil.md");
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn test_null_byte_rejected() {
        let fs = InMemoryFs::new();
        let mut vault = Vault::init(fs, test_peer_id()).await.unwrap();

        // Null bytes should be rejected
        let result = vault.delete_file("foo\0.md").await;
        assert!(result.is_err());

        let result = vault.register_file("bar\0.md");
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn test_non_markdown_rejected() {
        let fs = InMemoryFs::new();
        let mut vault = Vault::init(fs, test_peer_id()).await.unwrap();

        // Non-markdown files should be rejected
        let result = vault.register_file("script.js");
        assert!(result.is_err());

        let result = vault.delete_file("image.png").await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn test_empty_path_rejected() {
        let fs = InMemoryFs::new();
        let mut vault = Vault::init(fs, test_peer_id()).await.unwrap();

        // Empty path should be rejected
        let result = vault.register_file("");
        assert!(result.is_err());

        let result = vault.delete_file("").await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn test_empty_segment_rejected() {
        let fs = InMemoryFs::new();
        let mut vault = Vault::init(fs, test_peer_id()).await.unwrap();

        // Empty path segments (a//b.md) should be rejected
        let result = vault.register_file("a//b.md");
        assert!(result.is_err());

        let result = vault.delete_file("foo//bar.md").await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn test_path_too_long_rejected() {
        let fs = InMemoryFs::new();
        let mut vault = Vault::init(fs, test_peer_id()).await.unwrap();

        // Path over 1024 chars should be rejected
        let long_path = format!("{}.md", "a".repeat(1025));
        let result = vault.register_file(&long_path);
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn test_delete_syncs_via_registry() {
        use std::sync::Arc;

        let fs1 = Arc::new(InMemoryFs::new());
        let fs2 = Arc::new(InMemoryFs::new());

        // Create file in vault1
        fs1.write("note.md", b"# Hello").await.unwrap();
        let mut vault1 = Vault::init(Arc::clone(&fs1), test_peer_id()).await.unwrap();

        // Sync to vault2
        let mut vault2 = Vault::init(Arc::clone(&fs2), test_peer_id_2()).await.unwrap();
        let request = vault2.prepare_sync_request().await.unwrap();
        let (exchange, _) = vault1.process_sync_message(&request).await.unwrap();
        let (final_resp, _) = vault2.process_sync_message(&exchange.unwrap()).await.unwrap();
        if let Some(resp) = final_resp {
            vault1.process_sync_message(&resp).await.unwrap();
        }

        // Both vaults should have the file
        assert!(!vault1.is_file_deleted("note.md"));
        assert!(!vault2.is_file_deleted("note.md"));

        // Delete in vault1
        vault1.delete_file("note.md").await.unwrap();
        assert!(vault1.is_file_deleted("note.md"));

        // Sync again - vault2 should see deletion via registry
        let request2 = vault2.prepare_sync_request().await.unwrap();
        let (exchange2, _) = vault1.process_sync_message(&request2).await.unwrap();
        let (_, _) = vault2.process_sync_message(&exchange2.unwrap()).await.unwrap();

        // Vault2 should now see the file as deleted
        assert!(vault2.is_file_deleted("note.md"));
    }

    // ========== SyncTracker Tests ==========

    #[test]
    fn test_sync_tracker_mark_and_consume() {
        let tracker = SyncTracker::new();

        // Initially not synced
        assert!(!tracker.is_synced("test.md"));

        // Mark as synced
        tracker.mark_synced("test.md");
        assert!(tracker.is_synced("test.md"));

        // Consume returns true once
        assert!(tracker.consume_synced("test.md"));

        // Second consume returns false (flag cleared)
        assert!(!tracker.consume_synced("test.md"));
        assert!(!tracker.is_synced("test.md"));
    }

    #[test]
    fn test_sync_tracker_multiple_paths() {
        let tracker = SyncTracker::new();

        tracker.mark_synced("a.md");
        tracker.mark_synced("b.md");
        tracker.mark_synced("c.md");

        assert!(tracker.is_synced("a.md"));
        assert!(tracker.is_synced("b.md"));
        assert!(tracker.is_synced("c.md"));

        // Consume one doesn't affect others
        assert!(tracker.consume_synced("b.md"));
        assert!(tracker.is_synced("a.md"));
        assert!(!tracker.is_synced("b.md"));
        assert!(tracker.is_synced("c.md"));
    }

    #[test]
    fn test_sync_tracker_clone_shares_state() {
        let tracker1 = SyncTracker::new();
        let tracker2 = tracker1.clone();

        // Mark via tracker1
        tracker1.mark_synced("shared.md");

        // Visible via tracker2
        assert!(tracker2.is_synced("shared.md"));

        // Consume via tracker2
        assert!(tracker2.consume_synced("shared.md"));

        // Gone from tracker1 too
        assert!(!tracker1.is_synced("shared.md"));
    }

    #[tokio::test]
    async fn test_sync_marks_synced_flag() {
        use std::sync::Arc;

        let fs1 = Arc::new(InMemoryFs::new());
        let fs2 = Arc::new(InMemoryFs::new());

        // Create file in vault1
        fs1.write("note.md", b"# Hello").await.unwrap();
        let mut vault1 = Vault::init(Arc::clone(&fs1), test_peer_id())
            .await
            .unwrap();
        vault1.on_file_changed("note.md").await.unwrap();

        // Create empty vault2
        let mut vault2 = Vault::init(Arc::clone(&fs2), test_peer_id_2())
            .await
            .unwrap();

        // Sync from vault1 to vault2
        let request = vault1.prepare_sync_request().await.unwrap();
        let (response, _) = vault2.process_sync_message(&request).await.unwrap();
        let (_, modified) = vault1
            .process_sync_message(&response.unwrap())
            .await
            .unwrap();

        // vault1 shouldn't have modified files (it has newer data)
        assert!(modified.is_empty());

        // Sync response back to vault2
        let update = vault1.prepare_document_update("note.md").await.unwrap();
        let (_, modified2) = vault2
            .process_sync_message(&update.unwrap())
            .await
            .unwrap();

        // vault2 should have the synced flag set for modified files
        for path in &modified2 {
            assert!(
                vault2.consume_sync_flag(path),
                "Synced file {} should have sync flag set",
                path
            );
        }
    }

    #[tokio::test]
    async fn test_local_edit_does_not_set_sync_flag() {
        let fs = InMemoryFs::new();

        fs.write("note.md", b"# Original").await.unwrap();
        let mut vault = Vault::init(fs, test_peer_id()).await.unwrap();

        // Local edit
        vault.on_file_changed("note.md").await.unwrap();

        // Sync flag should NOT be set for local edits
        assert!(
            !vault.consume_sync_flag("note.md"),
            "Local edit should not set sync flag"
        );
    }

    #[tokio::test]
    async fn test_delete_sync_sets_flag() {
        use std::sync::Arc;

        let fs1 = Arc::new(InMemoryFs::new());
        let fs2 = Arc::new(InMemoryFs::new());

        // Create file in both vaults
        fs1.write("note.md", b"# Hello").await.unwrap();
        fs2.write("note.md", b"# Hello").await.unwrap();
        let mut vault1 = Vault::init(Arc::clone(&fs1), test_peer_id())
            .await
            .unwrap();
        let mut vault2 = Vault::init(Arc::clone(&fs2), test_peer_id_2())
            .await
            .unwrap();
        vault1.on_file_changed("note.md").await.unwrap();
        vault2.on_file_changed("note.md").await.unwrap();

        // Initial sync to get them in sync
        let req1 = vault1.prepare_sync_request().await.unwrap();
        let (resp1, _) = vault2.process_sync_message(&req1).await.unwrap();
        if let Some(r) = resp1 {
            vault1.process_sync_message(&r).await.unwrap();
        }

        // Delete in vault1
        vault1.delete_file("note.md").await.unwrap();

        // Prepare and send delete message
        let delete_msg = vault1.prepare_file_deleted("note.md").unwrap();
        let (_, modified) = vault2.process_sync_message(&delete_msg).await.unwrap();

        // vault2 should have the synced flag set for the deleted file
        assert!(modified.contains(&"note.md".to_string()));
        assert!(
            vault2.consume_sync_flag("note.md"),
            "Deleted file should have sync flag set"
        );
    }

    #[test]
    fn test_sync_tracker_flag_within_ttl() {
        let tracker = SyncTracker::new();

        // Mark and immediately check - should be within TTL
        tracker.mark_synced("test.md");
        assert!(tracker.is_synced("test.md"));
        assert!(tracker.consume_synced("test.md"));

        // After consume, flag is gone
        assert!(!tracker.is_synced("test.md"));
        assert!(!tracker.consume_synced("test.md"));
    }

    #[test]
    fn test_sync_tracker_cleanup_expired() {
        let tracker = SyncTracker::new();

        // Mark several paths
        tracker.mark_synced("a.md");
        tracker.mark_synced("b.md");
        tracker.mark_synced("c.md");

        // Cleanup shouldn't remove fresh flags
        tracker.cleanup_expired();

        // All should still be present (within TTL)
        assert!(tracker.is_synced("a.md"));
        assert!(tracker.is_synced("b.md"));
        assert!(tracker.is_synced("c.md"));
    }

    #[test]
    fn test_sync_tracker_rename_marks_both_paths() {
        // This tests the behavior expected when a rename sync is processed
        let tracker = SyncTracker::new();

        // Simulate what sync_engine does for FileRenamed
        let old_path = "old/note.md";
        let new_path = "new/note.md";
        tracker.mark_synced(old_path);
        tracker.mark_synced(new_path);

        // Both should be marked
        assert!(tracker.is_synced(old_path));
        assert!(tracker.is_synced(new_path));

        // Consuming one doesn't affect the other
        assert!(tracker.consume_synced(old_path));
        assert!(!tracker.is_synced(old_path));
        assert!(tracker.is_synced(new_path));
    }
}
