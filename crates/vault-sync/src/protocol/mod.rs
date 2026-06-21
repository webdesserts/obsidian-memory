//! The transport seam (§5) and the sync protocol: the UUID-keyed wire, the
//! symmetric three-message handshake, the receive flow (Flow-2), and the
//! real-time push primitives.
//!
//! ## The byte seam
//!
//! A consumer (the sync daemon) drives sync purely through bytes: it calls
//! [`Vault::prepare_request`] to start a handshake, ships the bytes to a peer over
//! whatever transport it owns, and feeds every inbound payload to
//! [`Vault::process_message`]. The protocol knows nothing about iroh, sockets, or
//! relays — only `&[u8]` in and [`SyncOutcome`] out. `process_message` is the ONLY
//! inbound-data path that mutates the filesystem (§5), and it persists its effects
//! before returning (effects durable on return — unlike Flow-1's caller-flushed
//! `on_file_changed`).
//!
//! ## UUID identity on the wire (the headline change vs the old path-keyed sync)
//!
//! Every document on the wire is keyed by its [`DocId`] (a UUID newtype), never by
//! path. A move is therefore a pure-structural Index op (`tree.mov`) carried in the
//! Index CRDT delta — there is no separate rename message, and a moved document
//! re-transfers zero content (INV-1). Because identity is the UUID, a same-document
//! merge is always a normal CRDT merge (INV-2); the old "silent latest-wins"
//! divergence path (which keyed on path and resolved with machine-local mtime) is
//! gone entirely.
//!
//! ## Registry-before-documents (INV-8)
//!
//! Both handshake arms apply Index updates BEFORE document updates. This ordering
//! is load-bearing for the Flow-2 apply gate: a brand-new document can only
//! materialize once its Index node has arrived (no node ⇒ no resolved path ⇒
//! nothing to materialize), so the node from the same message must land first.

mod apply_doc;
mod apply_index;
mod prepare;
mod process;

use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use thiserror::Error;
use uuid::Uuid;

/// A document's stable identity on the wire: the UUID minted in its content doc's
/// `_meta.doc_id` and recorded verbatim in its Index node's `uuid` meta.
///
/// Keying the wire on this (rather than on a path or a path-hash) is what makes a
/// move zero-content (INV-1) and a same-document merge always a normal CRDT merge
/// (INV-2). Wraps a [`Uuid`]; serialized transparently as the UUID's bytes so the
/// wire stays compact.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct DocId(pub Uuid);

impl DocId {
    /// The underlying UUID.
    pub fn uuid(&self) -> Uuid {
        self.0
    }
}

impl From<Uuid> for DocId {
    fn from(uuid: Uuid) -> Self {
        Self(uuid)
    }
}

impl std::fmt::Display for DocId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.0)
    }
}

/// Messages exchanged during sync.
///
/// Every document-bearing variant is keyed by [`DocId`] (UUID), not path. There is
/// deliberately NO rename message: a move rides the Index CRDT delta as a
/// `tree.mov` op (design §5.5). There is deliberately NO mtime field: conflict
/// resolution is content-based (INV-5), never machine-local-clock-based.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum SyncMessage {
    /// Open a sync: send our version vectors so the peer can compute what we lack.
    SyncRequest {
        /// Version vector of the Index CRDT.
        index_version: Vec<u8>,
        /// Per-document version vectors, keyed by document UUID.
        document_versions: HashMap<DocId, Vec<u8>>,
    },

    /// Updates the requester is missing (the tail of the handshake).
    SyncResponse {
        /// Index CRDT updates (a delta or a full snapshot), if any.
        index_updates: Option<Vec<u8>>,
        /// Per-document updates, keyed by document UUID.
        document_updates: HashMap<DocId, Vec<u8>>,
    },

    /// Symmetric exchange: a response plus our own request, bundled.
    ///
    /// When peer A sends a `SyncRequest`, peer B answers with `SyncExchange`
    /// carrying both the updates A needs from B AND B's own version vectors, so A
    /// can compute and send what B needs in one round-trip.
    SyncExchange {
        /// Updates the requester is missing (same shape as `SyncResponse`).
        response: SyncResponseData,
        /// Our version vectors (same shape as `SyncRequest`) so the requester can
        /// send us what we're missing.
        request: SyncRequestData,
    },

    /// Push a single document update (real-time sync after a local edit).
    ///
    /// Carries no node — a lone push for a brand-new document is hard-skipped by
    /// the receiver's Flow-2 gate and recovered by the next full sync or boot
    /// reconcile (S5; this is correct, not a bug).
    DocUpdate {
        /// The document's UUID identity.
        uuid: DocId,
        /// The document update (a snapshot or an incremental delta).
        data: Vec<u8>,
    },

    /// Notify that a document was deleted.
    DocDeleted {
        /// The document's UUID identity.
        uuid: DocId,
    },
}

/// The version-vector half of a sync request.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SyncRequestData {
    /// Version vector of the Index CRDT.
    pub index_version: Vec<u8>,
    /// Per-document version vectors, keyed by document UUID.
    pub document_versions: HashMap<DocId, Vec<u8>>,
}

/// The updates half of a sync response.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SyncResponseData {
    /// Index CRDT updates (a delta or a full snapshot), if any.
    pub index_updates: Option<Vec<u8>>,
    /// Per-document updates, keyed by document UUID.
    pub document_updates: HashMap<DocId, Vec<u8>>,
}

/// The result of feeding one inbound payload to [`Vault::process_message`].
///
/// `reply` is the bytes to ship back to the peer (the next handshake message), or
/// `None` when this payload terminates the exchange. `modified` lists the documents
/// whose materialized state changed, so the caller can react (re-render, notify)
/// without re-scanning.
#[derive(Debug, Default)]
pub struct SyncOutcome {
    /// The next message to send back to the peer, if the handshake continues.
    pub reply: Option<Vec<u8>>,
    /// Documents whose materialized state changed as a result of this payload.
    pub modified: Vec<DocId>,
}

/// Errors raised while preparing or processing sync messages.
#[derive(Debug, Error)]
pub enum SyncError {
    #[error("Serialization error: {0}")]
    Serialization(String),

    #[error("Deserialization error: {0}")]
    Deserialization(String),

    #[error("Filesystem error: {0}")]
    Fs(#[from] crate::fs::FsError),

    #[error("Index error: {0}")]
    Index(#[from] crate::index::IndexError),

    #[error("Document error: {0}")]
    Document(#[from] crate::content_doc::DocumentError),
}

/// Result alias for the sync protocol.
pub type Result<T> = std::result::Result<T, SyncError>;

use crate::fs::FileSystem;
use crate::index::content_doc_path;
use crate::vault::Vault;

impl<F: FileSystem> Vault<F> {
    /// Current timestamp in milliseconds since the Unix epoch (for sync events).
    ///
    /// `web-time` so it works on both native and wasm; an error clock reads as 0.0,
    /// which only affects event telemetry, never sync correctness.
    pub(crate) fn now_ms(&self) -> f64 {
        web_time::SystemTime::now()
            .duration_since(web_time::SystemTime::UNIX_EPOCH)
            .map(|d| d.as_secs_f64() * 1000.0)
            .unwrap_or(0.0)
    }

    /// Resolve a [`DocId`] to its current vault-relative path via the Index caches.
    ///
    /// `uuid → node → path`. Returns `None` when no Index node carries that UUID —
    /// the structural Flow-2 gate: a document we have no node for cannot be located,
    /// so it cannot be materialized (its update is held until the node arrives).
    pub(crate) fn path_for_doc(&self, uuid: &DocId) -> Option<String> {
        let node = self.index().find_node_by_uuid(&uuid.0)?;
        self.index().path_for_node(&node)
    }

    /// The on-disk content `.loro` path for a [`DocId`].
    pub(crate) fn doc_content_path(uuid: &DocId) -> String {
        content_doc_path(&uuid.0)
    }
}
