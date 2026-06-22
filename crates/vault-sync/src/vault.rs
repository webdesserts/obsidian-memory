//! The `Vault<F>` handle: the crate's public entry point, tying the Index (the
//! catalog), the content docs, and the filesystem together.
//!
//! The Index owns the folder tree, the path↔uuid caches, and the per-session sync
//! state; the content docs are the per-note CRDTs addressed on disk by UUID
//! (`docs/<uuid>.loro`). The Vault is the seam between those and the filesystem: it
//! owns the `fs`, an in-memory document cache, the vault/author identities, and the
//! event bus, and it implements the flows that move data between the fs and the
//! CRDT layer.
//!
//! ## Flow-1 — local write (this chunk)
//!
//! [`Vault::on_file_changed`] is the fs-as-truth path (INV-6): a local `.md` write
//! is diffed into its content doc and the doc's `<uuid>.loro` is rewritten only if
//! the content actually changed (echo-safe). Identity is the document's UUID —
//! resolved from the Index cache for an existing note, minted once for a brand-new
//! one — and the content `.loro` filename derives from that UUID, never the path,
//! so a move (a pure-structural Index op) re-transfers zero content (INV-1). The
//! inbound/receive flow (Flow-2) and the transport seam land in a later chunk.
//!
//! ## Interior mutability (NFR-1)
//!
//! The document cache and event bus fork by target: native uses `Mutex`/`Arc`
//! (multi-threaded Tokio), wasm uses `RefCell`/`Rc` (single-threaded browser). The
//! Index forks its own internals the same way. Never hold a borrow guard across an
//! await point — the crate-level `await_holding_refcell_ref` lint guards this.

use crate::content_doc::ContentDoc;
use crate::events::{EventBus, Subscription, SyncEvent};
use crate::fs::FileSystem;
use crate::hash::content_version_fingerprint;
use crate::index::{Index, IndexError, Result, SyncMetadata, VaultId, content_doc_path};

use std::collections::HashMap;
use uuid::Uuid;

// The document cache and event bus fork by target: native uses Arc/Mutex
// (multi-threaded Tokio), wasm uses Rc/RefCell (single-threaded browser). The
// Index owns its own interior-mutability internals (and the SyncState owns an
// unconditional Arc<Mutex<…>>), so the Vault only wraps what it adds on top.
#[cfg(target_arch = "wasm32")]
use std::cell::RefCell;
#[cfg(target_arch = "wasm32")]
use std::rc::Rc;
#[cfg(not(target_arch = "wasm32"))]
use std::sync::{Arc, Mutex};

/// The `.sync/` directory holding all vault sync state.
const SYNC_DIR: &str = ".sync";

/// The per-document content `.loro` directory (`.sync/docs/`).
///
/// A move never relocates a content file (it is addressed by UUID, not path), so
/// this directory holds one `<uuid>.loro` per logical document for the vault's life.
const DOCS_DIR: &str = ".sync/docs";

/// The public vault handle.
///
/// Owns the filesystem and the document cache; delegates the catalog (tree +
/// caches + deleted-paths guard) to the [`Index`]. Mutations to the document cache
/// go through the interior-mutability accessors; reads of the catalog go through
/// the `index` field's own public API.
pub struct Vault<F: FileSystem> {
    /// The vault catalog: the folder tree, the path↔uuid caches, and the
    /// per-session sync state. Owns the index CRDT (`.sync/index.loro`).
    index: Index,

    /// In-memory cache of loaded content docs, keyed by current vault-relative
    /// path. Local bookkeeping for Flow-1 ergonomics; the wire and `SyncOutcome`
    /// are UUID-keyed (a later chunk's contract surface).
    #[cfg(target_arch = "wasm32")]
    documents: RefCell<HashMap<String, ContentDoc>>,
    #[cfg(not(target_arch = "wasm32"))]
    documents: Mutex<HashMap<String, ContentDoc>>,

    /// Filesystem abstraction.
    fs: F,

    /// Vault identity used for the gossip topic seed and mDNS mesh grouping.
    ///
    /// Shared across every replica of this vault (persisted in
    /// `.sync/metadata.toml`). It is NOT the Loro author — see `loro_author`.
    vault_id: VaultId,

    /// This device's Loro peer id, authored on every Loro operation this replica
    /// produces. Unlike `vault_id`, it is unique per device so concurrent offline
    /// edits across devices don't collide on OpIds (see [[Loro Peer ID Semantics]]).
    loro_author: u64,

    /// Event bus for sync events (native: Arc for multi-threaded Tokio).
    #[cfg(not(target_arch = "wasm32"))]
    events: Arc<EventBus>,

    /// Event bus for sync events (WASM: Rc for single-threaded browser).
    #[cfg(target_arch = "wasm32")]
    events: Rc<EventBus>,
}

impl<F: FileSystem> Vault<F> {
    // ========== Interior-mutability accessors ==========
    //
    // These borrow the document cache (RefCell on WASM, Mutex on native).
    // IMPORTANT: never hold a borrow guard across an await point — the crate-level
    // `await_holding_refcell_ref` lint catches violations.

    /// Borrow the document cache for reading (WASM).
    #[cfg(target_arch = "wasm32")]
    pub(crate) fn documents(&self) -> std::cell::Ref<'_, HashMap<String, ContentDoc>> {
        self.documents.borrow()
    }

    /// Borrow the document cache for reading (native).
    #[cfg(not(target_arch = "wasm32"))]
    pub(crate) fn documents(&self) -> std::sync::MutexGuard<'_, HashMap<String, ContentDoc>> {
        self.documents.lock().unwrap()
    }

    /// Borrow the document cache for mutation (WASM).
    #[cfg(target_arch = "wasm32")]
    pub(crate) fn documents_mut(&self) -> std::cell::RefMut<'_, HashMap<String, ContentDoc>> {
        self.documents.borrow_mut()
    }

    /// Borrow the document cache for mutation (native).
    #[cfg(not(target_arch = "wasm32"))]
    pub(crate) fn documents_mut(&self) -> std::sync::MutexGuard<'_, HashMap<String, ContentDoc>> {
        self.documents.lock().unwrap()
    }

    /// Build the Vault wrapper around a constructed Index and the vault/author
    /// identities. Factored out so `init` and `load` share the field-assembly that
    /// differs only by interior-mutability flavor.
    fn assemble(index: Index, fs: F, vault_id: VaultId, loro_author: u64) -> Self {
        #[cfg(not(target_arch = "wasm32"))]
        let events = Arc::new(EventBus::new());
        #[cfg(target_arch = "wasm32")]
        let events = Rc::new(EventBus::new());

        #[cfg(target_arch = "wasm32")]
        let this = Self {
            index,
            documents: RefCell::new(HashMap::new()),
            fs,
            vault_id,
            loro_author,
            events,
        };
        #[cfg(not(target_arch = "wasm32"))]
        let this = Self {
            index,
            documents: Mutex::new(HashMap::new()),
            fs,
            vault_id,
            loro_author,
            events,
        };
        this
    }

    /// Initialize a new vault: create `.sync/` (and `.sync/docs/`), generate and
    /// persist the VaultId, build a fresh Index, then index every existing `.md`.
    ///
    /// `loro_author` is this device's Loro peer id — every operation this replica
    /// produces is authored under it (see [[Loro Peer ID Semantics]]).
    pub async fn init(fs: F, loro_author: u64) -> Result<Self> {
        fs.mkdir(SYNC_DIR).await?;
        fs.mkdir(DOCS_DIR).await?;

        // Generate and persist the vault metadata (the VaultId).
        let metadata = SyncMetadata::load_or_migrate(&fs).await?;
        let vault_id = metadata.vault_id;

        // A fresh Index authored under the device peer id; persist it so the
        // container exists on first save.
        let index = Index::new(loro_author);
        index.save_index(&fs).await?;

        let vault = Self::assemble(index, fs, vault_id, loro_author);

        // Document every existing markdown file into the catalog.
        vault.index_existing_files().await?;

        Ok(vault)
    }

    /// Load an existing vault: read the VaultId, load the Index (hard-failing on a
    /// corrupt index — EC-9), and rebuild its caches.
    ///
    /// A corrupt index is NEVER swallowed into an empty fallback: an empty index
    /// would re-index every file with fresh UUIDs and diverge from peers. The
    /// startup paths surface this as a clean logged exit.
    ///
    /// `loro_author` is this device's Loro peer id (see [[Loro Peer ID Semantics]]).
    ///
    /// ## Boot order (INV-7 — load-bearing)
    ///
    /// Load is strictly ordered: (1) load the Index, hard-failing on corruption
    /// (EC-9); (2) rebuild the caches (both inside `Index::load_index`); (3) run the
    /// fs-first boot reconcile, documenting local fs state into the Index
    /// (adopt/quarantine/report — INV-7); and ONLY THEN does the consumer open the
    /// vault to remote sync (`process_message`). Local state is fully captured before
    /// any remote delta integrates — "commit before pull."
    pub async fn load(fs: F, loro_author: u64) -> Result<Self> {
        if !fs.exists(SYNC_DIR).await? {
            return Err(IndexError::NotInitialized);
        }

        let metadata = SyncMetadata::load_or_migrate(&fs).await?;
        let vault_id = metadata.vault_id;

        // `load_index` hard-fails on a corrupt index (EC-9) and rebuilds the caches
        // from the loaded tree.
        let index = Index::load_index(&fs, loro_author).await?;

        let vault = Self::assemble(index, fs, vault_id, loro_author);

        // Reconcile the filesystem into the Index before the vault opens to remote
        // sync. The per-file work is log-and-continue inside reconcile, so only a
        // structural failure (e.g. persisting the merged Index) propagates here.
        vault.reconcile().await?;

        Ok(vault)
    }

    /// The vault's identity (gossip topic seed and mDNS mesh grouping key).
    pub fn vault_id(&self) -> VaultId {
        self.vault_id
    }

    /// Adopt a different VaultId, rewriting `.sync/metadata.toml` to match.
    ///
    /// Used when a pairing initiator joins an existing mesh: it abandons its own
    /// freshly-generated VaultId and takes on the mesh's so it lands on the same
    /// gossip topic / mDNS group. Safe because the VaultId is purely the
    /// replica-grouping id — the per-device `loro_author` is untouched.
    ///
    /// Idempotent: a no-op when `new_id` already matches. The on-disk format
    /// `version` is preserved by re-reading the existing metadata before rewriting.
    pub async fn adopt_vault_id(&mut self, new_id: VaultId) -> Result<()> {
        if new_id == self.vault_id {
            return Ok(());
        }

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

    /// This device's Loro author identity — the per-device peer id every Loro
    /// operation is authored under (distinct from `vault_id`, which groups replicas).
    pub fn loro_author(&self) -> u64 {
        self.loro_author
    }

    /// Subscribe to sync events. Returns a `Subscription` that unsubscribes on drop.
    #[cfg(not(target_arch = "wasm32"))]
    pub fn subscribe(&self, callback: impl Fn(SyncEvent) + Send + Sync + 'static) -> Subscription {
        self.events.subscribe(callback)
    }

    /// Subscribe to sync events. Returns a `Subscription` that unsubscribes on drop.
    #[cfg(target_arch = "wasm32")]
    pub fn subscribe(&self, callback: impl Fn(SyncEvent) + 'static) -> Subscription {
        self.events.subscribe(callback)
    }

    /// Emit a sync event to all subscribers.
    ///
    /// Carried from sync-core's vault as the internal emit hook. Flow-1 does not
    /// emit (the local-write path is silent, matching the carry); the wire chunk's
    /// inbound/outbound message handlers are the first emitters, so this is unused
    /// until then.
    #[cfg_attr(not(target_arch = "wasm32"), allow(dead_code))]
    pub(crate) fn emit(&self, event: SyncEvent) {
        self.events.emit(event);
    }

    /// The Index (the vault catalog) — folder tree, caches, and deleted-paths guard.
    ///
    /// The catalog's read/mutate API lives on [`Index`]; this exposes it so callers
    /// (and a later chunk's wire) can resolve paths/uuids and drive structural ops
    /// (e.g. `move_node`) without the Vault re-wrapping every method.
    pub fn index(&self) -> &Index {
        &self.index
    }

    /// A compact whole-vault digest the compare protocol exchanges to decide whether
    /// two replicas are fully in sync in one round-trip (the P3 fast-path
    /// discriminator).
    ///
    /// Two replicas with identical merged state produce byte-equal digests; on a
    /// match the sync ends with zero further transfer, and on a miss the handshake
    /// falls through to the per-document version compare. Computed from the Index
    /// alone over its structural version and each alive document's denormalized
    /// `content_version` — no content `.loro` is opened. See
    /// [`Index::catalog_digest`].
    pub fn catalog_digest(&self) -> [u8; 32] {
        self.index.catalog_digest()
    }

    /// Read and parse a content doc by current path, loading it into the cache if
    /// absent.
    ///
    /// Returns a clone (cheap — `ContentDoc`'s `LoroDoc` is `Arc`-backed). A doc
    /// with no `<uuid>.loro` on disk and no node yet resolves to a fresh empty doc,
    /// so callers always get a usable handle.
    pub async fn get_document(&self, path: &str) -> Result<ContentDoc> {
        if let Some(doc) = self.documents().get(path).cloned() {
            return Ok(doc);
        }
        let doc = self.load_document(path).await?;
        self.documents_mut().insert(path.to_string(), doc.clone());
        Ok(doc)
    }

    /// Persist a cached content doc to disk (its `<uuid>.loro` snapshot).
    ///
    /// The materialized `.md` is the user's file, owned by the editor/watcher (the
    /// fs-as-truth side), so this writes only the CRDT snapshot under the doc's
    /// UUID. A no-op if the path isn't cached or has no resolvable UUID.
    pub async fn save_document(&self, path: &str) -> Result<()> {
        let doc = self.documents().get(path).cloned();
        let Some(doc) = doc else {
            return Ok(());
        };
        let Some(uuid) = self.resolve_uuid(path, &doc) else {
            return Ok(());
        };
        let snapshot = doc.export_snapshot()?;
        self.fs
            .atomic_write(&content_doc_path(&uuid), &snapshot)
            .await?;
        Ok(())
    }

    /// Persist the Index CRDT to `.sync/index.loro`.
    ///
    /// Must be called after a local mutation of the catalog (register/move/delete)
    /// reaches a consistent state. Tree mutations are in-memory and synchronous, so
    /// the caller decides when to flush — once per mutation, or once after a batch.
    pub async fn save_index(&self) -> Result<()> {
        self.index.save_index(&self.fs).await
    }

    /// List every markdown file in the vault (excluding `.sync` and hidden files).
    pub async fn list_files(&self) -> Result<Vec<String>> {
        let mut files = Vec::new();
        let mut dirs_to_visit = vec![String::new()]; // Start at the root.

        while let Some(dir) = dirs_to_visit.pop() {
            let entries = self.fs.list(&dir).await?;

            for entry in entries {
                let path = if dir.is_empty() {
                    entry.name.clone()
                } else {
                    format!("{}/{}", dir, entry.name)
                };

                // Skip the `.sync` directory and hidden files/dirs.
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

    /// Materialize the folder tree to disk: create a real directory for every alive
    /// folder node, and remove the directory of a tombstoned folder node when it is
    /// EMPTY (INV-1.5a — first-class empty folders).
    ///
    /// `list_files` yields only `.md` files, so an empty folder is invisible to the
    /// file reconcile — this pass is the ONLY thing that makes a tracked empty folder
    /// appear (an alive node → `mkdir`) or disappear (a tombstoned node → `rmdir`) as a
    /// directory on disk. It runs at the tail of an inbound apply and during boot
    /// reconcile, so a synced or freshly-loaded vault reflects the Index's folder set.
    ///
    /// **Removal is empty-only and never recursive (INV-3).** A tombstoned folder whose
    /// on-disk directory still holds anything — a live descendant `.md`, an untracked
    /// file, or a sub-directory — is LEFT IN PLACE: a non-empty directory means content
    /// the user (or a concurrent peer) still has there, and silently `rm -rf`-ing it
    /// would be exactly the silent loss INV-3 forbids. Only a genuinely empty directory
    /// for a tombstoned folder is removed; the check is best-effort, and a removal
    /// failure is logged, never propagated (it must not fail an apply or a load).
    ///
    /// `mkdir` is idempotent (creating an existing directory is a no-op), so re-running
    /// the pass is safe and cheap.
    pub(crate) async fn materialize_folders(&self) -> Result<()> {
        // Snapshot the folder set before any await (never hold the Index borrow across
        // an fs call). Each entry is a folder node's display path + tombstone state.
        let folders = self.index.folder_paths();

        for folder in folders {
            if folder.is_deleted {
                // A tombstoned folder: remove its directory ONLY if empty. A non-empty
                // directory holds content we must not destroy (INV-3) — skip it.
                if self.dir_is_empty(&folder.path).await {
                    if let Err(e) = self.fs.delete(&folder.path).await {
                        tracing::warn!(
                            "materialize_folders: failed to remove empty tombstoned folder {}: {}",
                            folder.path,
                            e
                        );
                    } else {
                        tracing::debug!(
                            "materialize_folders: removed empty folder {}",
                            folder.path
                        );
                    }
                }
            } else {
                // An alive folder node materializes as a real directory (idempotent).
                if let Err(e) = self.fs.mkdir(&folder.path).await {
                    tracing::warn!(
                        "materialize_folders: failed to mkdir alive folder {}: {}",
                        folder.path,
                        e
                    );
                }
            }
        }

        Ok(())
    }

    /// Whether the on-disk directory at `path` exists and contains no entries.
    ///
    /// Used by [`Self::materialize_folders`] to gate the removal of a tombstoned
    /// folder: a directory that does not exist (already gone) is not "empty" to remove,
    /// and a directory that still holds anything must be preserved (INV-3). A `list`
    /// error (e.g. the path is not a directory, or vanished mid-pass) is treated as
    /// "not safe to remove" → `false`.
    async fn dir_is_empty(&self, path: &str) -> bool {
        match self.fs.list(path).await {
            Ok(entries) => entries.is_empty(),
            Err(_) => false,
        }
    }

    /// Document every existing markdown file into the catalog (Flow-1 over each).
    ///
    /// Called at `init` so every file is tracked before any sync. Persists the
    /// catalog once at the end (one write, not one per file). A single bad file is
    /// logged and skipped rather than failing the whole scan.
    async fn index_existing_files(&self) -> Result<()> {
        let files = self.list_files().await?;
        let mut any_registered = false;

        for path in files {
            let was_unregistered = self.index.node_for_path(&path).is_none();
            match self.on_file_changed(&path).await {
                Ok(_) => {
                    if was_unregistered && self.index.node_for_path(&path).is_some() {
                        any_registered = true;
                    }
                }
                Err(e) => {
                    tracing::warn!("Failed to index file {}: {}", path, e);
                }
            }
        }

        // Batch: one catalog write for all registrations.
        if any_registered {
            self.save_index().await?;
        }

        Ok(())
    }

    /// Load a content doc from disk by current path.
    ///
    /// Prefers the `<uuid>.loro` (the CRDT state) when the path resolves to a node
    /// whose UUID has a content file; falls back to the on-disk `.md`; and for a
    /// genuinely new path returns a fresh empty doc (so it gets a minted UUID).
    async fn load_document(&self, path: &str) -> Result<ContentDoc> {
        // A known path resolves to a UUID; load its content `.loro` if present.
        if let Some(uuid) = self.uuid_for_path(path) {
            let loro_path = content_doc_path(&uuid);
            if self.fs.exists(&loro_path).await? {
                let bytes = self.fs.read(&loro_path).await?;
                return Ok(ContentDoc::from_bytes(&bytes, self.loro_author)?);
            }
        }

        // Otherwise materialize from the on-disk markdown (mints a fresh UUID).
        if self.fs.exists(path).await? {
            let bytes = self.fs.read(path).await?;
            let content = String::from_utf8_lossy(&bytes);
            return Ok(ContentDoc::from_markdown(&content, self.loro_author)?);
        }

        // Brand-new document: an empty doc with a minted UUID.
        Ok(ContentDoc::from_markdown("", self.loro_author)?)
    }

    /// Resolve the UUID for a path via the Index cache (a live node's UUID meta).
    pub(crate) fn uuid_for_path(&self, path: &str) -> Option<Uuid> {
        let node = self.index.node_for_path(path)?;
        self.index.node_uuid(&node)
    }

    /// The filesystem this vault is bound to (for the protocol's apply paths).
    pub(crate) fn fs(&self) -> &F {
        &self.fs
    }

    /// Mark a path as synced before writing it to disk (echo detection).
    ///
    /// The inbound apply paths call this so the local file watcher, firing on the
    /// write the apply just made, recognizes it as an echo and does not re-broadcast.
    pub(crate) fn mark_synced(&self, path: &str) {
        self.index.sync_state.mark_synced(path);
    }

    /// Resolve the UUID for a (path, doc) pair: prefer the Index node's UUID, else
    /// fall back to the doc's own minted `_meta.doc_id`.
    ///
    /// The Index is authoritative for an indexed note; the doc's own id covers a
    /// freshly-minted doc not yet registered.
    fn resolve_uuid(&self, path: &str, doc: &ContentDoc) -> Option<Uuid> {
        self.uuid_for_path(path)
            .or_else(|| doc.doc_id().and_then(|s| Uuid::parse_str(&s).ok()))
    }

    /// Flow-1 — handle a local `.md` write (the fs-as-truth path, INV-6).
    ///
    /// Diffs the file content into its content doc, rewriting the `<uuid>.loro`
    /// **only when the content actually changed** (echo-safe — returns `false` on a
    /// no-op edit so callers can gate broadcasts). A real change also bumps the
    /// node's denormalized `content_version`. For a brand-new path the doc is
    /// minted, its `<uuid>.loro` written, and a node registered under that UUID; the
    /// content `.loro` filename derives from the UUID, never the path, so a later
    /// move re-transfers zero content (INV-1).
    ///
    /// Identity is resolved from the Index for an existing note, or minted for a new
    /// one. Stored Loro bodies are outputs of a prior `markdown::parse` (which
    /// strips leading newlines), the invariant that makes the round-trip echo-stable.
    pub async fn on_file_changed(&self, path: &str) -> Result<bool> {
        // Skip non-markdown files and anything under `.sync`.
        if !path.ends_with(".md") || path.starts_with(SYNC_DIR) {
            return Ok(false);
        }

        let bytes = self.fs.read(path).await?;
        let content = String::from_utf8_lossy(&bytes);
        let parsed = crate::markdown::parse(&content);

        // Does a content doc already exist for this path — in the cache, or as a
        // `<uuid>.loro` on disk for a node that resolves from the path?
        let cached = self.documents().get(path).cloned();
        let on_disk_uuid = match &cached {
            Some(_) => None,
            None => self.uuid_for_path(path),
        };

        if let Some(doc) = cached {
            // Cached: diff-and-merge into the live doc.
            return self.apply_local_edit(path, &doc, &parsed).await;
        }

        if let Some(uuid) = on_disk_uuid {
            let loro_path = content_doc_path(&uuid);
            if self.fs.exists(&loro_path).await? {
                // Cold cache: load the doc from its `<uuid>.loro`, then diff-merge.
                let loro_bytes = self.fs.read(&loro_path).await?;
                let doc = ContentDoc::from_bytes(&loro_bytes, self.loro_author)?;
                let changed = self.apply_local_edit(path, &doc, &parsed).await?;
                self.documents_mut().insert(path.to_string(), doc);
                return Ok(changed);
            }
        }

        // No doc anywhere for this path: create (or, in a later chunk, ADOPT an
        // orphaned `<uuid>.loro` from a native delete+create move). Today: mint a
        // fresh UUID. The seam — `resolve_create_uuid` — is where 1g hooks adopt in.
        self.create_document(path, &content).await
    }

    /// The create/adopt seam for a brand-new path (Flow-1 step 3).
    ///
    /// Builds a content doc from the file's markdown (minting a fresh UUID via
    /// `ContentDoc::from_markdown`), writes its `<uuid>.loro`, registers a node
    /// under that UUID with an initial `content_version`, and caches the doc.
    ///
    /// For now this always mints a new identity. Chunk 1g promotes this into the
    /// orphan-adopt path: a native move arrives as `delete(old)` + `create(new)`,
    /// and the create can re-attach an orphaned doc's UUID instead of minting. The
    /// seam is deliberately narrow so that hook is a localized change here.
    async fn create_document(&self, path: &str, content: &str) -> Result<bool> {
        let doc = ContentDoc::from_markdown(content, self.loro_author)?;
        let uuid = doc
            .doc_id()
            .and_then(|s| Uuid::parse_str(&s).ok())
            .ok_or_else(|| {
                IndexError::Document(crate::content_doc::DocumentError::Loro(
                    "freshly minted content doc has no parseable doc_id".into(),
                ))
            })?;

        let snapshot = doc.export_snapshot()?;
        self.fs
            .atomic_write(&content_doc_path(&uuid), &snapshot)
            .await?;

        // Register the node under the doc's UUID with an initial version fingerprint.
        let fingerprint = content_version_fingerprint(&doc.version());
        self.index.register_document(path, &uuid, &fingerprint)?;

        self.documents_mut().insert(path.to_string(), doc);
        tracing::debug!("Created new document: {} ({})", path, uuid);
        Ok(true)
    }

    /// Diff-merge a parsed `.md` into an existing content doc, persisting and
    /// bumping `content_version` only when the content genuinely changed.
    ///
    /// Returns `true` iff the body or frontmatter actually changed (echo-safe). On a
    /// real change: commit the doc, rewrite its `<uuid>.loro`, cache it, and refresh
    /// the node's `content_version` fingerprint (the derived digest cache). The
    /// catalog flush (`save_index`) is the caller's call — it's batched at `init`
    /// and a single mutation elsewhere.
    async fn apply_local_edit(
        &self,
        path: &str,
        doc: &ContentDoc,
        parsed: &crate::markdown::ParsedMarkdown,
    ) -> Result<bool> {
        let body_changed = doc.update_body(&parsed.body)?;
        let fm_changed = doc.update_frontmatter(parsed.frontmatter.as_ref())?;
        let changed = body_changed || fm_changed;

        if !changed {
            tracing::debug!("No changes detected (echo): {}", path);
            return Ok(false);
        }

        doc.commit();

        // Resolve the doc's UUID (Index-authoritative, with the doc's own id as a
        // fallback for an as-yet-unregistered doc) and persist under it.
        let uuid = self.resolve_uuid(path, doc).ok_or_else(|| {
            IndexError::Document(crate::content_doc::DocumentError::Loro(format!(
                "edited document at {} has no resolvable UUID",
                path
            )))
        })?;

        let snapshot = doc.export_snapshot()?;
        self.fs
            .atomic_write(&content_doc_path(&uuid), &snapshot)
            .await?;
        self.documents_mut().insert(path.to_string(), doc.clone());

        // Refresh the denormalized content_version on the node (if one exists).
        if let Some(node) = self.index.node_for_path(path) {
            let fingerprint = content_version_fingerprint(&doc.version());
            self.index.set_content_version(&node, &fingerprint)?;
        }

        tracing::debug!("Updated document via diff: {}", path);
        Ok(changed)
    }
}
