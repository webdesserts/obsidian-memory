//! Sync protocol for exchanging Loro document updates between peers.

use serde::{Deserialize, Serialize};
use std::collections::HashMap;

/// Messages exchanged during sync
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum SyncMessage {
    /// Request sync - send our version vectors
    SyncRequest {
        /// Version of the file registry
        registry_version: Vec<u8>,
        /// Versions of individual documents (path -> version)
        document_versions: HashMap<String, Vec<u8>>,
    },

    /// Response with updates the requester is missing
    SyncResponse {
        /// Updates to the file registry (if any)
        registry_updates: Option<Vec<u8>>,
        /// Updates to documents (path -> update data)
        document_updates: HashMap<String, Vec<u8>>,
    },

    /// Symmetric exchange: Response + Request bundled together.
    ///
    /// When peer A sends SyncRequest, peer B responds with SyncExchange containing:
    /// - response: updates A needs from B
    /// - request: B's version vectors so A can send updates B needs
    ///
    /// This enables bidirectional sync in a single round-trip.
    SyncExchange {
        /// Updates the requester is missing (same as SyncResponse)
        response: SyncResponseData,
        /// Our version vectors (same as SyncRequest) so requester can send us updates
        request: SyncRequestData,
    },

    /// Push a single document update (for real-time sync)
    DocumentUpdate {
        /// Document path
        path: String,
        /// Update data
        data: Vec<u8>,
    },

    /// Notify that a file was deleted
    FileDeleted {
        /// Document path
        path: String,
    },

    /// Notify that a file was renamed/moved
    FileRenamed {
        /// Old document path
        old_path: String,
        /// New document path
        new_path: String,
    },
}

/// Data for a sync request (version vectors)
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SyncRequestData {
    /// Version of the file registry
    pub registry_version: Vec<u8>,
    /// Versions of individual documents (path -> version)
    pub document_versions: HashMap<String, Vec<u8>>,
}

/// Data for a sync response (updates)
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SyncResponseData {
    /// Updates to the file registry (if any)
    pub registry_updates: Option<Vec<u8>>,
    /// Updates to documents (path -> update data)
    pub document_updates: HashMap<String, Vec<u8>>,
}
