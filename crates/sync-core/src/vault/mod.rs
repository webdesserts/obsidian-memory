//! Vault: Manages a collection of NoteDocuments and syncs with peers.

mod reconcile;
mod registry_tree;
mod state;

pub use state::*;

use crate::document::NoteDocument;
use crate::events::{EventBus, Subscription, SyncEvent};
use crate::fs::FileSystem;
use crate::{PeerId, VaultId};

use loro::{LoroDoc, TreeID, VersionVector};
use std::collections::HashMap;

// The interior-mutability wrappers for the Vault struct fork by target: native uses
// Arc/Mutex (multi-threaded Tokio), wasm uses Rc/RefCell (single-threaded browser).
// SyncState owns its own unconditional Arc<Mutex<…>> over in state.rs.
#[cfg(not(target_arch = "wasm32"))]
use std::sync::{Arc, Mutex};
#[cfg(target_arch = "wasm32")]
use std::cell::RefCell;
#[cfg(target_arch = "wasm32")]
use std::rc::Rc;

// ========== Debug API Types ==========

/// Convert VersionVector to HashMap<String, i32> for JSON serialization.
///
/// Keys are the 16-character hex of each Loro peer id — the u64 FNV-1a hash of
/// a device author's `PeerId`, not a VaultId. One entry per device that has
/// authored operations.
fn version_vector_to_map(vv: &VersionVector) -> HashMap<String, i32> {
    let mut map = HashMap::new();
    for (peer_id, counter) in vv.iter() {
        map.insert(format!("{:016x}", peer_id), *counter);
    }
    map
}

/// Directory for sync state
pub(crate) const SYNC_DIR: &str = ".sync";
/// File registry document
const REGISTRY_FILE: &str = ".sync/registry.loro";
/// Directory where reconcile quarantines disk orphans (tombstoned `.md` files
/// still on disk). A dot-directory so `list_files` and the watcher already exclude
/// it — see `quarantine_orphan` for why that exclusion is load-bearing.
///
/// `pub(crate)` so the reconcile impl (a sibling module after the vault split) can
/// reach it.
pub(crate) const TRASH_DIR: &str = ".trash";

// Loro container and field names for the registry tree — changing these breaks existing
// .loro files. Format stability tests will trip if these change.
//
// The TREE_META_* consts are `pub(crate)` so the registry-tree impl (a sibling module
// after the vault split) can reach them.

/// Loro tree container name in the registry document.
pub(crate) const REGISTRY_TREE: &str = "files";
/// Tree node meta field: node type (always "file").
pub(crate) const TREE_META_TYPE: &str = "type";
/// Tree node meta field: file name (last path segment).
pub(crate) const TREE_META_NAME: &str = "name";
/// Tree node meta field: deterministic hash for document identity.
pub(crate) const TREE_META_DOC_ID: &str = "doc_id";
/// Tree node meta field: full path of the file (e.g. `dir/note.md`).
///
/// Written at registration so a node's path survives deletion: Loro does not
/// expose a deleted node's real parent (`tree.parent` returns `Deleted`), so the
/// path stored here is the only way to recover which path a deleted node held —
/// the basis for the registry-truth resurrection guard.
pub(crate) const TREE_META_PATH: &str = "path";

/// Manages a vault of documents.
///
/// Uses interior mutability for core state to allow `&self` methods in WASM.
/// This prevents RefCell borrow conflicts when JavaScript interleaves calls during
/// async operations (WASM async methods hold their borrows across await points).
pub struct Vault<F: FileSystem> {
    /// File registry (tracks all files in vault via LoroTree)
    /// WASM: RefCell for single-threaded browser
    /// Native: Mutex for multi-threaded Tokio
    #[cfg(target_arch = "wasm32")]
    pub(crate) registry: RefCell<LoroDoc>,
    #[cfg(not(target_arch = "wasm32"))]
    pub(crate) registry: Mutex<LoroDoc>,

    /// Path lookup cache (LoroTree has no path-based lookup)
    /// Rebuilt after sync and updated inline for local operations
    #[cfg(target_arch = "wasm32")]
    path_to_node: RefCell<HashMap<String, TreeID>>,
    #[cfg(not(target_arch = "wasm32"))]
    path_to_node: Mutex<HashMap<String, TreeID>>,

    /// Loaded documents
    #[cfg(target_arch = "wasm32")]
    pub(crate) documents: RefCell<HashMap<String, NoteDocument>>,
    #[cfg(not(target_arch = "wasm32"))]
    pub(crate) documents: Mutex<HashMap<String, NoteDocument>>,

    /// Filesystem abstraction
    pub(crate) fs: F,

    /// Vault identity used for the gossip topic seed and mDNS mesh grouping.
    ///
    /// Shared across every replica of this vault (persisted in
    /// `.sync/metadata.toml`). It is NOT the Loro author — see `loro_author`.
    vault_id: VaultId,

    /// The device's Loro peer id, authored on every Loro operation this replica
    /// produces. Unlike `vault_id`, it is unique per device so concurrent offline
    /// edits across devices don't collide on OpIds (see [[Loro Peer ID Semantics]]).
    ///
    /// `pub(crate)` so the `sync_engine` module can author merge/temp docs under it.
    pub(crate) loro_author: PeerId,

    /// Tracks sync state for echo detection and consistency reconciliation.
    ///
    /// `pub(crate)` so the reconcile and registry-tree impls (sibling modules after
    /// the vault split) can reach the `take_*` / `replace_deleted_paths` methods that
    /// bypass the wrapper accessors.
    pub(crate) sync_state: SyncState,

    /// Event bus for sync events (native: Arc for multi-threaded Tokio)
    #[cfg(not(target_arch = "wasm32"))]
    events: Arc<EventBus>,

    /// Event bus for sync events (WASM: Rc for single-threaded browser)
    #[cfg(target_arch = "wasm32")]
    events: Rc<EventBus>,
}

impl<F: FileSystem> Vault<F> {
    // ========== Interior Mutability Accessors ==========
    //
    // These methods provide access to fields wrapped in RefCell (WASM) or Mutex (native).
    // IMPORTANT: Never hold a borrow guard across an await point!
    // Clippy lint `await_holding_refcell_ref` will catch violations.

    /// Borrow the registry for reading (WASM)
    #[cfg(target_arch = "wasm32")]
    pub(crate) fn registry(&self) -> std::cell::Ref<'_, LoroDoc> {
        self.registry.borrow()
    }

    /// Borrow the registry for reading (native)
    #[cfg(not(target_arch = "wasm32"))]
    pub(crate) fn registry(&self) -> std::sync::MutexGuard<'_, LoroDoc> {
        self.registry.lock().unwrap()
    }

    /// Borrow the registry for mutation (WASM)
    #[cfg(target_arch = "wasm32")]
    pub(crate) fn registry_mut(&self) -> std::cell::RefMut<'_, LoroDoc> {
        self.registry.borrow_mut()
    }

    /// Borrow the registry for mutation (native)
    #[cfg(not(target_arch = "wasm32"))]
    pub(crate) fn registry_mut(&self) -> std::sync::MutexGuard<'_, LoroDoc> {
        self.registry.lock().unwrap()
    }

    /// Borrow path_to_node for reading (WASM)
    #[cfg(target_arch = "wasm32")]
    pub(crate) fn path_to_node(&self) -> std::cell::Ref<'_, HashMap<String, TreeID>> {
        self.path_to_node.borrow()
    }

    /// Borrow path_to_node for reading (native)
    #[cfg(not(target_arch = "wasm32"))]
    pub(crate) fn path_to_node(&self) -> std::sync::MutexGuard<'_, HashMap<String, TreeID>> {
        self.path_to_node.lock().unwrap()
    }

    /// Borrow path_to_node for mutation (WASM)
    #[cfg(target_arch = "wasm32")]
    pub(crate) fn path_to_node_mut(&self) -> std::cell::RefMut<'_, HashMap<String, TreeID>> {
        self.path_to_node.borrow_mut()
    }

    /// Borrow path_to_node for mutation (native)
    #[cfg(not(target_arch = "wasm32"))]
    pub(crate) fn path_to_node_mut(&self) -> std::sync::MutexGuard<'_, HashMap<String, TreeID>> {
        self.path_to_node.lock().unwrap()
    }

    /// Borrow documents for reading (WASM)
    #[cfg(target_arch = "wasm32")]
    pub(crate) fn documents(&self) -> std::cell::Ref<'_, HashMap<String, NoteDocument>> {
        self.documents.borrow()
    }

    /// Borrow documents for reading (native)
    #[cfg(not(target_arch = "wasm32"))]
    pub(crate) fn documents(&self) -> std::sync::MutexGuard<'_, HashMap<String, NoteDocument>> {
        self.documents.lock().unwrap()
    }

    /// Borrow documents for mutation (WASM)
    #[cfg(target_arch = "wasm32")]
    pub(crate) fn documents_mut(&self) -> std::cell::RefMut<'_, HashMap<String, NoteDocument>> {
        self.documents.borrow_mut()
    }

    /// Borrow documents for mutation (native)
    #[cfg(not(target_arch = "wasm32"))]
    pub(crate) fn documents_mut(&self) -> std::sync::MutexGuard<'_, HashMap<String, NoteDocument>> {
        self.documents.lock().unwrap()
    }

    /// Initialize a new vault (creates .sync directory and generates VaultId).
    ///
    /// `author` is the device's Loro peer id — every operation this replica
    /// produces is authored under it (see [[Loro Peer ID Semantics]]).
    pub async fn init(fs: F, author: PeerId) -> Result<Self> {
        // Create .sync directory
        fs.mkdir(SYNC_DIR).await?;
        fs.mkdir(&format!("{}/documents", SYNC_DIR)).await?;

        // Generate and persist vault metadata
        let metadata = SyncMetadata::load_or_migrate(&fs).await?;
        let vault_id = metadata.vault_id;

        let registry = LoroDoc::new();
        // Author the registry under the device peer id, not the shared VaultId
        registry.set_peer_id(author.as_u64()).ok();
        // Initialize the file tree (LoroTree inside registry)
        // The tree is created on first access via get_tree()
        let _file_tree = registry.get_tree(REGISTRY_TREE);

        // Save initial registry
        let registry_bytes = registry.export(loro::ExportMode::Snapshot).map_err(|e| {
            VaultError::RegistryExport(format!("Failed to export initial registry: {}", e))
        })?;
        fs.atomic_write(REGISTRY_FILE, &registry_bytes).await?;

        #[cfg(not(target_arch = "wasm32"))]
        let events = Arc::new(EventBus::new());
        #[cfg(target_arch = "wasm32")]
        let events = Rc::new(EventBus::new());

        // Wrap fields in interior mutability containers
        #[cfg(target_arch = "wasm32")]
        let vault = Self {
            registry: RefCell::new(registry),
            path_to_node: RefCell::new(HashMap::new()),
            documents: RefCell::new(HashMap::new()),
            fs,
            vault_id,
            loro_author: author,
            sync_state: SyncState::new(),
            events,
        };
        #[cfg(not(target_arch = "wasm32"))]
        let vault = Self {
            registry: Mutex::new(registry),
            path_to_node: Mutex::new(HashMap::new()),
            documents: Mutex::new(HashMap::new()),
            fs,
            vault_id,
            loro_author: author,
            sync_state: SyncState::new(),
            events,
        };

        // Scan and index all existing markdown files
        vault.index_existing_files().await?;

        Ok(vault)
    }

    /// Load an existing vault and reconcile with filesystem.
    ///
    /// Reads VaultId from `.sync/metadata.toml` (migrating from v0 if needed).
    /// Reconciliation ensures the Loro state matches the filesystem:
    /// - New files (no .loro) → index them
    /// - Modified files (markdown ≠ Loro) → re-index from markdown
    /// - Orphaned .loro files → logged for future cleanup
    ///
    /// `author` is the device's Loro peer id — every operation this replica
    /// produces is authored under it (see [[Loro Peer ID Semantics]]).
    pub async fn load(fs: F, author: PeerId) -> Result<Self> {
        // Check if vault is initialized
        if !fs.exists(SYNC_DIR).await? {
            return Err(VaultError::NotInitialized);
        }

        // Load or migrate vault metadata
        let metadata = SyncMetadata::load_or_migrate(&fs).await?;
        let vault_id = metadata.vault_id;

        // Load registry
        let registry = if fs.exists(REGISTRY_FILE).await? {
            let bytes = fs.read(REGISTRY_FILE).await?;
            let doc = LoroDoc::new();
            // Author new registry ops under the device peer id before import
            doc.set_peer_id(author.as_u64()).ok();
            // HARD-FAIL on a corrupt registry rather than swallowing the error.
            // Falling back to an empty registry would re-index every file with fresh
            // doc_ids → mass divergence from peers → "latest wins" content clobber.
            // The desktop and daemon startup paths both surface this Err as a clean
            // logged exit, which is the safe outcome. (See audit / hardening Item 3.)
            doc.import(&bytes)
                .map_err(|e| VaultError::CorruptRegistry(e.to_string()))?;
            doc
        } else {
            let doc = LoroDoc::new();
            doc.set_peer_id(author.as_u64()).ok();
            // Initialize file tree for new registries
            let _file_tree = doc.get_tree(REGISTRY_TREE);
            doc
        };

        #[cfg(not(target_arch = "wasm32"))]
        let events = Arc::new(EventBus::new());
        #[cfg(target_arch = "wasm32")]
        let events = Rc::new(EventBus::new());

        // Wrap fields in interior mutability containers
        #[cfg(target_arch = "wasm32")]
        let vault = Self {
            registry: RefCell::new(registry),
            path_to_node: RefCell::new(HashMap::new()),
            documents: RefCell::new(HashMap::new()),
            fs,
            vault_id,
            loro_author: author,
            sync_state: SyncState::new(),
            events,
        };
        #[cfg(not(target_arch = "wasm32"))]
        let vault = Self {
            registry: Mutex::new(registry),
            path_to_node: Mutex::new(HashMap::new()),
            documents: Mutex::new(HashMap::new()),
            fs,
            vault_id,
            loro_author: author,
            sync_state: SyncState::new(),
            events,
        };

        // Build path cache from loaded tree
        vault.rebuild_path_cache();

        // Reconcile filesystem with Loro state
        vault.reconcile().await?;

        Ok(vault)
    }

    /// Get the vault's identity (used as gossip topic seed and mDNS mesh grouping key).
    pub fn vault_id(&self) -> VaultId {
        self.vault_id
    }

    /// Adopt a different VaultId, rewriting `.sync/metadata.toml` to match.
    ///
    /// Used when a pairing initiator joins an existing mesh: it abandons its own
    /// freshly-generated VaultId and takes on the mesh's so it lands on the same
    /// gossip topic / mDNS group. Safe because the VaultId is purely the
    /// replica-grouping id — the Loro author is the per-device `loro_author`, which
    /// is untouched here (see [[Loro Peer ID Semantics]]).
    ///
    /// Idempotent: a no-op when `new_id` already matches the current id. The format
    /// `version` is preserved by re-reading the existing metadata before rewriting.
    pub async fn adopt_vault_id(&mut self, new_id: VaultId) -> Result<()> {
        if new_id == self.vault_id {
            return Ok(());
        }

        // Preserve the on-disk format version — only the vault_id changes.
        let existing = SyncMetadata::load_or_migrate(&self.fs).await?;
        let meta = SyncMetadata {
            version: existing.version,
            vault_id: new_id,
        };
        meta.save(&self.fs).await?;

        self.vault_id = new_id;
        tracing::info!(vault_id = %new_id, "Adopted mesh VaultId");
        Ok(())
    }

    /// Get this device's Loro author identity.
    ///
    /// This is the per-device `PeerId` every Loro operation is authored under —
    /// distinct from `vault_id`, which groups replicas for gossip/mDNS.
    pub fn loro_author(&self) -> PeerId {
        self.loro_author
    }

    /// Subscribe to sync events. Returns `Subscription` that unsubscribes on drop.
    ///
    /// The callback receives `SyncEvent` objects for real-time monitoring.
    #[cfg(not(target_arch = "wasm32"))]
    pub fn subscribe(&self, callback: impl Fn(SyncEvent) + Send + Sync + 'static) -> Subscription {
        self.events.subscribe(callback)
    }

    /// Subscribe to sync events. Returns `Subscription` that unsubscribes on drop.
    ///
    /// The callback receives `SyncEvent` objects for real-time monitoring.
    #[cfg(target_arch = "wasm32")]
    pub fn subscribe(&self, callback: impl Fn(SyncEvent) + 'static) -> Subscription {
        self.events.subscribe(callback)
    }

    /// Emit a sync event to all subscribers.
    pub(crate) fn emit(&self, event: SyncEvent) {
        self.events.emit(event);
    }

    /// Get current timestamp in milliseconds.
    pub(crate) fn now_ms(&self) -> f64 {
        web_time::SystemTime::now()
            .duration_since(web_time::SystemTime::UNIX_EPOCH)
            .map(|d| d.as_secs_f64() * 1000.0)
            .unwrap_or(0.0)
    }

    /// Mark a path as synced (call before writing to disk).
    /// Used to prevent re-broadcasting files we just received from sync.
    pub fn mark_synced(&self, path: &str) {
        self.sync_state.mark_synced(path);
    }

    /// Mark registry as synced (call after writing registry to disk).
    /// Used to trigger registry reconciliation before next sync import.
    pub(crate) fn mark_registry_synced(&self) {
        self.sync_state.mark_registry_synced();
    }

    /// Check if a path was synced and consume the flag.
    /// Returns true once (and clears the flag), false on subsequent calls.
    ///
    /// Note: the production read-side (daemon `on_file_modified`/`on_file_deleted`) no
    /// longer consults this flag — those handlers now gate on the apply primitives'
    /// return values (`on_file_changed` → bool, `delete_file` → bool). The arming side
    /// (`mark_synced`) still feeds `pending_reconcile` which is load-bearing for
    /// `ensure_consistency`. Reap the `synced_paths` echo machinery in a later cleanup
    /// once the concurrent sync_engine work lands.
    pub fn consume_sync_flag(&self, path: &str) -> bool {
        self.sync_state.consume_synced(path)
    }

    /// Mark a single path as deleted in the registry tree (synchronous local-delete guard).
    pub(crate) fn mark_path_deleted(&self, path: &str) {
        self.sync_state.mark_path_deleted(path);
    }

    /// Check whether the given path's registry tree node is currently deleted.
    pub(crate) fn is_path_deleted_in_registry(&self, path: &str) -> bool {
        self.sync_state.is_path_deleted_in_registry(path)
    }

    /// Get the version vector for a document as encoded bytes.
    ///
    /// Returns None if the document hasn't been loaded.
    /// Use this for tracking which version was synced to detect if a local
    /// modification contains only changes we just received from sync.
    pub async fn get_document_version(&self, path: &str) -> Result<Option<Vec<u8>>> {
        if !self.documents().contains_key(path) {
            // Try to load the document
            let sync_path = self.document_sync_path(path);
            if !self.fs.exists(&sync_path).await? {
                return Ok(None);
            }
            let doc = self.load_document(path).await?;
            self.documents_mut().insert(path.to_string(), doc);
        }

        Ok(self.documents().get(path).map(|doc| doc.version().encode()))
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

    /// Get or load a document.
    ///
    /// Returns a clone of the document. With interior mutability, we can't return
    /// references into the documents HashMap since the borrow guard would be dropped.
    /// NoteDocument clones are cheap (LoroDoc uses Arc internally).
    pub async fn get_document(&self, path: &str) -> Result<NoteDocument> {
        if !self.documents().contains_key(path) {
            let doc = self.load_document(path).await?;
            self.documents_mut().insert(path.to_string(), doc.clone());
            return Ok(doc);
        }
        Ok(self.documents().get(path).unwrap().clone())
    }

    /// Get a mutable reference to a cached document.
    ///
    /// This returns a clone that can be modified. After modification, call
    /// `update_document()` to persist changes back to the cache, then `save_document()`
    /// to write to disk.
    ///
    /// Unlike `get_document()`, this loads the document first if not cached.
    pub async fn get_document_mut(&self, path: &str) -> Result<NoteDocument> {
        if !self.documents().contains_key(path) {
            let doc = self.load_document(path).await?;
            self.documents_mut().insert(path.to_string(), doc.clone());
            return Ok(doc);
        }
        Ok(self.documents().get(path).unwrap().clone())
    }

    /// Update a document in the cache after modification.
    ///
    /// Call this after modifying a document obtained from `get_document_mut()`,
    /// then call `save_document()` to persist to disk.
    pub fn update_document(&self, path: &str, doc: NoteDocument) {
        self.documents_mut().insert(path.to_string(), doc);
    }

    /// Load a document from disk
    async fn load_document(&self, path: &str) -> Result<NoteDocument> {
        // Try to load from .sync first (for Loro state)
        let sync_path = self.document_sync_path(path);
        if self.fs.exists(&sync_path).await? {
            let bytes = self.fs.read(&sync_path).await?;
            // Use from_bytes to preserve peer ID (imports before setting metadata)
            return Ok(NoteDocument::from_bytes(path, &bytes, self.loro_author)?);
        }

        // Otherwise load from markdown file
        if self.fs.exists(path).await? {
            let bytes = self.fs.read(path).await?;
            let content = String::from_utf8_lossy(&bytes);
            return Ok(NoteDocument::from_markdown(
                path,
                &content,
                self.loro_author,
            )?);
        }

        // New document - use from_markdown with empty content to get a doc_id
        Ok(NoteDocument::from_markdown(path, "", self.loro_author)?)
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
    /// Returns `true` if the document body or frontmatter actually changed, `false` if the
    /// disk content was identical to the stored Loro state (a sync echo or redundant watcher
    /// event). Callers can gate broadcasts on this bool without consulting the sync flag.
    ///
    /// Note: stored Loro bodies are outputs of a prior `markdown::parse`, which strips leading
    /// newlines — that invariant is what makes the body round-trip echo-stable. Don't change
    /// parse's newline handling without revisiting this comparison.
    pub async fn on_file_changed(&self, path: &str) -> Result<bool> {
        // Skip non-markdown files and .sync directory
        if !path.ends_with(".md") || path.starts_with(SYNC_DIR) {
            return Ok(false);
        }

        // Load the current file content
        let bytes = self.fs.read(path).await?;
        let content = String::from_utf8_lossy(&bytes);
        let parsed = crate::markdown::parse(&content);
        let sync_path = self.document_sync_path(path);

        // If document is in cache, diff-and-merge
        if self.documents().contains_key(path) {
            let existing_doc = self.documents().get(path).unwrap().clone();
            let body_changed = existing_doc.update_body(&parsed.body)?;
            let fm_changed = existing_doc.update_frontmatter(parsed.frontmatter.as_ref())?;

            if body_changed || fm_changed {
                existing_doc.commit();
                let snapshot = existing_doc.export_snapshot()?;
                self.documents_mut().insert(path.to_string(), existing_doc);
                self.fs.atomic_write(&sync_path, &snapshot).await?;
                tracing::debug!("Updated document via diff: {}", path);
            } else {
                tracing::debug!("No changes detected (sync echo): {}", path);
            }
            return Ok(body_changed || fm_changed);
        }

        // Check if .loro exists on disk but not in cache (cold cache scenario)
        if self.fs.exists(&sync_path).await? {
            // Load from disk and diff-merge (preserves peer ID)
            let loro_bytes = self.fs.read(&sync_path).await?;
            let doc = NoteDocument::from_bytes(path, &loro_bytes, self.loro_author)?;

            let body_changed = doc.update_body(&parsed.body)?;
            let fm_changed = doc.update_frontmatter(parsed.frontmatter.as_ref())?;

            if body_changed || fm_changed {
                doc.commit();
                let snapshot = doc.export_snapshot()?;
                self.fs.atomic_write(&sync_path, &snapshot).await?;
                tracing::debug!("Updated cold-cache document via diff: {}", path);
            } else {
                tracing::debug!("No changes detected (cold cache sync echo): {}", path);
            }

            self.documents_mut().insert(path.to_string(), doc);
            return Ok(body_changed || fm_changed);
        }

        // Document doesn't exist anywhere - create new (this is the only time we need new peer ID)
        let new_doc = NoteDocument::from_markdown(path, &content, self.loro_author)?;
        let snapshot = new_doc.export_snapshot()?;
        self.fs.atomic_write(&sync_path, &snapshot).await?;
        self.documents_mut().insert(path.to_string(), new_doc);

        // Register in tree for delete/rename tracking
        self.register_file(path)?;

        tracing::debug!("Created new document: {}", path);

        Ok(true)
    }

    /// Save a document to disk (both markdown and sync state)
    pub async fn save_document(&self, path: &str) -> Result<()> {
        // Clone document out before async operations to avoid holding lock across await
        let doc = self.documents().get(path).cloned();
        if let Some(doc) = doc {
            // Save markdown
            let markdown = doc.to_markdown();
            self.fs.write(path, markdown.as_bytes()).await?;

            // Save sync state
            let sync_path = self.document_sync_path(path);
            let snapshot = doc.export_snapshot()?;
            self.fs.atomic_write(&sync_path, &snapshot).await?;
        }
        Ok(())
    }

    /// Persist the registry CRDT to `.sync/registry.loro`.
    ///
    /// Must be called after every local mutation of the registry tree
    /// (register, delete, rename). The caller is responsible for deciding
    /// when to flush — individual mutations call this once after each change;
    /// batch operations (e.g. `reconcile`) call it once after the full batch to
    /// avoid O(n) writes during startup with hundreds of files.
    ///
    /// `register_file` is sync (no wide ripple to callers), so it cannot call
    /// this itself. All async callers that invoke `register_file` are responsible
    /// for calling `save_registry()` after the mutation reaches a consistent state.
    pub async fn save_registry(&self) -> Result<()> {
        let bytes = self
            .registry()
            .export(loro::ExportMode::Snapshot)
            .map_err(|e| VaultError::RegistryExport(format!("Failed to export registry: {}", e)))?;
        self.fs.atomic_write(REGISTRY_FILE, &bytes).await?;
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
    async fn index_existing_files(&self) -> Result<()> {
        let files = self.list_files().await?;
        let mut any_registered = false;

        for path in files {
            // Process each file as if it was just changed
            // This creates the Loro document and saves the sync state
            let was_registered = !self.path_to_node().contains_key(&path);
            if let Err(e) = self.on_file_changed(&path).await {
                // Log but don't fail - some files might have issues
                tracing::warn!("Failed to index file {}: {}", path, e);
            } else if was_registered && self.path_to_node().contains_key(&path) {
                any_registered = true;
            }
        }

        // Batch: persist all registrations with one write instead of one per file.
        if any_registered {
            self.save_registry().await?;
        }

        Ok(())
    }

    /// Scan the registry for debris without mutating anything.
    ///
    /// Surfaces two classes of damage the cross-machine sync history leaves behind:
    /// **duplicate alive pairs** (more than one alive file node resolving to the same
    /// path — parallel-index debris) and **relics** (an alive file node whose `.md` and
    /// `.loro` are both gone — a tombstone that never landed). Duplicate FOLDER groups
    /// are listed separately but not classified for dedupe; folder handling is out of
    /// scope for v1 (recursive `LoroTree::delete` + child-split makes it a riskier,
    /// separate effort).
    ///
    /// For each duplicate group the report records the deterministic winner — the lowest
    /// `TreeID` by `(peer, counter)` — so the same survivor is chosen on every machine
    /// from a converged registry. This is read-only: the caller (an operator dry run)
    /// reviews the report before any tombstoning happens under a separate `--apply` pass.
    ///
    /// Async because the relic gate consults `fs.exists` for the backing `.md`/`.loro`.
    pub async fn find_registry_debris(&self) -> Result<DebrisReport> {
        // Collect alive-node data while holding the tree borrow, then release it before
        // the async fs probes below (never hold a registry guard across an await).
        // Each file node carries its TreeID + doc_id; the grouping path is the map key.
        let mut alive_files: HashMap<String, Vec<(TreeID, String)>> = HashMap::new();
        let mut alive_folders: HashMap<String, Vec<TreeID>> = HashMap::new();

        {
            let tree = self.file_tree();
            for node_id in tree.nodes() {
                // A deleted (tombstoned) node is not debris — only alive nodes are.
                if tree.is_node_deleted(&node_id).unwrap_or(true) {
                    continue;
                }
                let Ok(meta) = tree.get_meta(node_id) else {
                    continue;
                };
                let node_type = Self::tree_meta_string(&meta, TREE_META_TYPE);
                let Some(path) = self.get_node_path(&node_id) else {
                    continue;
                };

                match node_type.as_deref() {
                    Some("file") => {
                        let doc_id = Self::tree_meta_string(&meta, TREE_META_DOC_ID)
                            .unwrap_or_else(|| simple_hash(&path));
                        alive_files.entry(path).or_default().push((node_id, doc_id));
                    }
                    Some("folder") => {
                        alive_folders.entry(path).or_default().push(node_id);
                    }
                    _ => {}
                }
            }
        }

        let mut report = DebrisReport::default();

        // Folder duplicate groups: surfaced for visibility, never deduped in v1.
        for (path, nodes) in alive_folders {
            if nodes.len() > 1 {
                report.folder_dups.push(FolderDupGroup {
                    path,
                    alive_nodes: nodes,
                });
            }
        }

        for (path, nodes) in alive_files {
            if nodes.len() > 1 {
                // Duplicate alive pair: the winner is the lowest TreeID (Ord on
                // (peer, counter)), deterministic across machines.
                let alive_nodes: Vec<TreeID> = nodes.iter().map(|(id, _)| *id).collect();
                let winner = alive_nodes
                    .iter()
                    .copied()
                    .min()
                    .expect("group has at least two nodes");
                // All nodes in the group share the same doc_id (= path hash).
                let doc_id = nodes[0].1.clone();
                report.duplicate_groups.push(DuplicateGroup {
                    path,
                    doc_id,
                    alive_nodes,
                    winner,
                });
            } else {
                // A single alive node — a relic only if its .md AND .loro are both gone.
                // If either exists it is a live node (perhaps just not scanned); skip it.
                let (node_id, doc_id) = &nodes[0];
                let md_exists = self.fs.exists(&path).await.unwrap_or(false);
                let loro_path = self.document_sync_path(&path);
                let loro_exists = self.fs.exists(&loro_path).await.unwrap_or(false);
                if !md_exists && !loro_exists {
                    report.relics.push(Relic {
                        node: *node_id,
                        path,
                        doc_id: doc_id.clone(),
                    });
                }
            }
        }

        Ok(report)
    }

    /// Tombstone the registry debris a prior [`Vault::find_registry_debris`] scan found.
    ///
    /// For each duplicate group, keeps `winner` alive and `tree.delete`s every other node
    /// in `alive_nodes` (the losers); for each relic, `tree.delete`s the relic node. Folder
    /// dups are NOT touched (v1 scope-out — `LoroTree::delete` is recursive and production
    /// children are split across the two folder nodes, so a naive folder-tombstone is data
    /// loss; the report surfaces them for visibility only).
    ///
    /// Both twins in a duplicate group share the same `doc_id` (= path hash), so tombstoning
    /// the loser leaves the document content untouched — only one tree node remains alive.
    /// The winner is never destroyed.
    ///
    /// The registry is persisted exactly ONCE at the end, so the whole pass is a single
    /// `save_registry` write regardless of how many nodes it tombstones (at production scale
    /// this is hundreds of nodes — one transaction, not one write per group). When this saved
    /// registry later rides a `registry_updates` broadcast, the deleted-paths alive-guard in
    /// `apply_registry_updates` protects every receiver: a duplicate path the winner still
    /// occupies is never fs-cleaned, so peers keep their `.md`/`.loro` on disk.
    ///
    /// Relics are tombstoned unconditionally here — the report only classifies a node as a
    /// relic when both its `.md` and `.loro` are already absent, so there is nothing on disk
    /// to remove.
    ///
    /// Safe to re-run with a STALE (pre-dedupe) report: a node that is already tombstoned is
    /// skipped rather than re-deleted. This matters because `LoroTree::delete` on an
    /// already-deleted node returns `Err(TreeNodeDeletedOrNotExist)` — it does NOT no-op — so
    /// without the skip a second pass over the same report would error. When nothing is left
    /// to tombstone the pass returns early without writing the registry, so a converged
    /// re-run is a true no-op (no spurious registry write, no cache rebuild).
    pub async fn apply_dedupe(&self, report: &DebrisReport) -> Result<DedupeStats> {
        let mut stats = DedupeStats::default();

        {
            // Hold the tree borrow for all tombstone ops, then drop it before save_registry
            // (never hold a registry guard across the await below).
            let tree = self.file_tree();

            for group in &report.duplicate_groups {
                let mut tombstoned_in_group = 0;
                for &node_id in &group.alive_nodes {
                    // Keep the deterministic survivor; tombstone every other twin. The winner
                    // is the lowest TreeID, identical on every machine from a converged
                    // registry, so two operators running --apply independently converge.
                    if node_id == group.winner {
                        continue;
                    }
                    // Already tombstoned (stale report re-run, or a peer's tombstone arrived
                    // first): skip it. `tree.delete` errors on an already-deleted node, so this
                    // guard is what keeps a re-run a clean no-op instead of a hard error.
                    if tree.is_node_deleted(&node_id).unwrap_or(false) {
                        continue;
                    }
                    tree.delete(node_id).map_err(|e| {
                        VaultError::TreeOperation(format!(
                            "Failed to tombstone duplicate node at '{}': {}",
                            group.path, e
                        ))
                    })?;
                    tombstoned_in_group += 1;
                }
                if tombstoned_in_group > 0 {
                    stats.groups_deduped += 1;
                    stats.nodes_tombstoned += tombstoned_in_group;
                }
            }

            for relic in &report.relics {
                // Same already-tombstoned skip as the loser loop above.
                if tree.is_node_deleted(&relic.node).unwrap_or(false) {
                    continue;
                }
                tree.delete(relic.node).map_err(|e| {
                    VaultError::TreeOperation(format!(
                        "Failed to tombstone relic node at '{}': {}",
                        relic.path, e
                    ))
                })?;
                stats.relics_tombstoned += 1;
            }
        }

        // Nothing was tombstoned (clean vault, or a converged re-run that skipped every
        // already-deleted node): skip the registry write + cache rebuild so the pass is a
        // true no-op rather than a redundant snapshot write.
        if stats == DedupeStats::default() {
            return Ok(stats);
        }

        // Single transaction for the whole pass: persist all tombstones with one write.
        self.save_registry().await?;

        // The tombstoned losers/relics no longer resolve; rebuild the path cache so the
        // survivors are the only nodes the cache points at.
        self.rebuild_path_cache();

        Ok(stats)
    }

    // ========== Debug API Methods ==========
    //
    // These methods expose internal CRDT state for debugging and dashboard UIs.
    // - `get_registry_*` methods are cheap (in-memory registry state)
    // - `get_document_blob_meta` is cheap (reads .loro file header only)
    // - `get_document_info` is expensive (loads full document if not cached)

    /// Get the registry version vector as a map of peer ID hex strings to counters.
    pub fn get_registry_version(&self) -> HashMap<String, i32> {
        // Use oplog_vv() instead of state_vv() to show all received operations,
        // not just the operations that contributed to current state
        version_vector_to_map(&self.registry().oplog_vv())
    }

    /// Get registry oplog statistics.
    pub fn get_registry_stats(&self) -> RegistryStats {
        // Note: Must extract values before struct initialization to ensure
        // MutexGuards are dropped in predictable order
        let change_count = self.registry().len_changes();
        let op_count = self.registry().len_ops();
        RegistryStats {
            change_count,
            op_count,
        }
    }

    /// Get cheap metadata from the .loro blob header without loading the full document.
    ///
    /// Returns `None` if the document doesn't exist. Uses `decode_import_blob_meta()`
    /// which only parses the blob header, not the full document content.
    pub async fn get_document_blob_meta(&self, path: &str) -> Result<Option<BlobMeta>> {
        let sync_path = self.document_sync_path(path);
        if !self.fs.exists(&sync_path).await? {
            return Ok(None);
        }

        let bytes = self.fs.read(&sync_path).await?;
        let meta = LoroDoc::decode_import_blob_meta(&bytes, false)
            .map_err(|e| VaultError::BlobDecode(format!("Failed to decode blob meta: {}", e)))?;

        Ok(Some(BlobMeta {
            change_count: meta.change_num,
            start_timestamp: meta.start_timestamp,
            end_timestamp: meta.end_timestamp,
            mode: format!("{:?}", meta.mode),
            start_version: version_vector_to_map(&meta.partial_start_vv),
            end_version: version_vector_to_map(&meta.partial_end_vv),
        }))
    }

    /// Get full document info (requires loading the document).
    ///
    /// Returns `None` if the document doesn't exist. Loads the document if not
    /// already cached, which includes content metadata not available from blob headers.
    pub async fn get_document_info(&self, path: &str) -> Result<Option<DocumentInfo>> {
        let sync_path = self.document_sync_path(path);
        if !self.fs.exists(&sync_path).await? {
            return Ok(None);
        }

        let doc = self.get_document(path).await?;
        let fm = doc.frontmatter().get_deep_value();
        let has_frontmatter = matches!(fm, loro::LoroValue::Map(m) if !m.is_empty());

        Ok(Some(DocumentInfo {
            path: path.to_string(),
            version: version_vector_to_map(&doc.version()),
            doc_id: doc.doc_id(),
            stored_path: doc.stored_path(),
            change_count: doc.len_changes(),
            op_count: doc.len_ops(),
            body_length: doc.body().len_unicode(),
            has_frontmatter,
        }))
    }
}

/// FNV-1a hash for deterministic file naming.
/// Uses FNV-1a instead of DefaultHasher because DefaultHasher is not stable across Rust versions.
///
/// `pub(crate)` so the reconcile and registry-tree impls (sibling modules after the
/// vault split) can reach it for document-path hashing.
pub(crate) fn simple_hash(s: &str) -> String {
    const FNV_OFFSET: u64 = 0xcbf29ce484222325;
    const FNV_PRIME: u64 = 0x100000001b3;

    let mut hash = FNV_OFFSET;
    for byte in s.bytes() {
        hash ^= byte as u64;
        hash = hash.wrapping_mul(FNV_PRIME);
    }
    format!("{:016x}", hash)
}

/// Inline tests for behavior that can ONLY be driven by reaching `pub(crate)`
/// vault internals — there is no public entry point that exercises these effects
/// deterministically, so per the "test the user-facing effect through the public
/// API" principle they stay here rather than move to a top-level `tests/vault_*`
/// integration file (the public-API-drivable tests live there). Specifically:
///
/// - Forging multi-node registries (a duplicate-alive pair) requires
///   `registry()` / `registry_mut().import()` / `rebuild_path_cache()`;
///   `register_file` short-circuits on an existing alive node, so the pair can't
///   be built through the public API.
/// - The dedupe-apply and quarantine guards assert on `pub(crate)` registry state
///   (`file_tree()`, `is_path_deleted_in_registry()`, `path_to_node()`) or call
///   `quarantine_orphan()` directly to exercise a guard in isolation.
/// - The empty-stored-path orphan-warning test needs the private `simple_hash`
///   both to seed the legacy `.loro` and to assert the hash fallback in the
///   report.
#[cfg(test)]
mod tests {
    use super::*;
    use crate::fs::InMemoryFs;

    fn test_peer_id() -> PeerId {
        // Deterministic test PeerId using from_bytes
        let mut bytes = [0u8; 32];
        bytes[..8].copy_from_slice(&12345u64.to_be_bytes());
        PeerId::from_bytes(bytes)
    }

    fn test_peer_id_2() -> PeerId {
        let mut bytes = [0u8; 32];
        bytes[..8].copy_from_slice(&67890u64.to_be_bytes());
        PeerId::from_bytes(bytes)
    }

    #[tokio::test]
    async fn test_reconcile_orphan_warning_uses_hash_for_empty_path() {
        // A legacy orphan .loro persisted with META_PATH="" (written by the
        // pre-5fd4a63 orphan loader that passed "" to from_bytes) must be reported
        // under its hash, not an empty string. stored_path() returns Some("") for
        // such docs, so the unwrap_or fallbacks don't fire — a defensive
        // empty-string filter is what keeps the warning/report meaningful.
        //
        // Inline because the test needs the private `simple_hash` both to seed the
        // `.loro` at the right path and to assert the hash fallback in the report.
        use std::sync::Arc;

        let fs = Arc::new(InMemoryFs::new());

        fs.write("legacy.md", b"# Legacy").await.unwrap();
        let _vault = Vault::init(Arc::clone(&fs), test_peer_id()).await.unwrap();

        // Rewrite the file's .loro so its stored META_PATH is "" — exactly the state
        // a pre-fix orphan loader left on disk. from_bytes("", ...) overwrites
        // META_PATH with the empty string.
        let hash = simple_hash("legacy.md");
        let sync_path = format!("{}/documents/{}.loro", SYNC_DIR, hash);
        let bytes = fs.read(&sync_path).await.unwrap();
        let legacy = NoteDocument::from_bytes("", &bytes, test_peer_id()).unwrap();
        assert_eq!(
            legacy.stored_path().as_deref(),
            Some(""),
            "test setup must produce a doc with empty stored path"
        );
        fs.write(&sync_path, &legacy.export_snapshot().unwrap())
            .await
            .unwrap();

        // Delete the markdown so the .loro becomes an orphan on reconcile.
        fs.delete("legacy.md").await.unwrap();

        let vault = Vault::load(Arc::clone(&fs), test_peer_id()).await.unwrap();
        let report = vault.reconcile().await.unwrap();

        assert!(
            !report.orphaned.contains(&String::new()),
            "orphan report must not carry an empty path, got: {:?}",
            report.orphaned
        );
        assert!(
            report.orphaned.contains(&hash),
            "orphan report should fall back to the hash for an empty stored path, got: {:?}",
            report.orphaned
        );
    }

    // ========== Orphan Quarantine Tests (inline: pub(crate) registry state) ==========

    #[tokio::test]
    async fn test_reconcile_does_not_quarantine_meta_less_tombstone() {
        // A deleted node with no TREE_META_PATH never enters deleted_paths, so its
        // path is indistinguishable from an absent file. A same-named disk file must
        // therefore be treated as new (indexed), NOT quarantined — proving we never
        // over-delete when the tombstone carries no recoverable path.
        //
        // Inline because the precondition asserts on `is_path_deleted_in_registry`
        // (pub(crate)) — the load-bearing "the registry cannot prove this deleted"
        // state has no public proxy.
        use std::sync::Arc;
        let fs = Arc::new(InMemoryFs::new());

        let _vault = Vault::init(Arc::clone(&fs), test_peer_id()).await.unwrap();
        // A file whose path the registry has no tombstone for (the meta-less case
        // surfaces identically: is_path_deleted_in_registry is false).
        fs.write("ambiguous.md", b"# Ambiguous").await.unwrap();

        let vault = Vault::load(Arc::clone(&fs), test_peer_id()).await.unwrap();
        assert!(
            !vault.is_path_deleted_in_registry("ambiguous.md"),
            "precondition: a meta-less/absent path is not in deleted_paths"
        );

        let report = vault.reconcile().await.unwrap();
        assert!(
            !report.quarantined.contains(&"ambiguous.md".to_string()),
            "a path the registry cannot prove deleted must never be quarantined"
        );
        assert!(
            fs.exists("ambiguous.md").await.unwrap(),
            "the file must stay on disk"
        );
    }

    #[tokio::test]
    async fn test_reconcile_does_not_quarantine_duplicate_node_pair_with_alive() {
        // A path can carry BOTH a tombstoned node and a fresh alive node — the
        // cross-machine parallel-index debris seen in production. deleted_paths is
        // alive-wins (rebuild_path_cache removes any path occupied by an alive node),
        // so such a path is NOT in the cleaned set and the disk file is left in place,
        // not quarantined. Reconcile never mutates the tree, so it cannot worsen the
        // duplication — that is the dedupe tooling's job.
        //
        // Inline because the alive-wins precondition asserts on
        // `is_path_deleted_in_registry` (pub(crate)).
        use std::sync::Arc;
        let fs = Arc::new(InMemoryFs::new());

        fs.write("dup.md", b"# Dup").await.unwrap();
        let vault = Vault::init(Arc::clone(&fs), test_peer_id()).await.unwrap();

        // Tombstone the node, then register the path again to mint a SECOND, alive
        // node at the same path (register_file only matches alive nodes, so it creates
        // a fresh one rather than reviving the tombstone) — a duplicate-node pair.
        fs.delete("dup.md").await.unwrap();
        vault.delete_file("dup.md").await.unwrap();
        fs.write("dup.md", b"# Dup").await.unwrap();
        vault.register_file("dup.md").unwrap();
        vault.save_registry().await.unwrap();
        drop(vault);

        // Reload so rebuild_path_cache recomputes deleted_paths over BOTH nodes:
        // alive-wins must keep "dup.md" out of deleted_paths.
        let vault = Vault::load(Arc::clone(&fs), test_peer_id()).await.unwrap();
        assert!(
            !vault.is_path_deleted_in_registry("dup.md"),
            "alive-wins: a path with any alive node must not be in deleted_paths"
        );

        let report = vault.reconcile().await.unwrap();
        assert!(
            !report.quarantined.contains(&"dup.md".to_string()),
            "a path with an alive duplicate node must never be quarantined"
        );
        assert!(
            fs.exists("dup.md").await.unwrap(),
            "the file must stay on disk"
        );
        assert!(
            !fs.exists(".trash/dup.md").await.unwrap(),
            "no .trash entry for an alive-duplicate path"
        );
    }

    #[tokio::test]
    async fn test_reconcile_quarantine_does_not_fight_alive_recreated_file() {
        // A1 (data-loss guard): a path can be in `deleted_paths` (stale from a
        // delete_file) AND carry an alive node at the same path (a re-create that
        // registered a fresh node). `register_file` does not clear deleted_paths, so
        // without the alive-node guard, quarantine would delete the user's freshly
        // recreated file. The path_to_node guard must short-circuit BEFORE any disk
        // move when an alive node occupies the path.
        //
        // Inline because it calls `quarantine_orphan` (pub(crate)) directly to
        // exercise the guard in isolation and asserts on `path_to_node` (pub(crate));
        // no public entry point drives quarantine_orphan on its own.
        use std::sync::Arc;
        let fs = Arc::new(InMemoryFs::new());

        fs.write("recreated.md", b"# Original").await.unwrap();
        let vault = Vault::init(Arc::clone(&fs), test_peer_id()).await.unwrap();

        // Delete it (tombstones, inserts into deleted_paths synchronously).
        fs.delete("recreated.md").await.unwrap();
        vault.delete_file("recreated.md").await.unwrap();

        // User recreates the file and it is registered live again (fresh node).
        fs.write("recreated.md", b"# Recreated by user").await.unwrap();
        vault.register_file("recreated.md").unwrap();

        // Precondition: deleted_paths is still armed (register_file doesn't clear it),
        // but an alive node now occupies the path.
        assert!(
            vault.path_to_node().contains_key("recreated.md"),
            "precondition: alive node present at the recreated path"
        );

        // Directly exercising the guard: quarantine must be a no-op for an alive path.
        vault.quarantine_orphan("recreated.md").await.unwrap();

        assert!(
            fs.exists("recreated.md").await.unwrap(),
            "the user's recreated file must NOT be quarantined"
        );
        assert!(
            !fs.exists(".trash/recreated.md").await.unwrap(),
            "no .trash entry — the alive node makes this not an orphan"
        );
    }

    // ========== Registry debris / dedupe (inline: pub(crate) forge + file_tree) ==========

    /// Build a vault holding TWO alive file nodes at one path — the cross-machine
    /// parallel-index debris the dedupe tool targets. `register_file` short-circuits
    /// on an existing alive node, so the pair can't be made by calling it twice on one
    /// vault. Instead, two vaults register the path under DISTINCT peer ids
    /// (`test_peer_id` vs `test_peer_id_2`), then one registry is exported and imported
    /// into the other; after `rebuild_path_cache` the merged registry carries both alive
    /// TreeIDs at the same path. Returns the loaded vault plus the two TreeIDs so callers
    /// can assert on the deterministic (lowest-TreeID) winner.
    async fn seed_duplicate_alive_pair(
        fs: &std::sync::Arc<InMemoryFs>,
        path: &str,
        content: &[u8],
    ) -> (Vault<std::sync::Arc<InMemoryFs>>, TreeID, TreeID) {
        fs.write(path, content).await.unwrap();

        // Vault A registers the path under peer A.
        let vault_a = Vault::init(std::sync::Arc::clone(fs), test_peer_id())
            .await
            .unwrap();
        let id_a = vault_a.register_file(path).unwrap();
        vault_a.save_registry().await.unwrap();

        // Vault B (a SEPARATE registry doc, peer B) registers the same path — yielding a
        // second node with a different TreeID. A fresh InMemoryFs keeps B's .sync isolated
        // from A's so the import below merges two independent registrations.
        let fs_b = std::sync::Arc::new(InMemoryFs::new());
        fs_b.write(path, content).await.unwrap();
        let vault_b = Vault::init(std::sync::Arc::clone(&fs_b), test_peer_id_2())
            .await
            .unwrap();
        let id_b = vault_b.register_file(path).unwrap();
        assert_ne!(
            id_a, id_b,
            "the two registrations must mint distinct TreeIDs"
        );

        // Merge B's registry into A's, then rebuild A's cache over both nodes.
        let b_snapshot = vault_b
            .registry()
            .export(loro::ExportMode::Snapshot)
            .unwrap();
        vault_a.registry_mut().import(&b_snapshot).unwrap();
        vault_a.rebuild_path_cache();

        (vault_a, id_a, id_b)
    }

    #[tokio::test]
    async fn test_find_registry_debris_flags_relic() {
        // A relic is an alive file node whose .md AND .loro are both gone — a tombstone
        // that never landed. The inspector flags it (so --apply can later tombstone it)
        // but only when BOTH backing files are absent; otherwise it is a live node.
        //
        // Inline because init's index pass writes the document `.loro` for the
        // pre-existing file, so producing the relic signature requires stripping that
        // blob via the `pub(crate)` `document_sync_path` — there is no public path to
        // the hashed `.loro` name.
        use std::sync::Arc;
        let fs = Arc::new(InMemoryFs::new());

        fs.write("relic.md", b"# Relic").await.unwrap();
        let vault = Vault::init(Arc::clone(&fs), test_peer_id()).await.unwrap();
        let relic_id = vault.register_file("relic.md").unwrap();
        vault.save_registry().await.unwrap();

        // Strip both backing files WITHOUT tombstoning the node — the node stays alive
        // in the tree with nothing on disk, which is the relic signature.
        fs.delete("relic.md").await.unwrap();
        let loro_path = vault.document_sync_path("relic.md");
        // The .loro may or may not have been written; delete is idempotent enough here.
        let _ = fs.delete(&loro_path).await;

        let report = vault.find_registry_debris().await.unwrap();

        assert_eq!(
            report.relics.len(),
            1,
            "the alive node with no .md and no .loro must be flagged as a relic, got: {:?}",
            report.relics
        );
        let relic = &report.relics[0];
        assert_eq!(relic.node, relic_id);
        assert_eq!(relic.path, "relic.md");
        assert!(
            report.duplicate_groups.is_empty(),
            "a single relic node is not a duplicate group"
        );
    }

    #[tokio::test]
    async fn test_find_registry_debris_flags_duplicate_alive_pair() {
        // Two alive file nodes at one path is the prime debris class. The inspector must
        // surface the group and name the deterministic winner: the lowest TreeID (Ord on
        // (peer, counter)), so every machine resolves the same survivor from a converged
        // registry. The loser is reported but nothing is mutated.
        use std::sync::Arc;
        let fs = Arc::new(InMemoryFs::new());

        let (vault, id_a, id_b) = seed_duplicate_alive_pair(&fs, "dup.md", b"# Dup").await;
        let expected_winner = std::cmp::min(id_a, id_b);

        let report = vault.find_registry_debris().await.unwrap();

        assert_eq!(
            report.duplicate_groups.len(),
            1,
            "exactly one duplicate group expected, got: {:?}",
            report.duplicate_groups
        );
        let group = &report.duplicate_groups[0];
        assert_eq!(group.path, "dup.md");
        assert_eq!(
            group.alive_nodes.len(),
            2,
            "the group must list both alive TreeIDs"
        );
        assert!(group.alive_nodes.contains(&id_a));
        assert!(group.alive_nodes.contains(&id_b));
        assert_eq!(
            group.winner, expected_winner,
            "the winner must be the lowest TreeID (deterministic across machines)"
        );
        assert!(
            report.relics.is_empty(),
            "an alive duplicate pair is not a relic"
        );
    }

    #[tokio::test]
    async fn test_apply_dedupe_keeps_winner_tombstones_loser_disk_untouched() {
        // The core dedupe: a duplicate alive pair at one path collapses to the
        // deterministic winner (lowest TreeID). The loser is tombstoned, the winner
        // stays alive, and — crucially — the .md on disk is NOT touched, because both
        // twins share the same doc_id and an alive winner still occupies the path.
        use std::sync::Arc;
        let fs = Arc::new(InMemoryFs::new());

        let (vault, id_a, id_b) = seed_duplicate_alive_pair(&fs, "dup.md", b"# Dup").await;
        let winner = std::cmp::min(id_a, id_b);
        let loser = std::cmp::max(id_a, id_b);

        let report = vault.find_registry_debris().await.unwrap();
        let stats = vault.apply_dedupe(&report).await.unwrap();

        assert_eq!(stats.groups_deduped, 1, "one group was deduped");
        assert_eq!(stats.nodes_tombstoned, 1, "exactly the loser was tombstoned");
        assert_eq!(stats.relics_tombstoned, 0, "no relics in this fixture");

        let tree = vault.file_tree();
        assert!(
            !tree.is_node_deleted(&winner).unwrap_or(true),
            "the winner (lowest TreeID) must remain alive"
        );
        assert!(
            tree.is_node_deleted(&loser).unwrap_or(false),
            "the loser (higher TreeID) must be tombstoned"
        );
        assert!(
            fs.exists("dup.md").await.unwrap(),
            "the .md on disk must be untouched — a tombstone is a tree-only mutation"
        );
    }

    #[tokio::test]
    async fn test_apply_dedupe_winner_independent_of_import_order() {
        // The winner is the lowest TreeID regardless of which registry was imported into
        // which — running the pair construction in the opposite import order must tombstone
        // the SAME node. This is what lets two operators run --apply on two machines and
        // converge on the same survivor.
        use std::sync::Arc;

        // Order 1: B imported into A (the helper's default).
        let fs1 = Arc::new(InMemoryFs::new());
        let (vault1, id1_a, id1_b) = seed_duplicate_alive_pair(&fs1, "dup.md", b"# Dup").await;
        let report1 = vault1.find_registry_debris().await.unwrap();
        vault1.apply_dedupe(&report1).await.unwrap();
        let surviving1 = {
            let tree = vault1.file_tree();
            [id1_a, id1_b]
                .into_iter()
                .find(|id| !tree.is_node_deleted(id).unwrap_or(true))
                .expect("one twin survives")
        };

        // Order 2: build the pair by importing A into B (reverse direction). The minted
        // TreeIDs come from the same two peers, so the min-TreeID winner is the same node;
        // we assert the survivor's peer matches order 1's, proving order-independence.
        let fs_a = Arc::new(InMemoryFs::new());
        fs_a.write("dup.md", b"# Dup").await.unwrap();
        let vault_a = Vault::init(Arc::clone(&fs_a), test_peer_id())
            .await
            .unwrap();
        vault_a.register_file("dup.md").unwrap();
        vault_a.save_registry().await.unwrap();

        let fs_b = Arc::new(InMemoryFs::new());
        fs_b.write("dup.md", b"# Dup").await.unwrap();
        let vault_b = Vault::init(Arc::clone(&fs_b), test_peer_id_2())
            .await
            .unwrap();
        vault_b.register_file("dup.md").unwrap();
        // Reverse import: A's registry into B.
        let a_snapshot = vault_a
            .registry()
            .export(loro::ExportMode::Snapshot)
            .unwrap();
        vault_b.registry_mut().import(&a_snapshot).unwrap();
        vault_b.rebuild_path_cache();

        let report2 = vault_b.find_registry_debris().await.unwrap();
        vault_b.apply_dedupe(&report2).await.unwrap();
        let surviving2 = {
            let tree = vault_b.file_tree();
            tree.nodes()
                .into_iter()
                .find(|id| {
                    !tree.is_node_deleted(id).unwrap_or(true)
                        && vault_b.get_node_path(id).as_deref() == Some("dup.md")
                })
                .expect("one twin survives")
        };

        assert_eq!(
            surviving1.peer, surviving2.peer,
            "the surviving (min-TreeID) node must be the same regardless of import order"
        );
    }

    #[tokio::test]
    async fn test_apply_dedupe_tombstones_relic() {
        // A relic (alive node, no .md, no .loro) is tombstoned under apply. Reusing the
        // relic fixture: register, then strip both backing files without tombstoning.
        //
        // Inline because the tombstone assertion reads `file_tree()` (pub(crate)).
        use std::sync::Arc;
        let fs = Arc::new(InMemoryFs::new());

        fs.write("relic.md", b"# Relic").await.unwrap();
        let vault = Vault::init(Arc::clone(&fs), test_peer_id()).await.unwrap();
        let relic_id = vault.register_file("relic.md").unwrap();
        vault.save_registry().await.unwrap();
        fs.delete("relic.md").await.unwrap();
        let loro_path = vault.document_sync_path("relic.md");
        let _ = fs.delete(&loro_path).await;

        let report = vault.find_registry_debris().await.unwrap();
        let stats = vault.apply_dedupe(&report).await.unwrap();

        assert_eq!(stats.relics_tombstoned, 1, "the relic must be tombstoned");
        assert_eq!(stats.groups_deduped, 0, "no duplicate groups in this fixture");
        assert!(
            vault.file_tree().is_node_deleted(&relic_id).unwrap_or(false),
            "the relic node must be tombstoned"
        );
    }

    #[tokio::test]
    async fn test_apply_dedupe_leaves_folder_dups_alive() {
        // Folder dedupe is scoped out of v1 (recursive delete + split children = data loss).
        // A duplicate FOLDER pair must be left fully alive by apply_dedupe — the report
        // surfaces it for visibility only. Registering a nested path on two vaults then
        // merging yields duplicate folder nodes for the parent dir alongside the file dup.
        use std::sync::Arc;
        let fs = Arc::new(InMemoryFs::new());

        // A nested path: both vaults independently create a "dir" folder node, so the merge
        // produces a folder duplicate group at "dir" in addition to the file dup.
        let (vault, _, _) = seed_duplicate_alive_pair(&fs, "dir/dup.md", b"# Dup").await;

        let report = vault.find_registry_debris().await.unwrap();
        assert_eq!(
            report.folder_dups.len(),
            1,
            "the nested-path fixture must surface one folder duplicate group, got: {:?}",
            report.folder_dups
        );
        let folder_nodes = report.folder_dups[0].alive_nodes.clone();

        vault.apply_dedupe(&report).await.unwrap();

        // Every folder node in the group must still be alive — apply_dedupe never touches
        // folder dups.
        let tree = vault.file_tree();
        for id in &folder_nodes {
            assert!(
                !tree.is_node_deleted(id).unwrap_or(true),
                "folder node {:?} must remain alive — folder dedupe is out of scope for v1",
                id
            );
        }
    }

    #[tokio::test]
    async fn test_apply_dedupe_is_idempotent_with_stale_report() {
        // Re-running the dedupe must be a graceful no-op even with a STALE report — the
        // exact safety the live 445-node vault depends on, since an operator could re-run
        // --apply against a report captured before the first pass. The original report's
        // losers are already tombstoned the second time around; apply_dedupe must skip them
        // (tree.delete on an already-deleted node ERRORS, so a naive re-run would fail) and
        // return zero new tombstones rather than erroring or double-acting.
        use std::sync::Arc;
        let fs = Arc::new(InMemoryFs::new());

        let (vault, _, _) = seed_duplicate_alive_pair(&fs, "dup.md", b"# Dup").await;
        let report = vault.find_registry_debris().await.unwrap();
        vault.apply_dedupe(&report).await.unwrap();

        // A fresh scan must see a converged registry: the losers are tombstoned, so each
        // path has exactly one alive node.
        let report_after = vault.find_registry_debris().await.unwrap();
        assert!(
            report_after.duplicate_groups.is_empty(),
            "after dedupe, no duplicate groups remain, got: {:?}",
            report_after.duplicate_groups
        );

        // Re-apply the ORIGINAL (pre-dedupe, now stale) report: its losers are already
        // tombstoned. This must NOT error and must tombstone nothing — the already-deleted
        // skip-guard turns the stale re-run into a clean no-op.
        let stats_stale = vault
            .apply_dedupe(&report)
            .await
            .expect("re-applying a stale report must not error on already-tombstoned nodes");
        assert_eq!(
            stats_stale,
            DedupeStats::default(),
            "re-applying a stale report must tombstone nothing (losers already gone)"
        );
    }
}

