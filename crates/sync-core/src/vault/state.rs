//! Self-contained vault types: metadata, error taxonomy, reports, sync-state,
//! and the debris/dedupe read models.
//!
//! These carry no `Vault<F>` coupling, so they live apart from the impl blocks
//! in `mod.rs`. `mod.rs` re-exports everything here (`pub use state::*`) so both
//! the flat `sync_core::*` public surface and the crate-internal
//! `crate::vault::VaultError` paths keep resolving after the split.

use crate::fs::{FileSystem, FsError};

use loro::TreeID;
use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use thiserror::Error;
use web_time::Instant;

// ========== Debug API Types ==========

/// Registry oplog statistics
#[derive(serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct RegistryStats {
    pub change_count: usize,
    pub op_count: usize,
}

/// Cheap metadata from .loro blob header (no document content access).
/// Returned by `decode_import_blob_meta()` - just parses header bytes.
#[derive(serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct BlobMeta {
    pub change_count: u32,
    pub start_timestamp: i64,
    pub end_timestamp: i64,
    pub mode: String,
    pub start_version: HashMap<String, i32>,
    pub end_version: HashMap<String, i32>,
}

/// Full document info (requires loading the document).
/// Includes content metadata that BlobMeta doesn't have.
#[derive(serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct DocumentInfo {
    pub path: String,
    pub version: HashMap<String, i32>,
    pub doc_id: Option<String>,
    pub stored_path: Option<String>,
    pub change_count: usize,
    pub op_count: usize,
    pub body_length: usize,
    pub has_frontmatter: bool,
}

/// Vault metadata file (contains VaultId and format version)
pub(crate) const METADATA_FILE: &str = ".sync/metadata.toml";

/// Current version of the .sync/ directory format.
/// Bump this when any breaking change is made to file layout, Loro schema, or naming.
pub(crate) const CURRENT_SYNC_VERSION: u32 = 1;

/// Vault-level metadata persisted in `.sync/metadata.toml`.
///
/// The `version` field represents the entire `.sync/` directory format — not just
/// this struct's schema. Migrations can move files, rewrite Loro docs, create
/// directories, etc. The version is just the marker for what state the directory is in.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct SyncMetadata {
    /// Format version of the .sync/ directory
    pub version: u32,
    /// Vault identity (gossip topic seed + mDNS mesh grouping), shared across replicas
    pub vault_id: crate::VaultId,
}

impl SyncMetadata {
    /// Create metadata for a new vault with a freshly generated VaultId.
    pub fn new() -> Self {
        Self {
            version: CURRENT_SYNC_VERSION,
            vault_id: crate::VaultId::generate(),
        }
    }
}

impl Default for SyncMetadata {
    fn default() -> Self {
        Self::new()
    }
}

impl SyncMetadata {
    /// Load metadata from `.sync/metadata.toml`, running migrations if needed.
    ///
    /// If `metadata.toml` doesn't exist, runs the v0→v1 migration (generates a new
    /// VaultId). If it does exist but has a version newer than supported, returns an error.
    /// Each migration is idempotent — safe to re-run if a previous attempt crashed.
    pub async fn load_or_migrate<F: FileSystem>(fs: &F) -> Result<Self> {
        if fs.exists(METADATA_FILE).await? {
            let bytes = fs.read(METADATA_FILE).await?;
            let content = String::from_utf8(bytes.to_vec())
                .map_err(|e| VaultError::CorruptMetadata(format!("invalid UTF-8: {}", e)))?;
            let meta: SyncMetadata = toml::from_str(&content)
                .map_err(|e| VaultError::CorruptMetadata(format!("invalid TOML: {}", e)))?;

            if meta.version > CURRENT_SYNC_VERSION {
                return Err(VaultError::VersionTooNew(
                    meta.version,
                    CURRENT_SYNC_VERSION,
                ));
            }

            tracing::info!(vault_id = %meta.vault_id, version = meta.version, "Loaded vault metadata");
            Ok(meta)
        } else {
            // v0 → v1: Generate VaultId and write metadata.toml
            tracing::info!("Running migration v0 → v1: generating VaultId");
            let meta = SyncMetadata::new();

            let toml_str = toml::to_string(&meta).map_err(|e| {
                VaultError::MetadataSerialization(format!("Failed to serialize metadata: {}", e))
            })?;
            fs.atomic_write(METADATA_FILE, toml_str.as_bytes()).await?;

            // Verify the write succeeded by re-reading. Does NOT protect against
            // concurrent writers — vault locking (Phase 2) addresses that.
            let bytes = fs.read(METADATA_FILE).await?;
            let content = String::from_utf8(bytes.to_vec())
                .map_err(|e| VaultError::CorruptMetadata(format!("invalid UTF-8: {}", e)))?;
            let written: SyncMetadata = toml::from_str(&content).map_err(|e| {
                VaultError::CorruptMetadata(format!("invalid TOML after write: {}", e))
            })?;

            tracing::info!(vault_id = %written.vault_id, "Migration v0 → v1 complete");
            Ok(written)
        }
    }

    /// Persist this metadata to `.sync/metadata.toml` (atomic write).
    ///
    /// Used both by the migration path and by VaultId adoption (`Vault::adopt_vault_id`)
    /// so the toml is serialized in exactly one place.
    pub async fn save<F: FileSystem>(&self, fs: &F) -> Result<()> {
        let toml_str = toml::to_string(self).map_err(|e| {
            VaultError::MetadataSerialization(format!("Failed to serialize metadata: {}", e))
        })?;
        fs.atomic_write(METADATA_FILE, toml_str.as_bytes()).await?;
        Ok(())
    }
}

#[derive(Debug, Error)]
pub enum VaultError {
    #[error("Filesystem error: {0}")]
    Fs(#[from] FsError),

    #[error("Document error: {0}")]
    Document(#[from] crate::document::DocumentError),

    #[error("Vault not initialized")]
    NotInitialized,

    #[error("Sync data version {0} is newer than supported (max {1}), update your client")]
    VersionTooNew(u32, u32),

    #[error("Corrupt metadata: {0}")]
    CorruptMetadata(String),

    #[error("Corrupt registry: {0}")]
    CorruptRegistry(String),

    /// Failed to serialize vault metadata (metadata.toml) for persistence.
    #[error("{0}")]
    MetadataSerialization(String),

    /// Failed to export the registry CRDT to bytes for persistence.
    #[error("{0}")]
    RegistryExport(String),

    /// Failed to import registry bytes into the in-memory registry CRDT.
    #[error("{0}")]
    RegistryImport(String),

    /// A Loro tree mutation (create/move/delete node or set node metadata) failed.
    #[error("{0}")]
    TreeOperation(String),

    /// A rename was requested but its source path has no registry node.
    #[error("{0}")]
    RenameSourceMissing(String),

    /// A rename was requested but its target path already has a registry node.
    #[error("{0}")]
    RenameTargetExists(String),

    /// A sync path failed validation (traversal, absolute, non-markdown, etc.).
    #[error("{0}")]
    InvalidPath(String),

    /// Failed to decode a Loro blob's metadata header.
    #[error("{0}")]
    BlobDecode(String),
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
    /// Disk orphans (tombstoned-in-registry `.md` files still on disk) that were
    /// moved to `.trash/`. Deliberately excluded from `has_changes()`: quarantine is
    /// local disk cleanup with no registry mutation and no sync implications, so it
    /// must not cause the daemon to broadcast.
    pub quarantined: Vec<String>,
}

impl ReconcileReport {
    /// Check if any changes were made
    ///
    /// `quarantined` is intentionally NOT consulted here — see the field doc: it is
    /// local cleanup with no sync implications, and the daemon gates broadcasts on
    /// this method.
    pub fn has_changes(&self) -> bool {
        !self.indexed.is_empty() || !self.reindexed.is_empty() || !self.moved.is_empty()
    }

    /// Total number of files processed
    pub fn total_processed(&self) -> usize {
        self.indexed.len() + self.reindexed.len() + self.moved.len()
    }
}

/// Tracks sync state for echo detection and consistency reconciliation.
///
/// When a file is received from sync, we mark it here BEFORE writing to disk.
/// When the file watcher fires, we check and consume this flag to skip broadcast.
///
/// Also tracks paths that need reconciliation before processing the next sync message.
/// This ensures loro documents match the filesystem before importing sync data.
#[derive(Clone)]
pub struct SyncState {
    /// Map of path -> timestamp when marked as synced (for echo detection)
    synced_paths: Arc<Mutex<HashMap<String, Instant>>>,
    /// Paths that may need reconciliation before next sync import
    pending_reconcile: Arc<Mutex<HashSet<String>>>,
    /// Registry may need reconciliation before next sync import
    registry_pending: Arc<Mutex<bool>>,
    /// Paths whose registry tree node is currently deleted. Used to prevent inbound
    /// DocumentUpdates from resurrecting files the local device (or a peer) has deleted.
    ///
    /// This set is derived from registry truth: `rebuild_path_cache` recomputes it from
    /// the persisted tree on every rebuild (so it survives a daemon restart), and
    /// `delete_file` inserts synchronously so a local delete guards inbound updates before
    /// the next registry sync. The two are complementary, not exclusive.
    deleted_paths: Arc<Mutex<HashSet<String>>>,
}

/// Time-to-live for sync flags. Flags older than this are considered stale.
/// Set to 30s to provide safety margin for echo detection even with delayed file watchers.
const FLAG_TTL: Duration = Duration::from_secs(30);

impl Default for SyncState {
    fn default() -> Self {
        Self {
            synced_paths: Arc::new(Mutex::new(HashMap::new())),
            pending_reconcile: Arc::new(Mutex::new(HashSet::new())),
            registry_pending: Arc::new(Mutex::new(false)),
            deleted_paths: Arc::new(Mutex::new(HashSet::new())),
        }
    }
}

impl SyncState {
    pub fn new() -> Self {
        Self::default()
    }

    /// Mark a path as having been synced (call before writing to disk).
    /// Adds to both synced_paths (for echo detection) and pending_reconcile
    /// (to ensure consistency before next sync import).
    pub fn mark_synced(&self, path: &str) {
        self.synced_paths
            .lock()
            .unwrap()
            .insert(path.to_string(), Instant::now());
        self.pending_reconcile
            .lock()
            .unwrap()
            .insert(path.to_string());
    }

    /// Check if path was synced and consume the flag (returns true once).
    /// Also removes from pending_reconcile set.
    /// Returns false if the flag has expired (older than FLAG_TTL).
    pub fn consume_synced(&self, path: &str) -> bool {
        self.pending_reconcile.lock().unwrap().remove(path);
        let mut paths = self.synced_paths.lock().unwrap();
        if let Some(timestamp) = paths.remove(path) {
            // Check if flag is still valid (not expired)
            if timestamp.elapsed() < crate::time_scale::scaled(FLAG_TTL) {
                return true;
            }
            // Flag expired - log for diagnostics
            tracing::debug!(
                "Sync flag expired for {} (age={}ms)",
                path,
                timestamp.elapsed().as_millis()
            );
        }
        false
    }

    /// Check if path was synced (without consuming).
    /// Returns false if the flag has expired.
    #[allow(dead_code)]
    pub fn is_synced(&self, path: &str) -> bool {
        let paths = self.synced_paths.lock().unwrap();
        if let Some(timestamp) = paths.get(path) {
            return timestamp.elapsed() < crate::time_scale::scaled(FLAG_TTL);
        }
        false
    }

    /// Remove expired flags to prevent memory growth.
    /// Called periodically during normal operations.
    #[allow(dead_code)]
    pub fn cleanup_expired(&self) {
        let mut paths = self.synced_paths.lock().unwrap();
        paths.retain(|_, timestamp| timestamp.elapsed() < crate::time_scale::scaled(FLAG_TTL));
    }

    /// Take all paths pending reconciliation (called before sync import).
    /// Returns the set of paths and clears the pending set.
    pub fn take_pending_reconcile(&self) -> HashSet<String> {
        std::mem::take(&mut *self.pending_reconcile.lock().unwrap())
    }

    /// Mark registry as needing reconciliation before next sync import.
    pub fn mark_registry_synced(&self) {
        *self.registry_pending.lock().unwrap() = true;
    }

    /// Check and clear registry pending flag (atomic take).
    /// Returns true if registry needs reconciliation.
    pub fn take_registry_pending(&self) -> bool {
        std::mem::take(&mut *self.registry_pending.lock().unwrap())
    }

    /// Mark a single path as deleted in the registry tree.
    ///
    /// Called synchronously by `delete_file` so an inbound DocumentUpdate arriving before
    /// the next registry sync is still guarded against resurrecting the just-deleted path.
    pub fn mark_path_deleted(&self, path: &str) {
        self.deleted_paths
            .lock()
            .unwrap()
            .insert(path.to_string());
    }

    /// Replace the entire deleted-paths set with one freshly derived from registry truth.
    ///
    /// Called by `rebuild_path_cache`, which recomputes the set from the persisted tree —
    /// so the guard survives a daemon restart, and "alive wins" (a path occupied by any
    /// alive node) is honored by construction (such paths are simply not in `paths`).
    pub fn replace_deleted_paths(&self, paths: HashSet<String>) {
        *self.deleted_paths.lock().unwrap() = paths;
    }

    /// Check whether the given path's registry tree node is currently deleted.
    ///
    /// Returns true only for paths known-deleted in the registry. Paths not in the set are
    /// treated as potentially new, which preserves create-if-new for brand-new paths.
    pub fn is_path_deleted_in_registry(&self, path: &str) -> bool {
        self.deleted_paths.lock().unwrap().contains(path)
    }
}

/// One path occupied by more than one alive file node — the cross-machine
/// parallel-index debris the dedupe tool targets. Both nodes share the same
/// `doc_id` (= path hash), so the document content is unaffected; the dedupe
/// keeps `winner` alive and tombstones the rest.
#[derive(Debug, Clone)]
pub struct DuplicateGroup {
    /// The shared path these alive nodes resolve to.
    pub path: String,
    /// The `doc_id` shared by every node in the group (= path hash, so the
    /// duplicated nodes point at the same document content).
    pub doc_id: String,
    /// Every alive TreeID at `path` (always length >= 2).
    pub alive_nodes: Vec<TreeID>,
    /// The deterministic survivor: the lowest TreeID by (peer, counter). Every
    /// machine computes the same winner from a converged registry, so the dedupe
    /// is idempotent and convergent even if run on two machines independently.
    pub winner: TreeID,
}

/// An alive file node with no backing data anywhere — a tombstone that never
/// landed. Both the `.md` at its path AND the `.sync/documents/<doc_id>.loro`
/// are absent, so it has nothing to lose if tombstoned. (If either exists it is
/// a live node, not a relic, and is left untouched.)
#[derive(Debug, Clone)]
pub struct Relic {
    /// The alive TreeID with no backing files.
    pub node: TreeID,
    /// The path the node resolves to (no `.md` exists there).
    pub path: String,
    /// The node's `doc_id` (no `.loro` exists for it either).
    pub doc_id: String,
}

/// One path occupied by more than one alive FOLDER node. Surfaced for operator
/// visibility only — folder dedupe is intentionally out of scope for v1 because
/// `LoroTree::delete` is recursive and production children are split across the
/// two folder nodes, so a naive tombstone is direct data loss. Listed, not handled.
#[derive(Debug, Clone)]
pub struct FolderDupGroup {
    /// The shared path these alive folder nodes resolve to.
    pub path: String,
    /// Every alive folder TreeID at `path` (always length >= 2).
    pub alive_nodes: Vec<TreeID>,
}

/// The result of a read-only registry-debris scan ([`crate::Vault::find_registry_debris`]).
///
/// Classifies the loro-registry debris that accumulates from cross-machine
/// parallel indexing and lost tombstones. Pure inspection — building it mutates
/// nothing; the operator reviews it before any `--apply` pass acts on it.
#[derive(Debug, Clone, Default)]
pub struct DebrisReport {
    /// Paths with more than one alive FILE node, each naming the deterministic winner.
    pub duplicate_groups: Vec<DuplicateGroup>,
    /// Alive file nodes with no `.md` and no `.loro` on disk.
    pub relics: Vec<Relic>,
    /// Paths with more than one alive FOLDER node — listed but NOT handled in v1.
    pub folder_dups: Vec<FolderDupGroup>,
}

/// The outcome of an [`crate::Vault::apply_dedupe`] pass — how much debris was tombstoned.
///
/// Counts only what the dedupe actually mutated: duplicate-group losers and relics.
/// Folder dups are out of scope for v1 and never contribute here.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct DedupeStats {
    /// How many duplicate groups had their losers tombstoned (one survivor kept each).
    pub groups_deduped: usize,
    /// Total loser nodes tombstoned across all duplicate groups (excludes each winner).
    pub nodes_tombstoned: usize,
    /// How many relic nodes were tombstoned.
    pub relics_tombstoned: usize,
}
