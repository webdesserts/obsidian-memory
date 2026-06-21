//! The Index: the vault's catalog — the folder tree plus per-document existence,
//! location, and UUID identity.
//!
//! The Index is one Loro document (`.sync/index.loro`) wrapping a single movable
//! `LoroTree`. Folder and file nodes carry meta describing their `type`, `name`,
//! and (for file nodes) the document's `path`, its `uuid` identity, and a
//! denormalized `content_version` fingerprint. Two in-memory caches make lookups
//! O(1): `path_to_node` (a watcher/event resolves a path to its node) and the
//! inverse `uuid_to_node` (an inbound wire `DocUpdate{uuid}` resolves to a
//! node/path).
//!
//! ## UUID identity (the headline data-model change vs the old registry)
//!
//! A file node's identity is the document's UUID — minted once in the content
//! doc's `_meta.doc_id` and written verbatim into the node's `uuid` meta. It is
//! **never recomputed**, in particular not on a move. The old registry keyed nodes
//! by a hash of the path, which changed on every rename and collided across
//! parallel-indexed paths; the UUID dissolves both problems. Because a content
//! `.loro` is addressed by `docs/<uuid>.loro` (path-independent), a move is a
//! pure-structural tree operation that touches no content — that is what makes
//! "moves re-transfer zero content" (INV-1) structurally true.
//!
//! ## Persistence boundary (this chunk)
//!
//! The Index is the catalog half: tree and cache operations are in-memory CRDT
//! mutations, independent of the filesystem. `save_index`/`load_index` take the
//! `FileSystem` at the boundary rather than holding it, so the Index stays
//! fs-agnostic. The public `Vault<F>` handle (a later chunk) owns the fs, the
//! content docs, and the flows that tie them to the Index.

mod state;
mod tree;

pub use state::{
    FileMove, IndexError, ReconcileReport, Result, SyncMetadata, SyncState, VaultId,
};

use crate::fs::FileSystem;
use loro::LoroDoc;
use std::collections::HashMap;
use loro::TreeID;
use uuid::Uuid;

#[cfg(not(target_arch = "wasm32"))]
use std::sync::Mutex;

#[cfg(target_arch = "wasm32")]
use std::cell::RefCell;

/// The `.sync/` directory holding all vault sync state.
pub(crate) const SYNC_DIR: &str = ".sync";

/// The persisted index CRDT (`.sync/index.loro`).
pub(crate) const INDEX_FILE: &str = ".sync/index.loro";

/// The `LoroTree` container name inside the index document.
///
/// This literal string is part of the on-disk format; a later chunk's names-test
/// pins it so a rename can't silently diverge the format fleet-wide.
pub(crate) const INDEX_TREE: &str = "files";

/// Node meta: discriminates `"folder"` from `"file"` nodes.
pub(crate) const TREE_META_TYPE: &str = "type";

/// Node meta: the node's own name (one path segment).
pub(crate) const TREE_META_NAME: &str = "name";

/// Node meta: the file node's full vault-relative path.
///
/// Stored explicitly so a deleted node's path is recoverable for the
/// deleted-paths guard (a tombstoned node's parent is no longer walkable).
pub(crate) const TREE_META_PATH: &str = "path";

/// Node meta (file nodes): the document's UUID identity.
///
/// Replaces the old path-hash `doc_id`. Written from the content doc's minted UUID
/// and NEVER recomputed — in particular, a move does not touch it.
pub(crate) const TREE_META_UUID: &str = "uuid";

/// Node meta (file nodes): a denormalized fingerprint of the content doc's
/// current version vector.
///
/// A **derived cache** (the authoritative source is the content doc's
/// `state_vv()`): it lets the compare protocol digest the catalog without opening
/// every content doc. This chunk introduces the field and sets an initial value at
/// registration; a later chunk bumps it on each local edit, and the compare
/// protocol consumes it. Stored as the raw 32 fingerprint bytes (a Loro binary
/// value).
pub(crate) const TREE_META_CONTENT_VERSION: &str = "content_version";

/// The on-disk path of a document's content `.loro`, addressed by its UUID.
///
/// UUID addressing (not a path hash) is what makes the content file
/// path-independent, so a move never has to relocate it.
pub fn content_doc_path(uuid: &Uuid) -> String {
    format!("{}/docs/{}.loro", SYNC_DIR, uuid)
}

/// The vault catalog: the index CRDT, its two lookup caches, and the per-session
/// sync state.
///
/// Lookups go through the caches; mutations go through the tree-op methods (in
/// `tree.rs`), which keep both caches and the deleted-paths guard consistent with
/// the CRDT. The struct holds no filesystem — persistence is `save_index` /
/// `load_index`, which take the `FileSystem` as a parameter.
pub struct Index {
    /// The index CRDT document. A single `LoroTree` of folder + file nodes.
    #[cfg(not(target_arch = "wasm32"))]
    index: Mutex<LoroDoc>,
    #[cfg(target_arch = "wasm32")]
    index: RefCell<LoroDoc>,

    /// Path → node cache. A watcher/event resolves a path to its node.
    #[cfg(not(target_arch = "wasm32"))]
    path_to_node: Mutex<HashMap<String, TreeID>>,
    #[cfg(target_arch = "wasm32")]
    path_to_node: RefCell<HashMap<String, TreeID>>,

    /// UUID → node cache (the inverse). An inbound wire `DocUpdate{uuid}` resolves
    /// to its node (and thence its current path) without scanning the tree.
    #[cfg(not(target_arch = "wasm32"))]
    uuid_to_node: Mutex<HashMap<Uuid, TreeID>>,
    #[cfg(target_arch = "wasm32")]
    uuid_to_node: RefCell<HashMap<Uuid, TreeID>>,

    /// Echo-detection + reconciliation flags + the deleted-paths resurrection guard.
    pub(crate) sync_state: SyncState,
}

impl Index {
    /// Create a fresh, empty index authored under `author` (a Loro peer id).
    ///
    /// The index tree is initialized eagerly so the document has the container on
    /// first save. Caches start empty.
    pub fn new(author: u64) -> Self {
        let index = LoroDoc::new();
        // Author the index under the device peer id (not the shared VaultId), so
        // each device's structural ops attribute to an independent replica.
        index.set_peer_id(author).ok();
        // Initialize the file tree (created on first access).
        let _tree = index.get_tree(INDEX_TREE);

        Self::from_doc(index)
    }

    /// Build the wrapper around an already-constructed index document.
    fn from_doc(index: LoroDoc) -> Self {
        #[cfg(not(target_arch = "wasm32"))]
        let this = Self {
            index: Mutex::new(index),
            path_to_node: Mutex::new(HashMap::new()),
            uuid_to_node: Mutex::new(HashMap::new()),
            sync_state: SyncState::new(),
        };
        #[cfg(target_arch = "wasm32")]
        let this = Self {
            index: RefCell::new(index),
            path_to_node: RefCell::new(HashMap::new()),
            uuid_to_node: RefCell::new(HashMap::new()),
            sync_state: SyncState::new(),
        };
        this
    }

    /// Load the index from `.sync/index.loro` (or start fresh if absent), then
    /// rebuild the caches from the loaded tree.
    ///
    /// A corrupt index hard-fails rather than silently starting empty: falling back
    /// to an empty index would re-index every file with fresh UUIDs and diverge from
    /// peers. New ops author under `author`.
    pub async fn load_index<F: FileSystem>(fs: &F, author: u64) -> Result<Self> {
        let index = if fs.exists(INDEX_FILE).await? {
            let bytes = fs.read(INDEX_FILE).await?;
            let doc = LoroDoc::new();
            doc.set_peer_id(author).ok();
            doc.import(&bytes)
                .map_err(|e| IndexError::CorruptIndex(e.to_string()))?;
            doc
        } else {
            let doc = LoroDoc::new();
            doc.set_peer_id(author).ok();
            let _tree = doc.get_tree(INDEX_TREE);
            doc
        };

        let this = Self::from_doc(index);
        this.rebuild_caches();
        Ok(this)
    }

    /// Persist the index CRDT to `.sync/index.loro` (atomic snapshot write).
    ///
    /// Tree mutations are in-memory and synchronous, so they cannot flush
    /// themselves; the caller decides when to flush — once per mutation, or once
    /// after a batch (e.g. boot reconcile) to avoid O(n) writes.
    pub async fn save_index<F: FileSystem>(&self, fs: &F) -> Result<()> {
        let bytes = self
            .index()
            .export(loro::ExportMode::Snapshot)
            .map_err(|e| IndexError::IndexExport(format!("Failed to export index: {}", e)))?;
        fs.atomic_write(INDEX_FILE, &bytes).await?;
        Ok(())
    }

    /// Borrow the index document for reads.
    #[cfg(not(target_arch = "wasm32"))]
    pub(crate) fn index(&self) -> std::sync::MutexGuard<'_, LoroDoc> {
        self.index.lock().unwrap()
    }
    #[cfg(target_arch = "wasm32")]
    pub(crate) fn index(&self) -> std::cell::Ref<'_, LoroDoc> {
        self.index.borrow()
    }

    // ========== Wire surface (consumed by the sync protocol) ==========
    //
    // The Index is the catalog CRDT; the sync protocol exchanges its updates as
    // bytes. These wrap the underlying `LoroDoc` export/import so the protocol
    // (a sibling module) never reaches into the `LoroDoc` directly.

    /// The Index CRDT's current version vector.
    ///
    /// The basis for computing what Index updates a peer is missing.
    pub(crate) fn state_vv(&self) -> loro::VersionVector {
        self.index().state_vv()
    }

    /// Export the entire Index CRDT as a snapshot (for a peer that lacks it, or
    /// when a new-doc snapshot rides along and the node must be resend-durable).
    pub(crate) fn export_snapshot(&self) -> Result<Vec<u8>> {
        self.index()
            .export(loro::ExportMode::Snapshot)
            .map_err(|e| IndexError::IndexExport(format!("Failed to export Index snapshot: {}", e)))
    }

    /// Export the Index CRDT updates since `from` (an incremental delta).
    pub(crate) fn export_updates(&self, from: &loro::VersionVector) -> Result<Vec<u8>> {
        self.index()
            .export(loro::ExportMode::updates(from))
            .map_err(|e| IndexError::IndexExport(format!("Failed to export Index updates: {}", e)))
    }

    /// Import Index CRDT updates from a peer (a delta or a snapshot — Loro merges
    /// either). Returns the [`loro::ImportStatus`] so the caller can surface ops
    /// parked with unsatisfied causal dependencies (`warn_if_pending`).
    ///
    /// Does NOT rebuild the caches — the caller does that after import (it also
    /// needs the pre-import cache to detect deletes/moves).
    pub(crate) fn import_updates(&self, data: &[u8]) -> Result<loro::ImportStatus> {
        self.index()
            .import(data)
            .map_err(|e| IndexError::IndexImport(format!("Index import failed: {}", e)))
    }

    /// Borrow the path → node cache for reads.
    #[cfg(not(target_arch = "wasm32"))]
    pub(crate) fn path_to_node(&self) -> std::sync::MutexGuard<'_, HashMap<String, TreeID>> {
        self.path_to_node.lock().unwrap()
    }
    #[cfg(target_arch = "wasm32")]
    pub(crate) fn path_to_node(&self) -> std::cell::Ref<'_, HashMap<String, TreeID>> {
        self.path_to_node.borrow()
    }

    /// Borrow the path → node cache for writes.
    #[cfg(not(target_arch = "wasm32"))]
    pub(crate) fn path_to_node_mut(&self) -> std::sync::MutexGuard<'_, HashMap<String, TreeID>> {
        self.path_to_node.lock().unwrap()
    }
    #[cfg(target_arch = "wasm32")]
    pub(crate) fn path_to_node_mut(&self) -> std::cell::RefMut<'_, HashMap<String, TreeID>> {
        self.path_to_node.borrow_mut()
    }

    /// Borrow the uuid → node cache for reads.
    #[cfg(not(target_arch = "wasm32"))]
    pub(crate) fn uuid_to_node(&self) -> std::sync::MutexGuard<'_, HashMap<Uuid, TreeID>> {
        self.uuid_to_node.lock().unwrap()
    }
    #[cfg(target_arch = "wasm32")]
    pub(crate) fn uuid_to_node(&self) -> std::cell::Ref<'_, HashMap<Uuid, TreeID>> {
        self.uuid_to_node.borrow()
    }

    /// Borrow the uuid → node cache for writes.
    #[cfg(not(target_arch = "wasm32"))]
    pub(crate) fn uuid_to_node_mut(&self) -> std::sync::MutexGuard<'_, HashMap<Uuid, TreeID>> {
        self.uuid_to_node.lock().unwrap()
    }
    #[cfg(target_arch = "wasm32")]
    pub(crate) fn uuid_to_node_mut(&self) -> std::cell::RefMut<'_, HashMap<Uuid, TreeID>> {
        self.uuid_to_node.borrow_mut()
    }
}
