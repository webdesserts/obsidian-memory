//! Vault: Manages a collection of NoteDocuments and syncs with peers.

mod reconcile;
mod state;

pub use state::*;

use crate::document::NoteDocument;
use crate::events::{EventBus, Subscription, SyncEvent};
use crate::fs::FileSystem;
use crate::{PeerId, VaultId};

use loro::{LoroDoc, LoroTree, TreeID, TreeParentId, VersionVector};
use std::collections::{HashMap, HashSet};

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

    // ========== File Tree Operations (LoroTree) ==========

    /// Get the file tree from the registry
    pub(crate) fn file_tree(&self) -> LoroTree {
        self.registry().get_tree(REGISTRY_TREE)
    }

    /// Rebuild the path cache from the current tree state.
    /// Call this after applying sync updates.
    pub(crate) fn rebuild_path_cache(&self) {
        self.path_to_node_mut().clear();
        let tree = self.file_tree();

        // Deleted file paths recovered from node meta. A deleted node's real path is not
        // walkable (Loro reports its parent as `Deleted`), so we read the `path` meta
        // written at registration.
        //
        // Residual gap: the `path` meta is only written by builds that include this change,
        // so any node deleted before this build first ran on a given device carries no
        // `path` meta and is skipped here. In practice that means pre-existing production
        // tombstones are NOT guarded — only deletions made post-upgrade are recoverable by
        // path and thus protected against restart-time resurrection. This is bounded: those
        // historic tombstones have already CRDT-synced to peers, and every delete from here
        // forward is fully durable. Closing it would require a one-time backfill (out of scope).
        let mut deleted_paths: HashSet<String> = HashSet::new();

        for node_id in tree.nodes() {
            let is_deleted = tree.is_node_deleted(&node_id).unwrap_or(true);

            let Ok(meta) = tree.get_meta(node_id) else {
                continue;
            };

            let node_type = meta.get(TREE_META_TYPE).and_then(|v| {
                if let loro::ValueOrContainer::Value(val) = v {
                    val.as_string().map(|s| s.to_string())
                } else {
                    None
                }
            });

            // Only file nodes participate in the path cache and the deleted-paths guard.
            if node_type.as_deref() != Some("file") {
                continue;
            }

            if is_deleted {
                if let Some(path) = Self::tree_meta_string(&meta, TREE_META_PATH) {
                    deleted_paths.insert(path);
                }
            } else if let Some(path) = self.get_node_path(&node_id) {
                self.path_to_node_mut().insert(path, node_id);
            }
        }

        // Alive wins: a path occupied by any alive node is not deleted, regardless of any
        // duplicate-node debris or a re-create-after-delete that left an old deleted node
        // carrying the same `path` meta.
        for alive_path in self.path_to_node().keys() {
            deleted_paths.remove(alive_path);
        }

        self.sync_state.replace_deleted_paths(deleted_paths);

        tracing::debug!(
            "Rebuilt path cache with {} entries",
            self.path_to_node().len()
        );
    }

    /// Read a string-valued tree node meta field, or None if absent / not a string.
    fn tree_meta_string(meta: &loro::LoroMap, key: &str) -> Option<String> {
        meta.get(key).and_then(|v| {
            if let loro::ValueOrContainer::Value(val) = v {
                val.as_string().map(|s| s.to_string())
            } else {
                None
            }
        })
    }

    /// Get the path for a node by walking up the tree
    pub(crate) fn get_node_path(&self, node_id: &TreeID) -> Option<String> {
        let tree = self.file_tree();
        let mut parts = vec![];
        let mut current = *node_id;

        loop {
            // Get node metadata
            let meta = tree.get_meta(current).ok()?;
            let name = meta.get(TREE_META_NAME).and_then(|v| {
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
        self.path_to_node().get(path).copied()
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

    /// Validate a sync path for security
    fn validate_sync_path(path: &str) -> Result<()> {
        // Empty path
        if path.is_empty() {
            return Err(VaultError::InvalidPath("Empty path not allowed".into()));
        }
        // Path traversal
        if path.contains("..") {
            return Err(VaultError::InvalidPath("Path traversal not allowed".into()));
        }
        // Empty segments (a//b.md)
        if path.contains("//") {
            return Err(VaultError::InvalidPath(
                "Empty path segment not allowed".into(),
            ));
        }
        // Absolute paths (Unix)
        if path.starts_with('/') {
            return Err(VaultError::InvalidPath("Absolute path not allowed".into()));
        }
        // Absolute paths (Windows - drive letter)
        if path.len() >= 2 && path.chars().nth(1) == Some(':') {
            return Err(VaultError::InvalidPath(
                "Windows absolute path not allowed".into(),
            ));
        }
        // Backslash
        if path.contains('\\') {
            return Err(VaultError::InvalidPath(
                "Backslash in path not allowed".into(),
            ));
        }
        // Null bytes
        if path.contains('\0') {
            return Err(VaultError::InvalidPath(
                "Null byte in path not allowed".into(),
            ));
        }
        // Must be .md
        if !path.ends_with(".md") {
            return Err(VaultError::InvalidPath(
                "Only markdown files allowed".into(),
            ));
        }
        // Control characters
        if path.chars().any(|c| c.is_control()) {
            return Err(VaultError::InvalidPath(
                "Control character in path not allowed".into(),
            ));
        }
        // Path length limit (filesystem safety)
        if path.len() > 1024 {
            return Err(VaultError::InvalidPath("Path too long".into()));
        }
        Ok(())
    }

    /// Register a new file in the tree (creates parent folders as needed).
    /// Returns the TreeID of the created file node.
    ///
    /// # Persistence
    ///
    /// This mutates the registry CRDT tree **in memory only**. It is sync, so it
    /// cannot flush on its own — callers own persistence. Async callers must follow
    /// it with `save_registry()` once the mutation reaches a consistent state, or the
    /// registration silently dies on the next restart. (Kept `pub` because the
    /// sync-wasm shim calls it across crate boundaries.)
    pub fn register_file(&self, path: &str) -> Result<TreeID> {
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
            .map_err(|e| VaultError::TreeOperation(format!("Failed to create file node: {}", e)))?;

        let meta = tree
            .get_meta(node_id)
            .map_err(|e| VaultError::TreeOperation(format!("Failed to get file meta: {}", e)))?;
        meta.insert(TREE_META_TYPE, "file")
            .map_err(|e| VaultError::TreeOperation(format!("Failed to set file type: {}", e)))?;
        meta.insert(TREE_META_NAME, file_name[0])
            .map_err(|e| VaultError::TreeOperation(format!("Failed to set file name: {}", e)))?;
        meta.insert(TREE_META_DOC_ID, simple_hash(path))
            .map_err(|e| VaultError::TreeOperation(format!("Failed to set doc_id: {}", e)))?;
        // Store the full path so the node's path is recoverable after deletion.
        meta.insert(TREE_META_PATH, path)
            .map_err(|e| VaultError::TreeOperation(format!("Failed to set path meta: {}", e)))?;

        // Update cache
        self.path_to_node_mut().insert(path.to_string(), node_id);

        // A local re-create at a previously-deleted path leaves the path in deleted_paths
        // until the next rebuild_path_cache. That stale entry is harmless: on_file_changed
        // has already written the .loro and cached the document, so any inbound
        // DocumentUpdate for this path takes the exists/merge branch in apply_single_update
        // and never reaches the is_path_deleted_in_registry guard. The next
        // rebuild_path_cache then drops the path ("alive wins") as eventual cleanup.

        tracing::debug!("Registered file in tree: {}", path);
        Ok(node_id)
    }

    /// Delete a file from the tree (CRDT operation - tracked, reversible).
    /// Also cleans up the .loro document file.
    ///
    /// Returns `true` if a live tree node was tombstoned, `false` if the path was
    /// already absent from the registry (idempotent no-op). Callers can gate
    /// broadcasts on this bool without consulting the sync flag.
    pub async fn delete_file(&self, path: &str) -> Result<bool> {
        Self::validate_sync_path(path)?;

        if let Some(node_id) = self.find_node_by_path(path) {
            let tree = self.file_tree();
            tree.delete(node_id).map_err(|e| {
                VaultError::TreeOperation(format!("Failed to delete file node: {}", e))
            })?;

            // Mark the path deleted synchronously so an inbound DocumentUpdate arriving
            // before the next registry sync doesn't resurrect it. rebuild_path_cache also
            // derives this from registry truth, but it isn't called from delete_file, so
            // the synchronous insert is what guards the live-session window.
            self.mark_path_deleted(path);

            // Remove from cache
            self.path_to_node_mut().remove(path);

            // Clean up .loro document
            let sync_path = self.document_sync_path(path);
            if self.fs.exists(&sync_path).await? {
                self.fs.delete(&sync_path).await?;
            }

            // Remove from documents cache
            self.documents_mut().remove(path);

            // Persist the deletion tombstone immediately so peers importing the
            // saved registry see the op even if the process restarts before the
            // next inbound sync triggers apply_registry_updates.
            self.save_registry().await?;

            tracing::info!("Deleted file from tree: {}", path);
            Ok(true)
        } else if self.is_path_deleted_in_registry(path) {
            // A registry tombstone already covers this path, so this is a redundant
            // watcher echo for a delete that already recorded its tombstone (e.g. the
            // local watcher firing after delete_file already ran). The deletion has
            // propagated; nothing is lost, so this is debug-level noise, not a warning.
            tracing::debug!(
                "delete_file: '{}' already tombstoned — redundant delete, no-op",
                path
            );
            Ok(false)
        } else {
            // Genuinely unknown path: no node at all (alive or tombstoned), so the
            // deletion records no tombstone and won't propagate to peers. The `false`
            // return carries this diagnostic for daemon callers, but the warn stays for
            // non-daemon callers that ignore the bool.
            tracing::warn!(
                "delete_file: no registry node for '{}' — no tombstone recorded",
                path
            );
            Ok(false)
        }
    }

    /// Rename/move a file in the tree (CRDT operation via tree move).
    pub async fn rename_file(&self, old_path: &str, new_path: &str) -> Result<()> {
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

                // Update documents cache - extract first to release mutex before re-acquiring
                let doc = self.documents_mut().remove(old_path);
                if let Some(doc) = doc {
                    self.documents_mut().insert(new_path.to_string(), doc);
                }

                // Register in tree and persist
                self.register_file(new_path)?;
                self.save_registry().await?;
                return Ok(());
            } else if self.fs.exists(new_path).await.unwrap_or(false) {
                // Target already exists (rename already happened) - just register it
                tracing::debug!(
                    "rename_file: source {} not in tree, but {} exists - registering target",
                    old_path,
                    new_path
                );
                self.register_file(new_path)?;
                self.save_registry().await?;

                // Clean up orphaned .loro at old path if it exists
                let old_sync = self.document_sync_path(old_path);
                if self.fs.exists(&old_sync).await.unwrap_or(false) {
                    let _ = self.fs.delete(&old_sync).await;
                }

                return Ok(());
            }
            return Err(VaultError::RenameSourceMissing(format!(
                "Source file not found: {}",
                old_path
            )));
        };

        // Check target doesn't exist
        if self.find_node_by_path(new_path).is_some() {
            return Err(VaultError::RenameTargetExists(format!(
                "Target already exists: {}",
                new_path
            )));
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
            .map_err(|e| VaultError::TreeOperation(format!("Failed to move file node: {}", e)))?;

        // Update name in metadata
        let meta = tree
            .get_meta(node_id)
            .map_err(|e| VaultError::TreeOperation(format!("Failed to get file meta: {}", e)))?;
        meta.insert(TREE_META_NAME, new_name[0])
            .map_err(|e| VaultError::TreeOperation(format!("Failed to update file name: {}", e)))?;
        meta.insert(TREE_META_DOC_ID, simple_hash(new_path))
            .map_err(|e| VaultError::TreeOperation(format!("Failed to update doc_id: {}", e)))?;
        // This main path moves the node via tree.mov() rather than register_file, so the
        // path meta must be updated explicitly to keep the deleted-path guard accurate if
        // the renamed node is later deleted.
        meta.insert(TREE_META_PATH, new_path)
            .map_err(|e| VaultError::TreeOperation(format!("Failed to update path meta: {}", e)))?;

        // Update caches
        self.path_to_node_mut().remove(old_path);
        self.path_to_node_mut()
            .insert(new_path.to_string(), node_id);

        // Move .loro document file
        let old_sync_path = self.document_sync_path(old_path);
        let new_sync_path = self.document_sync_path(new_path);
        if self.fs.exists(&old_sync_path).await? {
            let bytes = self.fs.read(&old_sync_path).await?;
            self.fs.atomic_write(&new_sync_path, &bytes).await?;
            self.fs.delete(&old_sync_path).await?;
        }

        // Update documents cache - extract first to release mutex before re-acquiring
        let doc = self.documents_mut().remove(old_path);
        if let Some(doc) = doc {
            self.documents_mut().insert(new_path.to_string(), doc);
        }

        // Persist the move op so restarts see the updated registry.
        self.save_registry().await?;

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
    fn get_or_create_folder(&self, parent: TreeParentId, name: &str) -> Result<TreeParentId> {
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

                let child_name = meta.get(TREE_META_NAME).and_then(|v| {
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
        let node_id = tree.create(parent).map_err(|e| {
            VaultError::TreeOperation(format!("Failed to create folder node: {}", e))
        })?;

        let meta = tree
            .get_meta(node_id)
            .map_err(|e| VaultError::TreeOperation(format!("Failed to get folder meta: {}", e)))?;
        meta.insert(TREE_META_TYPE, "folder")
            .map_err(|e| VaultError::TreeOperation(format!("Failed to set folder type: {}", e)))?;
        meta.insert(TREE_META_NAME, name)
            .map_err(|e| VaultError::TreeOperation(format!("Failed to set folder name: {}", e)))?;

        Ok(TreeParentId::Node(node_id))
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::VaultId;
    use crate::fs::{FsError, InMemoryFs};

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

    // ========== SyncMetadata Tests ==========

    #[test]
    fn test_sync_metadata_default_has_version_1() {
        let meta = SyncMetadata::default();
        assert_eq!(meta.version, 1);
    }

    #[test]
    fn test_sync_metadata_serializes_to_toml() {
        let meta = SyncMetadata {
            version: 1,
            vault_id: VaultId::from(0xa1b2c3d4e5f67890u64),
        };
        let toml_str = toml::to_string(&meta).unwrap();
        assert!(toml_str.contains("version = 1"));
        assert!(toml_str.contains("vault_id = \"a1b2c3d4e5f67890\""));
    }

    #[test]
    fn test_sync_metadata_deserializes_from_toml() {
        let toml_str = "version = 1\nvault_id = \"a1b2c3d4e5f67890\"\n";
        let meta: SyncMetadata = toml::from_str(toml_str).unwrap();
        assert_eq!(meta.version, 1);
        assert_eq!(meta.vault_id.as_u64(), 0xa1b2c3d4e5f67890);
    }

    #[test]
    fn test_sync_metadata_roundtrip() {
        let original = SyncMetadata::new();
        let toml_str = toml::to_string(&original).unwrap();
        let parsed: SyncMetadata = toml::from_str(&toml_str).unwrap();
        assert_eq!(original.version, parsed.version);
        assert_eq!(original.vault_id, parsed.vault_id);
    }

    // ========== SyncMetadata Migration Tests ==========

    #[tokio::test]
    async fn test_init_generates_vault_id_and_writes_metadata() {
        use std::sync::Arc;
        let fs = Arc::new(InMemoryFs::new());
        let vault = Vault::init(Arc::clone(&fs), test_peer_id()).await.unwrap();

        // metadata.toml should exist
        assert!(fs.exists(METADATA_FILE).await.unwrap());

        // Should contain a valid VaultId
        let bytes = fs.read(METADATA_FILE).await.unwrap();
        let meta: SyncMetadata = toml::from_str(&String::from_utf8(bytes).unwrap()).unwrap();
        assert_eq!(meta.version, 1);
        assert_eq!(meta.vault_id, vault.vault_id());
    }

    #[tokio::test]
    async fn test_init_then_load_roundtrip_same_vault_id() {
        use std::sync::Arc;
        let fs = Arc::new(InMemoryFs::new());

        let vault1 = Vault::init(Arc::clone(&fs), test_peer_id()).await.unwrap();
        let vault_id = vault1.vault_id();
        drop(vault1);

        let vault2 = Vault::load(Arc::clone(&fs), test_peer_id()).await.unwrap();
        assert_eq!(vault2.vault_id(), vault_id);
    }

    // ========== VaultId Adoption Tests ==========

    #[tokio::test]
    async fn test_adopt_vault_id_rewrites_metadata_and_persists() {
        use std::sync::Arc;
        let fs = Arc::new(InMemoryFs::new());

        let mut vault = Vault::init(Arc::clone(&fs), test_peer_id()).await.unwrap();
        let original_id = vault.vault_id();
        let adopted_id = VaultId::from(0xdeadbeefcafef00du64);
        assert_ne!(original_id, adopted_id, "test ids must differ");

        vault.adopt_vault_id(adopted_id).await.unwrap();

        // In-memory id reflects the adoption.
        assert_eq!(vault.vault_id(), adopted_id);

        // Reloading the vault from disk reads the adopted id — proves persistence,
        // not just in-memory mutation.
        drop(vault);
        let reloaded = Vault::load(Arc::clone(&fs), test_peer_id()).await.unwrap();
        assert_eq!(reloaded.vault_id(), adopted_id);
    }

    #[tokio::test]
    async fn test_adopt_vault_id_preserves_format_version() {
        use std::sync::Arc;
        let fs = Arc::new(InMemoryFs::new());

        let mut vault = Vault::init(Arc::clone(&fs), test_peer_id()).await.unwrap();
        vault
            .adopt_vault_id(VaultId::from(0x1111222233334444u64))
            .await
            .unwrap();

        let bytes = fs.read(METADATA_FILE).await.unwrap();
        let meta: SyncMetadata = toml::from_str(&String::from_utf8(bytes).unwrap()).unwrap();
        assert_eq!(
            meta.version, 1,
            "adoption must not change the format version"
        );
    }

    #[tokio::test]
    async fn test_adopt_vault_id_same_id_is_noop() {
        use std::sync::Arc;
        let fs = Arc::new(InMemoryFs::new());

        let mut vault = Vault::init(Arc::clone(&fs), test_peer_id()).await.unwrap();
        let current = vault.vault_id();

        vault.adopt_vault_id(current).await.unwrap();
        assert_eq!(vault.vault_id(), current);
    }

    #[tokio::test]
    async fn test_legacy_vault_migration_generates_vault_id() {
        use std::sync::Arc;
        let fs = Arc::new(InMemoryFs::new());

        // Simulate a legacy vault: .sync/ exists but no metadata.toml
        fs.mkdir(SYNC_DIR).await.unwrap();
        fs.mkdir(&format!("{}/documents", SYNC_DIR)).await.unwrap();

        // Load should run v0→v1 migration
        let vault = Vault::load(Arc::clone(&fs), test_peer_id()).await.unwrap();

        // metadata.toml should now exist with version 1
        let bytes = fs.read(METADATA_FILE).await.unwrap();
        let meta: SyncMetadata = toml::from_str(&String::from_utf8(bytes).unwrap()).unwrap();
        assert_eq!(meta.version, 1);
        assert_eq!(meta.vault_id, vault.vault_id());
    }

    #[tokio::test]
    async fn test_migration_idempotency() {
        use std::sync::Arc;
        let fs = Arc::new(InMemoryFs::new());

        // Simulate legacy vault
        fs.mkdir(SYNC_DIR).await.unwrap();
        fs.mkdir(&format!("{}/documents", SYNC_DIR)).await.unwrap();

        // First migration
        let vault1 = Vault::load(Arc::clone(&fs), test_peer_id()).await.unwrap();
        let vault_id = vault1.vault_id();
        drop(vault1);

        // Second load — should read same VaultId, not generate new one
        let vault2 = Vault::load(Arc::clone(&fs), test_peer_id()).await.unwrap();
        assert_eq!(vault2.vault_id(), vault_id);
    }

    #[tokio::test]
    async fn test_version_too_new_returns_error() {
        let fs = InMemoryFs::new();
        fs.mkdir(SYNC_DIR).await.unwrap();
        fs.mkdir(&format!("{}/documents", SYNC_DIR)).await.unwrap();

        // Write metadata with a future version
        let meta = format!("version = 99\nvault_id = \"{}\"\n", VaultId::generate());
        fs.write(METADATA_FILE, meta.as_bytes()).await.unwrap();

        let result = Vault::load(fs, test_peer_id()).await;
        let err = result.err().expect("should fail with version too new");
        assert!(
            err.to_string().contains("newer than supported"),
            "Got: {}",
            err
        );
    }

    #[tokio::test]
    async fn test_corrupt_metadata_returns_error() {
        let fs = InMemoryFs::new();
        fs.mkdir(SYNC_DIR).await.unwrap();
        fs.mkdir(&format!("{}/documents", SYNC_DIR)).await.unwrap();

        // Write garbage to metadata.toml
        fs.write(METADATA_FILE, b"not valid toml {{{{")
            .await
            .unwrap();

        let result = Vault::load(fs, test_peer_id()).await;
        let err = result.err().expect("should fail with corrupt metadata");
        assert!(err.to_string().contains("Corrupt metadata"), "Got: {}", err);
    }

    // ========== Vault Tests ==========

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
        let vault = Vault::init(fs, test_peer_id()).await.unwrap();

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
        let vault = Vault::load(Arc::clone(&fs), test_peer_id()).await.unwrap();

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
        let vault = Vault::load(Arc::clone(&fs), test_peer_id()).await.unwrap();

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
    async fn test_reconcile_orphan_report_uses_real_path_not_empty() {
        // When a markdown file is deleted offline, its orphaned `.loro` must be
        // reported under the file's real path. The orphan-loader previously passed
        // an empty path to `from_bytes`, which clobbered the doc's stored META_PATH
        // to "" and surfaced empty `Orphaned .loro file (deleted?):` reports.
        use std::sync::Arc;

        let fs = Arc::new(InMemoryFs::new());

        fs.write("notes/deleted.md", b"# Deleted Note")
            .await
            .unwrap();
        let _vault = Vault::init(Arc::clone(&fs), test_peer_id()).await.unwrap();

        // Delete the markdown offline; the .loro orphan remains on disk.
        fs.delete("notes/deleted.md").await.unwrap();

        // Reload and reconcile — the orphan report must carry the real path.
        let vault = Vault::load(Arc::clone(&fs), test_peer_id()).await.unwrap();
        let report = vault.reconcile().await.unwrap();

        assert!(
            report.orphaned.contains(&"notes/deleted.md".to_string()),
            "orphan report should contain the real path, got: {:?}",
            report.orphaned
        );
        assert!(
            !report.orphaned.contains(&String::new()),
            "orphan report must not contain an empty path, got: {:?}",
            report.orphaned
        );
    }

    #[tokio::test]
    async fn test_reconcile_detects_file_move() {
        use std::sync::Arc;

        let fs = Arc::new(InMemoryFs::new());

        // Initialize vault with a file
        fs.write("old_name.md", b"# Unique Content ABC123")
            .await
            .unwrap();
        let _vault = Vault::init(Arc::clone(&fs), test_peer_id()).await.unwrap();

        // Simulate renaming the file while plugin was off
        let content = fs.read("old_name.md").await.unwrap();
        fs.write("new_name.md", &content).await.unwrap();
        fs.delete("old_name.md").await.unwrap();

        // Load vault - should detect move and migrate .loro file
        let vault = Vault::load(Arc::clone(&fs), test_peer_id()).await.unwrap();

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
        assert!(
            !fs.exists(&format!("{}/documents/{}.loro", SYNC_DIR, old_hash))
                .await
                .unwrap()
        );
        assert!(
            fs.exists(&format!("{}/documents/{}.loro", SYNC_DIR, new_hash))
                .await
                .unwrap()
        );
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
        let vault = Vault::load(Arc::clone(&fs), test_peer_id()).await.unwrap();

        // The moved file should be accessible
        let doc = vault.get_document("knowledge/note.md").await.unwrap();
        assert!(doc.to_markdown().contains("My Note XYZ789"));

        // Only the new path should exist
        let files = vault.list_files().await.unwrap();
        assert!(!files.contains(&"note.md".to_string()));
        assert!(files.contains(&"knowledge/note.md".to_string()));
    }

    #[tokio::test]
    async fn test_reconcile_skips_race_deleted_file() {
        // A markdown file deleted between list_files() and the per-file reconcile
        // body (a race window during startup scan) must NOT abort Vault::load. The
        // surviving files reconcile normally; the vanished one is skipped.
        use std::sync::Arc;

        let fs = Arc::new(InMemoryFs::new());

        fs.write("keep.md", b"# Keep").await.unwrap();
        let _vault = Vault::init(Arc::clone(&fs), test_peer_id()).await.unwrap();

        // A brand-new file appears (no .loro yet). The wrapping filesystem lets
        // list_files() still enumerate it but returns FsError::NotFound when reconcile
        // reads it to index — exactly the race where a file vanishes between the
        // directory scan and the per-file body.
        fs.write("racy.md", b"# Racy").await.unwrap();
        let race_fs = Arc::new(VanishOnReadFs::new(Arc::clone(&fs), "racy.md"));

        // Before the fix: NotFound propagates through reconcile → Vault::load → Err.
        // After: the file is skipped (debug-logged), load succeeds, survivor present.
        let vault = Vault::load(race_fs, test_peer_id())
            .await
            .expect("Vault::load must survive a race-deleted file during reconcile");

        let files = vault.list_files().await.unwrap();
        assert!(
            files.contains(&"keep.md".to_string()),
            "surviving file should still reconcile, got: {:?}",
            files
        );
    }

    #[tokio::test]
    async fn test_load_with_corrupt_registry_returns_error() {
        // A corrupt registry.loro must HARD-FAIL the load rather than silently
        // falling back to an empty registry. An empty registry re-indexes every file
        // with fresh doc_ids → mass divergence from peers → latest-wins content
        // clobber. Failing loud is the only safe behavior. (See Item 3 / audit.)
        use std::sync::Arc;

        let fs = Arc::new(InMemoryFs::new());

        fs.write("note.md", b"# Note").await.unwrap();
        let _vault = Vault::init(Arc::clone(&fs), test_peer_id()).await.unwrap();

        // Corrupt the persisted registry.
        fs.write(REGISTRY_FILE, b"not a valid loro snapshot")
            .await
            .unwrap();

        let result = Vault::load(Arc::clone(&fs), test_peer_id()).await;
        assert!(
            matches!(result, Err(VaultError::CorruptRegistry(_))),
            "corrupt registry must fail with CorruptRegistry, got: {:?}",
            result.map(|_| "Ok(vault)")
        );
    }

    #[tokio::test]
    async fn test_reconcile_orphan_warning_uses_hash_for_empty_path() {
        // A legacy orphan .loro persisted with META_PATH="" (written by the
        // pre-5fd4a63 orphan loader that passed "" to from_bytes) must be reported
        // under its hash, not an empty string. stored_path() returns Some("") for
        // such docs, so the unwrap_or fallbacks don't fire — a defensive
        // empty-string filter is what keeps the warning/report meaningful.
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

    // ========== Tree Operation Tests ==========

    // ========== Registry Persistence Tests ==========
    //
    // These tests verify that local registry tree mutations (register, delete,
    // rename) survive a process restart — i.e., the in-memory LoroDoc is written
    // to disk after each mutation so a fresh Vault::load recovers the same state.

    #[tokio::test]
    async fn test_register_file_survives_reload() {
        // Validates the bare register + caller-flush contract: register_file mutates
        // the tree in memory only, and the async caller's save_registry() is what makes
        // the node durable. This must isolate the contract from init's batched index
        // save AND from reconcile re-registering on reload:
        //   - init with NO pre-existing file, so the index pass registers nothing.
        //   - register the file AFTER init, then flush explicitly via save_registry().
        //   - delete the markdown before reloading, so reconcile has nothing to
        //     re-register — a node present after reload can only come from the
        //     persisted registry snapshot, not a fresh index pass.
        use std::sync::Arc;
        let fs = Arc::new(InMemoryFs::new());

        let vault = Vault::init(Arc::clone(&fs), test_peer_id()).await.unwrap();

        // Bare sync registration (in memory) followed by the explicit caller flush.
        fs.write("note.md", b"# Hello").await.unwrap();
        vault.register_file("note.md").unwrap();
        vault.save_registry().await.unwrap();

        // Remove the markdown so reload's reconcile can't re-register it from disk.
        fs.delete("note.md").await.unwrap();
        drop(vault);

        // Load a fresh vault over the same fs — the node must come from the persisted
        // registry, proving register_file's mutation was flushed to disk.
        let vault2 = Vault::load(Arc::clone(&fs), test_peer_id()).await.unwrap();
        assert!(
            vault2.path_to_node().contains_key("note.md"),
            "registered path must survive a reload via the persisted registry"
        );
    }

    #[tokio::test]
    async fn test_delete_file_tombstone_survives_reload() {
        // A deletion tombstone must be persisted so that peers importing the saved
        // registry see the deletion op. After reload the path must be absent from
        // the alive set, and a fresh peer importing the saved registry must also see
        // the node as deleted (tombstone carried in the CRDT snapshot).
        //
        // The test mirrors the daemon sequence: the OS/user deletes the markdown file
        // first, then delete_file records the CRDT tombstone. Without the markdown on
        // disk, reconcile during reload has nothing to re-register.
        use std::sync::Arc;
        let fs = Arc::new(InMemoryFs::new());

        // Register the file via init's index pass
        fs.write("note.md", b"# Hello").await.unwrap();
        let vault = Vault::init(Arc::clone(&fs), test_peer_id()).await.unwrap();

        // Simulate the daemon sequence: filesystem delete first, then CRDT tombstone
        fs.delete("note.md").await.unwrap();
        vault.delete_file("note.md").await.unwrap();

        // Grab the registry snapshot to verify it carries the tombstone op
        let saved_bytes = fs.read(REGISTRY_FILE).await.unwrap();
        drop(vault);

        // Reload — path must still be gone from alive set
        let vault2 = Vault::load(Arc::clone(&fs), test_peer_id()).await.unwrap();
        assert!(
            !vault2.path_to_node().contains_key("note.md"),
            "deleted path must not reappear after reload"
        );

        // A fresh peer importing the saved registry must also see the node deleted
        let peer_doc = loro::LoroDoc::new();
        peer_doc.import(&saved_bytes).unwrap();
        let peer_tree = peer_doc.get_tree(REGISTRY_TREE);
        let any_alive = peer_tree
            .nodes()
            .into_iter()
            .filter(|id| !peer_tree.is_node_deleted(id).unwrap_or(true))
            .count();
        assert_eq!(
            any_alive, 0,
            "saved registry must carry the deletion tombstone for peers"
        );
    }

    #[tokio::test]
    async fn test_rename_file_survives_reload() {
        // A rename must persist the updated registry so the new path (not the old)
        // survives a reload. The test simulates the real watcher sequence: the OS
        // moves the file (old gone, new exists), then rename_file records the CRDT op.
        use std::sync::Arc;
        let fs = Arc::new(InMemoryFs::new());

        // Start with old.md registered
        fs.write("old.md", b"# Content").await.unwrap();
        let vault = Vault::init(Arc::clone(&fs), test_peer_id()).await.unwrap();

        // Simulate OS-level rename: delete old, create new on the filesystem.
        // rename_file is called after the FS move, matching the daemon's event sequence.
        fs.delete("old.md").await.unwrap();
        fs.write("new.md", b"# Content").await.unwrap();
        vault.rename_file("old.md", "new.md").await.unwrap();
        drop(vault);

        let vault2 = Vault::load(Arc::clone(&fs), test_peer_id()).await.unwrap();
        assert!(
            vault2.path_to_node().contains_key("new.md"),
            "renamed path must survive reload"
        );
        assert!(
            !vault2.path_to_node().contains_key("old.md"),
            "old path must not appear in registry after reload"
        );
    }

    #[tokio::test]
    async fn test_reconcile_batch_registrations_survive_reload() {
        // Reconcile can register hundreds of files during startup. The registrations
        // must be batched (one save at the end of reconcile, not per-file) and must
        // survive a second load. This test verifies correctness; the per-file O(n)
        // write cost is the failure mode we're guarding against structurally.
        use std::sync::Arc;
        let fs = Arc::new(InMemoryFs::new());

        // Pre-populate several markdown files before init so reconcile finds them
        fs.write("a.md", b"# A").await.unwrap();
        fs.write("b.md", b"# B").await.unwrap();
        fs.write("c.md", b"# C").await.unwrap();

        // init already calls index_existing_files, which registers via on_file_changed
        let _vault = Vault::init(Arc::clone(&fs), test_peer_id()).await.unwrap();
        drop(_vault);

        // Second load must find all three paths already in the registry
        let vault2 = Vault::load(Arc::clone(&fs), test_peer_id()).await.unwrap();
        assert!(
            vault2.path_to_node().contains_key("a.md"),
            "a.md must survive reload"
        );
        assert!(
            vault2.path_to_node().contains_key("b.md"),
            "b.md must survive reload"
        );
        assert!(
            vault2.path_to_node().contains_key("c.md"),
            "c.md must survive reload"
        );
    }

    #[tokio::test]
    async fn test_delete_file_warns_on_untracked_path() {
        // delete_file on a path with no registry node must emit a warn log and
        // succeed silently (no error). This makes the silent no-op diagnosable.
        // We can't easily assert on tracing output, so this just confirms the
        // current Result::Ok behavior is preserved after the warn is added.
        let fs = InMemoryFs::new();
        let vault = Vault::init(fs, test_peer_id()).await.unwrap();

        // Calling delete on an unregistered path must not return an error
        let result = vault.delete_file("untracked.md").await;
        assert!(result.is_ok(), "delete of untracked path must succeed silently");
    }

    #[tokio::test]
    async fn test_delete_file_removes_from_tree() {
        let fs = InMemoryFs::new();

        // Create and index a file
        fs.write("note.md", b"# Hello").await.unwrap();
        let vault = Vault::init(fs, test_peer_id()).await.unwrap();
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
        let vault = Vault::init(fs, test_peer_id()).await.unwrap();
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
        let vault = Vault::init(fs, test_peer_id()).await.unwrap();

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
        let vault = Vault::init(fs, test_peer_id()).await.unwrap();

        // Null bytes should be rejected
        let result = vault.delete_file("foo\0.md").await;
        assert!(result.is_err());

        let result = vault.register_file("bar\0.md");
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn test_non_markdown_rejected() {
        let fs = InMemoryFs::new();
        let vault = Vault::init(fs, test_peer_id()).await.unwrap();

        // Non-markdown files should be rejected
        let result = vault.register_file("script.js");
        assert!(result.is_err());

        let result = vault.delete_file("image.png").await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn test_empty_path_rejected() {
        let fs = InMemoryFs::new();
        let vault = Vault::init(fs, test_peer_id()).await.unwrap();

        // Empty path should be rejected
        let result = vault.register_file("");
        assert!(result.is_err());

        let result = vault.delete_file("").await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn test_empty_segment_rejected() {
        let fs = InMemoryFs::new();
        let vault = Vault::init(fs, test_peer_id()).await.unwrap();

        // Empty path segments (a//b.md) should be rejected
        let result = vault.register_file("a//b.md");
        assert!(result.is_err());

        let result = vault.delete_file("foo//bar.md").await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn test_path_too_long_rejected() {
        let fs = InMemoryFs::new();
        let vault = Vault::init(fs, test_peer_id()).await.unwrap();

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
        let vault1 = Vault::init(Arc::clone(&fs1), test_peer_id()).await.unwrap();

        // Sync to vault2
        let vault2 = Vault::init(Arc::clone(&fs2), test_peer_id_2())
            .await
            .unwrap();
        let request = vault2.prepare_sync_request().await.unwrap();
        let (exchange, _) = vault1.process_sync_message(&request).await.unwrap();
        let (final_resp, _) = vault2
            .process_sync_message(&exchange.unwrap())
            .await
            .unwrap();
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
        let (_, _) = vault2
            .process_sync_message(&exchange2.unwrap())
            .await
            .unwrap();

        // Vault2 should now see the file as deleted
        assert!(vault2.is_file_deleted("note.md"));
    }

    /// A registry-mediated move (tree.mov, same node, new parent path) must clean up
    /// the old physical .md and .loro on the RECEIVER. Without this, every move leaves
    /// an untracked orphan at the old path on every peer — the bug that accumulated 25
    /// stranded root notes in production.
    #[tokio::test]
    async fn test_move_syncs_via_registry_removes_old_path() {
        use std::sync::Arc;

        let fs1 = Arc::new(InMemoryFs::new());
        let fs2 = Arc::new(InMemoryFs::new());

        // Create file in vault1
        fs1.write("note.md", b"# Hello").await.unwrap();
        let vault1 = Vault::init(Arc::clone(&fs1), test_peer_id()).await.unwrap();

        // Handshake so both vaults have note.md
        let vault2 = Vault::init(Arc::clone(&fs2), test_peer_id_2())
            .await
            .unwrap();
        let request = vault2.prepare_sync_request().await.unwrap();
        let (exchange, _) = vault1.process_sync_message(&request).await.unwrap();
        let (final_resp, _) = vault2
            .process_sync_message(&exchange.unwrap())
            .await
            .unwrap();
        if let Some(resp) = final_resp {
            vault1.process_sync_message(&resp).await.unwrap();
        }

        assert!(!vault1.is_file_deleted("note.md"));
        assert!(!vault2.is_file_deleted("note.md"));
        assert!(fs2.exists("note.md").await.unwrap());
        let old_sync_path = vault2.document_sync_path("note.md");

        // Move note.md → moved/note.md on vault1 (mirror test_rename_file_updates_tree:
        // write the target file, then rename_file performs the tree.mov + fs cleanup).
        fs1.write("moved/note.md", b"# Hello").await.unwrap();
        fs1.delete("note.md").await.unwrap();
        vault1
            .rename_file("note.md", "moved/note.md")
            .await
            .unwrap();

        // Sync again — vault2 should see the move via registry
        let request2 = vault2.prepare_sync_request().await.unwrap();
        let (exchange2, _) = vault1.process_sync_message(&request2).await.unwrap();
        let (_, _) = vault2
            .process_sync_message(&exchange2.unwrap())
            .await
            .unwrap();

        // Old path must be gone on the receiver: no orphaned .md, no orphaned .loro.
        assert!(
            !fs2.exists("note.md").await.unwrap(),
            "old .md at note.md should be removed on the receiver after a move"
        );
        assert!(
            !fs2.exists(&old_sync_path).await.unwrap(),
            "old .loro for note.md should be removed on the receiver after a move"
        );

        // New path must exist with the original content.
        assert!(
            fs2.exists("moved/note.md").await.unwrap(),
            "moved/note.md should exist on the receiver after a move"
        );
        let moved_content = fs2.read("moved/note.md").await.unwrap();
        assert_eq!(moved_content, b"# Hello");
    }

    /// Swap case: two files exchange paths in a SINGLE registry exchange. The naive
    /// move-cleanup (delete every vacated old_path) would delete the file the OTHER node
    /// just moved into and strip its document update — permanent data loss on the receiver.
    /// The fix excludes any old_path that an alive node now occupies. This pins that
    /// exclusion against regression.
    #[tokio::test]
    async fn test_swap_move_syncs_via_registry() {
        use std::sync::Arc;

        let fs1 = Arc::new(InMemoryFs::new());
        let fs2 = Arc::new(InMemoryFs::new());

        // Create two files in vault1
        fs1.write("a.md", b"# AAA").await.unwrap();
        fs1.write("b.md", b"# BBB").await.unwrap();
        let vault1 = Vault::init(Arc::clone(&fs1), test_peer_id()).await.unwrap();

        // Handshake so both vaults have a.md and b.md
        let vault2 = Vault::init(Arc::clone(&fs2), test_peer_id_2())
            .await
            .unwrap();
        let request = vault2.prepare_sync_request().await.unwrap();
        let (exchange, _) = vault1.process_sync_message(&request).await.unwrap();
        let (final_resp, _) = vault2
            .process_sync_message(&exchange.unwrap())
            .await
            .unwrap();
        if let Some(resp) = final_resp {
            vault1.process_sync_message(&resp).await.unwrap();
        }

        assert_eq!(fs2.read("a.md").await.unwrap(), b"# AAA");
        assert_eq!(fs2.read("b.md").await.unwrap(), b"# BBB");

        // Swap on vault1: a.md → b.md and b.md → a.md via a temp path. Both moves accumulate
        // in the registry and reach vault2 in ONE exchange.
        fs1.write("tmp.md", b"# AAA").await.unwrap();
        fs1.delete("a.md").await.unwrap();
        vault1.rename_file("a.md", "tmp.md").await.unwrap();

        fs1.write("a.md", b"# BBB").await.unwrap();
        fs1.delete("b.md").await.unwrap();
        vault1.rename_file("b.md", "a.md").await.unwrap();

        fs1.write("b.md", b"# AAA").await.unwrap();
        fs1.delete("tmp.md").await.unwrap();
        vault1.rename_file("tmp.md", "b.md").await.unwrap();

        // Sync the whole swap to vault2 in one exchange
        let request2 = vault2.prepare_sync_request().await.unwrap();
        let (exchange2, _) = vault1.process_sync_message(&request2).await.unwrap();
        let (_, _) = vault2
            .process_sync_message(&exchange2.unwrap())
            .await
            .unwrap();

        // The B1 exclusion's job: both swapped-into paths must SURVIVE on the receiver.
        // Without it, the naive move-cleanup deletes the file an alive node just moved into,
        // so `fs2.exists(...)` goes false (data loss). With it, both files remain present.
        // The final swapped CONTENT would be handled by apply_single_update's divergent-history
        // reconciliation if content changes accompanied the move; this test pins the structural
        // survival invariant, not the content flip (content was seeded at the pre-swap handshake
        // and content correctness in the swap case is out of scope for this move-cleanup fix).
        assert!(
            fs2.exists("a.md").await.unwrap(),
            "a.md must survive — not be deleted by b's vacate-cleanup"
        );
        assert!(
            fs2.exists("b.md").await.unwrap(),
            "b.md must survive — not be deleted by a's vacate-cleanup"
        );
    }

    // ========== SyncState Tests ==========

    #[test]
    fn test_sync_state_mark_and_consume() {
        let tracker = SyncState::new();

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
    fn test_sync_state_multiple_paths() {
        let tracker = SyncState::new();

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
    fn test_sync_state_clone_shares_state() {
        let tracker1 = SyncState::new();
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
        let vault1 = Vault::init(Arc::clone(&fs1), test_peer_id()).await.unwrap();
        vault1.on_file_changed("note.md").await.unwrap();

        // Create empty vault2
        let vault2 = Vault::init(Arc::clone(&fs2), test_peer_id_2())
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
        let (_, modified2) = vault2.process_sync_message(&update.unwrap()).await.unwrap();

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
        let vault = Vault::init(fs, test_peer_id()).await.unwrap();

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
        let vault1 = Vault::init(Arc::clone(&fs1), test_peer_id()).await.unwrap();
        let vault2 = Vault::init(Arc::clone(&fs2), test_peer_id_2())
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
    fn test_sync_state_flag_within_ttl() {
        let tracker = SyncState::new();

        // Mark and immediately check - should be within TTL
        tracker.mark_synced("test.md");
        assert!(tracker.is_synced("test.md"));
        assert!(tracker.consume_synced("test.md"));

        // After consume, flag is gone
        assert!(!tracker.is_synced("test.md"));
        assert!(!tracker.consume_synced("test.md"));
    }

    #[test]
    fn test_sync_state_cleanup_expired() {
        let tracker = SyncState::new();

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
    fn test_sync_state_rename_marks_both_paths() {
        // This tests the behavior expected when a rename sync is processed
        let tracker = SyncState::new();

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

    // ========== Debug API Tests ==========

    #[tokio::test]
    async fn test_get_registry_version() {
        let fs = InMemoryFs::new();
        fs.write("note.md", b"# Hello").await.unwrap();
        let vault = Vault::init(fs, test_peer_id()).await.unwrap();
        vault.on_file_changed("note.md").await.unwrap();

        let version = vault.get_registry_version();
        // Registry is authored by the device peer id, so the version vector is
        // keyed by the author's u64 hash (16-char hex), not the VaultId.
        let author_key = format!("{:016x}", test_peer_id().as_u64());
        assert!(
            version.contains_key(&author_key),
            "Expected device author {} in version {:?}",
            author_key,
            version
        );
    }

    #[tokio::test]
    async fn test_get_registry_stats() {
        let fs = InMemoryFs::new();
        fs.write("note.md", b"# Hello").await.unwrap();
        let vault = Vault::init(fs, test_peer_id()).await.unwrap();
        vault.on_file_changed("note.md").await.unwrap();

        let stats = vault.get_registry_stats();
        // op_count should be non-zero after registering a file
        // (Loro batches changes, so change_count may still be 0 before commit)
        assert!(
            stats.op_count > 0 || stats.change_count > 0,
            "Expected at least some operations, got op_count={}, change_count={}",
            stats.op_count,
            stats.change_count
        );
    }

    #[tokio::test]
    async fn test_get_document_blob_meta_not_found() {
        let fs = InMemoryFs::new();
        let vault = Vault::init(fs, test_peer_id()).await.unwrap();
        let meta = vault
            .get_document_blob_meta("nonexistent.md")
            .await
            .unwrap();
        assert!(meta.is_none());
    }

    #[tokio::test]
    async fn test_get_document_blob_meta() {
        let fs = InMemoryFs::new();
        fs.write("test.md", b"# Hello").await.unwrap();
        let vault = Vault::init(fs, test_peer_id()).await.unwrap();
        vault.on_file_changed("test.md").await.unwrap();

        let meta = vault
            .get_document_blob_meta("test.md")
            .await
            .unwrap()
            .unwrap();
        assert!(meta.change_count > 0);
        // Version vectors should contain our peer
        assert!(!meta.end_version.is_empty());
    }

    #[tokio::test]
    async fn test_get_document_info_not_found() {
        let fs = InMemoryFs::new();
        let vault = Vault::init(fs, test_peer_id()).await.unwrap();
        let info = vault.get_document_info("nonexistent.md").await.unwrap();
        assert!(info.is_none());
    }

    #[tokio::test]
    async fn test_get_document_info() {
        let fs = InMemoryFs::new();
        fs.write("test.md", b"# Hello").await.unwrap();
        let vault = Vault::init(fs, test_peer_id()).await.unwrap();
        vault.on_file_changed("test.md").await.unwrap();

        let info = vault.get_document_info("test.md").await.unwrap().unwrap();
        assert_eq!(info.path, "test.md");
        assert!(info.body_length > 0);
        assert!(info.change_count > 0);
        assert!(info.doc_id.is_some());
        assert!(!info.version.is_empty());
    }

    #[tokio::test]
    async fn test_get_document_info_with_frontmatter() {
        let fs = InMemoryFs::new();
        let content = "---\ntitle: Test\n---\n\n# Hello";
        fs.write("test.md", content.as_bytes()).await.unwrap();
        let vault = Vault::init(fs, test_peer_id()).await.unwrap();
        vault.on_file_changed("test.md").await.unwrap();

        let info = vault.get_document_info("test.md").await.unwrap().unwrap();
        assert!(info.has_frontmatter);
    }

    // ========== Format Stability Tests ==========
    //
    // These tests act as tripwires — if any format changes, the test fails and
    // forces the developer to consider whether a version bump + migration is needed.

    #[test]
    fn test_simple_hash_known_outputs() {
        // simple_hash uses FNV-1a. Changing the algorithm breaks doc_id linkage
        // between registry tree nodes and .loro document files.
        assert_eq!(simple_hash("test.md"), "9006b3b63c0e9510");
        assert_eq!(simple_hash("folder/note.md"), "a1b149525257902d");
        assert_eq!(
            simple_hash("deeply/nested/path/file.md"),
            "daec06fdbc6a5936"
        );
        assert_eq!(simple_hash(""), "cbf29ce484222325"); // FNV-1a offset basis
    }

    #[test]
    fn test_registry_tree_container_name() {
        assert_eq!(REGISTRY_TREE, "files");
    }

    #[test]
    fn test_document_container_names() {
        use crate::document::{
            BODY_CONTAINER, FRONTMATTER_CONTAINER, META_CONTAINER, META_DOC_ID, META_PATH,
        };
        assert_eq!(META_CONTAINER, "_meta");
        assert_eq!(FRONTMATTER_CONTAINER, "frontmatter");
        assert_eq!(BODY_CONTAINER, "body");
        assert_eq!(META_DOC_ID, "doc_id");
        assert_eq!(META_PATH, "path");
    }

    #[test]
    fn test_tree_meta_field_names() {
        assert_eq!(TREE_META_TYPE, "type");
        assert_eq!(TREE_META_NAME, "name");
        assert_eq!(TREE_META_DOC_ID, "doc_id");
        assert_eq!(TREE_META_PATH, "path");
    }

    #[test]
    fn test_peer_id_display_format_is_64_char_hex() {
        let peer_id = PeerId::generate();
        let display = peer_id.to_string();
        assert_eq!(display.len(), 64);
        assert!(display.chars().all(|c| c.is_ascii_hexdigit()));
    }

    #[test]
    fn test_vault_id_display_format_is_16_char_hex() {
        let vault_id = VaultId::from(0xa1b2c3d4e5f67890u64);
        let display = vault_id.to_string();
        assert_eq!(display.len(), 16);
        assert!(display.chars().all(|c| c.is_ascii_hexdigit()));
        assert_eq!(display, "a1b2c3d4e5f67890");
    }

    #[test]
    fn test_sync_dir_paths() {
        assert_eq!(SYNC_DIR, ".sync");
        assert_eq!(REGISTRY_FILE, ".sync/registry.loro");
        assert_eq!(METADATA_FILE, ".sync/metadata.toml");
    }

    #[test]
    fn test_metadata_toml_field_names() {
        // Verify the TOML keys used in metadata.toml are stable
        let meta = SyncMetadata {
            version: 1,
            vault_id: VaultId::from(0x1234567890abcdefu64),
        };
        let toml_str = toml::to_string(&meta).unwrap();
        assert!(toml_str.contains("version = "));
        assert!(toml_str.contains("vault_id = "));
    }

    // ========== Orphan Quarantine Tests ==========
    //
    // Reconcile moves untracked disk orphans — `.md` files on disk whose registry
    // state is tombstoned — to `.trash/<path>`. These cover the happy path, the
    // safety guards (never touch absent or alive paths), idempotency, and the A1
    // data-loss guard (a recreated file with a live node at a stale-tombstoned path
    // must NOT be quarantined).

    /// Seed a tombstoned-with-meta orphan and return a loaded vault whose
    /// `deleted_paths` is repopulated from the persisted tombstone, with the orphan
    /// markdown NOT yet on disk. The caller writes the orphan strand back to disk and
    /// drives `reconcile()` itself so it can assert on the returned report.
    ///
    /// (The orphan must be off disk at load time, otherwise `Vault::load`'s own
    /// reconcile quarantines it before the caller's explicit reconcile runs — the
    /// load-time path is covered by the Commit 3 NativeFs integration test.)
    async fn seed_tombstoned_orphan(
        fs: &std::sync::Arc<InMemoryFs>,
        path: &str,
        content: &[u8],
    ) -> Vault<std::sync::Arc<InMemoryFs>> {
        fs.write(path, content).await.unwrap();
        let vault = Vault::init(std::sync::Arc::clone(fs), test_peer_id())
            .await
            .unwrap();
        fs.delete(path).await.unwrap();
        vault.delete_file(path).await.unwrap();
        drop(vault);

        // Load fresh (orphan still off disk) so rebuild_path_cache repopulates
        // deleted_paths from the persisted tombstone and load's reconcile is a no-op.
        Vault::load(std::sync::Arc::clone(fs), test_peer_id())
            .await
            .unwrap()
    }

    #[tokio::test]
    async fn test_reconcile_quarantines_tombstoned_orphan() {
        use std::sync::Arc;
        let fs = Arc::new(InMemoryFs::new());

        let vault = seed_tombstoned_orphan(&fs, "orphan.md", b"# Orphan").await;
        // Write the orphan strand back to disk, then reconcile.
        fs.write("orphan.md", b"# Orphan").await.unwrap();
        let report = vault.reconcile().await.unwrap();

        // The orphan was moved to .trash/, not re-indexed.
        assert!(
            report.quarantined.contains(&"orphan.md".to_string()),
            "tombstoned orphan should be quarantined, got: {:?}",
            report.quarantined
        );
        assert!(
            !report.indexed.contains(&"orphan.md".to_string()),
            "a tombstoned orphan must never be indexed as a new file"
        );

        // Original gone from the vault, present under .trash/ on disk.
        assert!(
            !fs.exists("orphan.md").await.unwrap(),
            "the orphan must be removed from its original path"
        );
        assert!(
            fs.exists(".trash/orphan.md").await.unwrap(),
            "the orphan must be moved under .trash/"
        );

        // Registry untouched: the path stays tombstoned (no alive node).
        assert!(
            !vault.path_to_node().contains_key("orphan.md"),
            "quarantine must not register an alive node"
        );
        assert!(
            vault.is_path_deleted_in_registry("orphan.md"),
            "the path must remain tombstoned after quarantine"
        );
    }

    #[tokio::test]
    async fn test_reconcile_does_not_quarantine_registry_absent_file() {
        // A disk file with no registry entry at all (never deleted) must be indexed
        // as new, never quarantined. Regression guard against over-deletion.
        use std::sync::Arc;
        let fs = Arc::new(InMemoryFs::new());

        let _vault = Vault::init(Arc::clone(&fs), test_peer_id()).await.unwrap();
        fs.write("brand_new.md", b"# Brand New").await.unwrap();

        let vault = Vault::load(Arc::clone(&fs), test_peer_id()).await.unwrap();
        let report = vault.reconcile().await.unwrap();

        assert!(
            !report.quarantined.contains(&"brand_new.md".to_string()),
            "a registry-absent file must not be quarantined"
        );
        assert!(
            fs.exists("brand_new.md").await.unwrap(),
            "the absent file must stay on disk and be indexed"
        );
        assert!(
            !fs.exists(".trash/brand_new.md").await.unwrap(),
            "no .trash entry should be created for an absent file"
        );
    }

    #[tokio::test]
    async fn test_reconcile_does_not_quarantine_meta_less_tombstone() {
        // A deleted node with no TREE_META_PATH never enters deleted_paths, so its
        // path is indistinguishable from an absent file. A same-named disk file must
        // therefore be treated as new (indexed), NOT quarantined — proving we never
        // over-delete when the tombstone carries no recoverable path.
        //
        // We simulate the pre-upgrade meta-less tombstone by deleting the file via a
        // direct tree delete that bypasses the path-meta recording path. There is no
        // such path on the current build (delete_file always records meta), so we
        // assert the equivalent observable: a path that is NOT in deleted_paths but
        // exists on disk is indexed, never quarantined.
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
    async fn test_reconcile_alive_node_no_disk_file_is_noop() {
        // Constraint 7: an alive registry node whose path has no disk file is a
        // registry-debris relic, not a disk orphan. Reconcile must leave it untouched
        // (nothing on disk to clean) and must not crash.
        use std::sync::Arc;
        let fs = Arc::new(InMemoryFs::new());

        fs.write("relic.md", b"# Relic").await.unwrap();
        let vault = Vault::init(Arc::clone(&fs), test_peer_id()).await.unwrap();
        // Remove the markdown but leave the alive node in the registry.
        fs.delete("relic.md").await.unwrap();
        // Note: we do NOT delete_file, so the node stays alive with no disk file.

        let report = vault.reconcile().await.unwrap();
        assert!(
            report.quarantined.is_empty(),
            "an alive-node-no-disk-file relic must not be quarantined, got: {:?}",
            report.quarantined
        );
        assert!(
            !fs.exists(".trash/relic.md").await.unwrap(),
            "no .trash entry for a relic with no disk file"
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

    /// Test fs that delegates to a shared InMemoryFs but returns
    /// `FsError::NotFound` when reading a specific armed path, simulating a file
    /// that was deleted after `list_files()` enumerated it but before the per-file
    /// reconcile body read it (the startup-scan race).
    struct VanishOnReadFs {
        inner: std::sync::Arc<InMemoryFs>,
        vanish_path: String,
    }

    impl VanishOnReadFs {
        fn new(inner: std::sync::Arc<InMemoryFs>, vanish_path: &str) -> Self {
            Self {
                inner,
                vanish_path: vanish_path.to_string(),
            }
        }
    }

    #[cfg_attr(target_arch = "wasm32", async_trait::async_trait(?Send))]
    #[cfg_attr(not(target_arch = "wasm32"), async_trait::async_trait)]
    impl FileSystem for VanishOnReadFs {
        async fn read(&self, path: &str) -> crate::fs::Result<Vec<u8>> {
            if path == self.vanish_path {
                return Err(FsError::NotFound(path.to_string()));
            }
            self.inner.read(path).await
        }
        async fn write(&self, path: &str, content: &[u8]) -> crate::fs::Result<()> {
            self.inner.write(path, content).await
        }
        async fn list(&self, path: &str) -> crate::fs::Result<Vec<crate::fs::FileEntry>> {
            self.inner.list(path).await
        }
        async fn delete(&self, path: &str) -> crate::fs::Result<()> {
            self.inner.delete(path).await
        }
        async fn exists(&self, path: &str) -> crate::fs::Result<bool> {
            self.inner.exists(path).await
        }
        async fn stat(&self, path: &str) -> crate::fs::Result<crate::fs::FileStat> {
            self.inner.stat(path).await
        }
        async fn mkdir(&self, path: &str) -> crate::fs::Result<()> {
            self.inner.mkdir(path).await
        }
        async fn rename(&self, from: &str, to: &str) -> crate::fs::Result<()> {
            self.inner.rename(from, to).await
        }
    }

    /// Test fs that delegates to a shared InMemoryFs but fails writes into
    /// `.trash/`, simulating a real-fs quarantine failure (full disk, permission
    /// error). Wraps the same Arc the seed state was written through.
    struct TrashWriteFailingFs {
        inner: std::sync::Arc<InMemoryFs>,
    }

    #[cfg_attr(target_arch = "wasm32", async_trait::async_trait(?Send))]
    #[cfg_attr(not(target_arch = "wasm32"), async_trait::async_trait)]
    impl FileSystem for TrashWriteFailingFs {
        async fn read(&self, path: &str) -> crate::fs::Result<Vec<u8>> {
            self.inner.read(path).await
        }
        async fn write(&self, path: &str, content: &[u8]) -> crate::fs::Result<()> {
            if path.starts_with(".trash/") {
                return Err(FsError::Io("simulated trash write failure".into()));
            }
            self.inner.write(path, content).await
        }
        async fn list(&self, path: &str) -> crate::fs::Result<Vec<crate::fs::FileEntry>> {
            self.inner.list(path).await
        }
        async fn delete(&self, path: &str) -> crate::fs::Result<()> {
            self.inner.delete(path).await
        }
        async fn exists(&self, path: &str) -> crate::fs::Result<bool> {
            self.inner.exists(path).await
        }
        async fn stat(&self, path: &str) -> crate::fs::Result<crate::fs::FileStat> {
            self.inner.stat(path).await
        }
        async fn mkdir(&self, path: &str) -> crate::fs::Result<()> {
            self.inner.mkdir(path).await
        }
        async fn rename(&self, from: &str, to: &str) -> crate::fs::Result<()> {
            self.inner.rename(from, to).await
        }
    }

    #[tokio::test]
    async fn test_reconcile_quarantine_failure_does_not_abort_load() {
        // B1 regression: reconcile runs inside Vault::load, so a per-orphan quarantine
        // failure must NOT propagate and abort daemon startup. The failing orphan is
        // logged and skipped; other files still reconcile and the vault loads.
        use std::sync::Arc;

        // Seed a tombstoned orphan and a separate brand-new file in InMemoryFs.
        let seed = Arc::new(InMemoryFs::new());
        seed.write("orphan.md", b"# Orphan").await.unwrap();
        let vault = Vault::init(Arc::clone(&seed), test_peer_id())
            .await
            .unwrap();
        seed.delete("orphan.md").await.unwrap();
        vault.delete_file("orphan.md").await.unwrap();
        drop(vault);
        // Recreate the orphan strand and add a genuinely new file.
        seed.write("orphan.md", b"# Orphan").await.unwrap();
        seed.write("fresh.md", b"# Fresh").await.unwrap();

        // Wrap the SAME underlying fs so quarantine's write into .trash/ fails.
        // Load must still succeed.
        let failing = Arc::new(TrashWriteFailingFs {
            inner: Arc::clone(&seed),
        });
        let vault = Vault::load(Arc::clone(&failing), test_peer_id())
            .await
            .expect("Vault::load must succeed even when a quarantine write fails");

        // The new file was still indexed despite the orphan's quarantine failing.
        let files = vault.list_files().await.unwrap();
        assert!(
            files.contains(&"fresh.md".to_string()),
            "a brand-new file must still be indexed when a sibling orphan fails to \
             quarantine; got: {:?}",
            files
        );

        // The orphan was NOT quarantined (its .trash write failed) and was NOT
        // resurrected (the gate took the quarantine branch, not the index branch),
        // so it stays on disk for the next reconcile to retry.
        let report = vault.reconcile().await.unwrap();
        assert!(
            !report.quarantined.contains(&"orphan.md".to_string()),
            "the failing orphan must not be reported as quarantined"
        );
        assert!(
            !report.indexed.contains(&"orphan.md".to_string()),
            "the failing orphan must not be resurrected as a new index entry"
        );
    }

    #[tokio::test]
    async fn test_reconcile_quarantine_recovers_from_partial_failure_idempotently() {
        // Crash-idempotency: if a prior quarantine wrote .trash/<path> but failed to
        // delete the original (the non-atomic write→delete window), the orphan sits at
        // BOTH paths. The next reconcile must reuse the existing identical trash copy
        // and retry the delete — NOT allocate a new collision suffix. Without this,
        // .trash/<path>.N would grow without bound under a persistent delete failure.
        use std::sync::Arc;
        let fs = Arc::new(InMemoryFs::new());

        // Tombstone orphan.md in the registry, then load a fresh vault so deleted_paths
        // is repopulated (orphan off disk at load → load's reconcile is a no-op).
        fs.write("orphan.md", b"# Orphan").await.unwrap();
        let vault = Vault::init(Arc::clone(&fs), test_peer_id()).await.unwrap();
        fs.delete("orphan.md").await.unwrap();
        vault.delete_file("orphan.md").await.unwrap();
        drop(vault);
        let vault = Vault::load(Arc::clone(&fs), test_peer_id()).await.unwrap();

        // Construct the post-crash partial-failure state directly: identical content at
        // both the original path AND the trash destination (write succeeded, delete did
        // not).
        fs.write("orphan.md", b"# Orphan").await.unwrap();
        fs.write(".trash/orphan.md", b"# Orphan").await.unwrap();

        let report = vault.reconcile().await.unwrap();

        // The retry deleted the original and reported the quarantine complete.
        assert!(
            report.quarantined.contains(&"orphan.md".to_string()),
            "the partial-failure orphan should report as quarantined once the delete \
             retry succeeds, got: {:?}",
            report.quarantined
        );
        assert!(
            !fs.exists("orphan.md").await.unwrap(),
            "the original must be removed on the retry"
        );
        // The existing trash copy was reused — NO new collision-suffixed duplicate.
        assert!(
            fs.exists(".trash/orphan.md").await.unwrap(),
            ".trash/orphan.md must remain"
        );
        assert!(
            !fs.exists(".trash/orphan.md.1").await.unwrap(),
            "a new collision suffix must NOT be created when the trash copy is identical"
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

    #[tokio::test]
    async fn test_reconcile_quarantine_is_idempotent() {
        // A second reconcile pass must quarantine nothing: the first pass moved the
        // orphan under .trash/, and list_files excludes dot-directories, so the moved
        // file is no longer a candidate. Verify exactly one trash entry, no nesting.
        use std::sync::Arc;
        let fs = Arc::new(InMemoryFs::new());

        let vault = seed_tombstoned_orphan(&fs, "dupe.md", b"# Dupe").await;
        // Write the orphan strand back to disk, then reconcile.
        fs.write("dupe.md", b"# Dupe").await.unwrap();

        let first = vault.reconcile().await.unwrap();
        assert!(first.quarantined.contains(&"dupe.md".to_string()));

        let second = vault.reconcile().await.unwrap();
        assert!(
            second.quarantined.is_empty(),
            "second pass must quarantine nothing, got: {:?}",
            second.quarantined
        );
        assert!(
            fs.exists(".trash/dupe.md").await.unwrap(),
            ".trash/dupe.md must still exist after the second pass"
        );
        assert!(
            !fs.exists(".trash/.trash/dupe.md").await.unwrap(),
            "trash contents must never be re-quarantined into a nested .trash/"
        );
    }

    // ========== Registry debris inspector (find_registry_debris) ==========

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
    async fn test_find_registry_debris_flags_relic() {
        // A relic is an alive file node whose .md AND .loro are both gone — a tombstone
        // that never landed. The inspector flags it (so --apply can later tombstone it)
        // but only when BOTH backing files are absent; otherwise it is a live node.
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
    async fn test_find_registry_debris_ignores_healthy_node() {
        // A normal path — one alive node with its .md on disk — is neither a duplicate
        // nor a relic. The inspector must report nothing for it, so an operator running
        // a dry run on a clean vault sees an empty report.
        use std::sync::Arc;
        let fs = Arc::new(InMemoryFs::new());

        fs.write("healthy.md", b"# Healthy").await.unwrap();
        let vault = Vault::init(Arc::clone(&fs), test_peer_id()).await.unwrap();
        vault.register_file("healthy.md").unwrap();
        vault.save_registry().await.unwrap();

        let report = vault.find_registry_debris().await.unwrap();

        assert!(
            report.duplicate_groups.is_empty(),
            "a single-alive-node path must not be a duplicate group, got: {:?}",
            report.duplicate_groups
        );
        assert!(
            report.relics.is_empty(),
            "a node with its .md on disk must not be a relic, got: {:?}",
            report.relics
        );
    }

    // ========== Registry dedupe apply (apply_dedupe) ==========

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
