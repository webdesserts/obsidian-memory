use crate::events::SyncEvent;
use crate::fs::FileSystem;
use crate::sync::{SyncMessage, SyncRequestData, SyncResponseData};
use crate::vault::Vault;

use std::collections::HashMap;
use tracing::warn;

use super::{Result, SyncEngineError};

impl<F: FileSystem> Vault<F> {
    /// Prepare a sync request to send to a peer.
    ///
    /// Returns serialized bytes of a SyncRequest containing our version vectors
    /// for all known documents.
    pub async fn prepare_sync_request(&self) -> Result<Vec<u8>> {
        // Get registry version
        let registry_version = self.registry_version();

        // Get versions for all loaded documents
        let mut document_versions = HashMap::new();

        // Load all files to get their versions
        let files = self.list_files().await?;
        for path in files {
            // Load document if not already loaded
            let doc = self.get_document(&path).await?;
            let version = doc.version().encode();
            document_versions.insert(path, version);
        }

        let msg = SyncMessage::SyncRequest {
            registry_version,
            document_versions,
        };

        let bytes =
            bincode::serialize(&msg).map_err(|e| SyncEngineError::Serialization(e.to_string()))?;

        self.emit(SyncEvent::MessageSent {
            message_type: "SyncRequest".into(),
            size: bytes.len(),
            timestamp: self.now_ms(),
        });

        Ok(bytes)
    }

    /// Prepare a document update to broadcast after a file change.
    ///
    /// Returns None if the document hasn't been loaded/modified.
    pub async fn prepare_document_update(&self, path: &str) -> Result<Option<Vec<u8>>> {
        // Ensure document is loaded
        let doc = self.get_document(path).await?;

        // Export a snapshot (for now - could optimize to send incremental updates)
        let snapshot = doc.export_snapshot()?;

        // Get file modification time for "latest wins" conflict resolution
        let mtime = self.fs.stat(path).await.ok().map(|s| s.mtime_millis);

        let msg = SyncMessage::DocumentUpdate {
            path: path.to_string(),
            data: snapshot,
            mtime,
        };

        let bytes =
            bincode::serialize(&msg).map_err(|e| SyncEngineError::Serialization(e.to_string()))?;

        self.emit(SyncEvent::MessageSent {
            message_type: "DocumentUpdate".into(),
            size: bytes.len(),
            timestamp: self.now_ms(),
        });

        Ok(Some(bytes))
    }

    /// Prepare a file deletion message to broadcast.
    pub fn prepare_file_deleted(&self, path: &str) -> Result<Vec<u8>> {
        let msg = SyncMessage::FileDeleted {
            path: path.to_string(),
        };

        let bytes =
            bincode::serialize(&msg).map_err(|e| SyncEngineError::Serialization(e.to_string()))?;

        self.emit(SyncEvent::FileOp {
            operation: "delete".into(),
            path: path.to_string(),
            new_path: None,
            timestamp: self.now_ms(),
        });

        Ok(bytes)
    }

    /// Prepare a file renamed message to broadcast.
    pub fn prepare_file_renamed(&self, old_path: &str, new_path: &str) -> Result<Vec<u8>> {
        let msg = SyncMessage::FileRenamed {
            old_path: old_path.to_string(),
            new_path: new_path.to_string(),
        };

        let bytes =
            bincode::serialize(&msg).map_err(|e| SyncEngineError::Serialization(e.to_string()))?;

        self.emit(SyncEvent::FileOp {
            operation: "rename".into(),
            path: old_path.to_string(),
            new_path: Some(new_path.to_string()),
            timestamp: self.now_ms(),
        });

        Ok(bytes)
    }

    /// Get the registry version vector as bytes.
    fn registry_version(&self) -> Vec<u8> {
        self.registry().state_vv().encode()
    }

    /// Prepare a SyncExchange in response to a SyncRequest.
    ///
    /// This bundles:
    /// - Our response (updates they need from us)
    /// - Our request (our version vectors so they can send us updates)
    pub(super) async fn prepare_sync_exchange(
        &self,
        their_registry_version: &[u8],
        their_versions: HashMap<String, Vec<u8>>,
    ) -> Result<SyncMessage> {
        // Prepare updates they need from us
        let response = self
            .prepare_sync_response_data(their_registry_version, their_versions)
            .await?;

        // Prepare our version vectors so they can send us updates
        let request = self.prepare_sync_request_data().await?;

        Ok(SyncMessage::SyncExchange { response, request })
    }

    /// Prepare sync request data (our version vectors).
    async fn prepare_sync_request_data(&self) -> Result<SyncRequestData> {
        let registry_version = self.registry_version();
        let mut document_versions = HashMap::new();

        let files = self.list_files().await?;
        for path in files {
            let doc = self.get_document(&path).await?;
            let version = doc.version().encode();
            document_versions.insert(path, version);
        }

        Ok(SyncRequestData {
            registry_version,
            document_versions,
        })
    }

    /// Prepare sync response data (updates the peer is missing).
    async fn prepare_sync_response_data(
        &self,
        their_registry_version: &[u8],
        their_versions: HashMap<String, Vec<u8>>,
    ) -> Result<SyncResponseData> {
        self.prepare_sync_response_data_excluding(
            their_registry_version,
            their_versions,
            &std::collections::HashSet::new(),
        )
        .await
    }

    /// Prepare sync response data, excluding specific files.
    ///
    /// Used when responding to a SyncExchange - we exclude files we just received
    /// to avoid echoing them back. Loro's import creates a local change marker,
    /// so version-based comparison would incorrectly send updates for files
    /// we just imported.
    pub(super) async fn prepare_sync_response_data_excluding(
        &self,
        their_registry_version: &[u8],
        their_versions: HashMap<String, Vec<u8>>,
        exclude: &std::collections::HashSet<String>,
    ) -> Result<SyncResponseData> {
        let mut document_updates = HashMap::new();

        // Tracks whether this response ships a brand-new doc (a full snapshot for a
        // path the peer lacks). The registry node-create op for such a doc rides a
        // resend-once VV-delta, so a peer whose registry VV already "covers" the
        // op without it having landed would get the doc but not the node. When any
        // new doc ships, we force a full registry snapshot below so the node is as
        // resend-durable as its content — this is the send-side guarantee the
        // Flow-2 apply gate in `document_apply.rs` depends on (it hard-skips a new
        // doc whose node isn't present).
        let mut sent_new_doc_snapshot = false;

        // Get all our files
        let our_files = self.list_files().await?;

        for path in our_files {
            // Skip files we just received (would incorrectly appear as updates due to import marker)
            if exclude.contains(&path) {
                continue;
            }

            let doc = self.get_document(&path).await?;
            let _our_version = doc.version();

            // Check if they have this document and what version
            if let Some(their_version_bytes) = their_versions.get(&path) {
                // They have it - send updates since their version
                if let Ok(their_version) = loro::VersionVector::decode(their_version_bytes) {
                    let updates = doc.export_updates(&their_version)?;
                    if !updates.is_empty() {
                        document_updates.insert(path, updates);
                    }
                }
            } else {
                // They don't have it - send full snapshot
                document_updates.insert(path, doc.export_snapshot()?);
                sent_new_doc_snapshot = true;
            }
        }

        // Export registry updates.
        //
        // When this response ships any new-doc snapshot, send the FULL registry
        // snapshot regardless of their registry version. This closes the
        // resend-once asymmetry: documents are resent from disk until the peer
        // catches up, but registry node-creates ride a one-shot VV-delta, so a
        // doc could otherwise arrive without its node. The full snapshot (~194 KB
        // on the umbra vault, cheap on an exchange already shipping new content)
        // guarantees the node lands before the doc — exactly what the receive-side
        // gate in `document_apply.rs` relies on. loro can't cross-reference the two
        // separate docs, so this durability coupling is necessarily application-level.
        let registry_updates = if sent_new_doc_snapshot {
            // A new-doc snapshot is riding along, so the receiver's Flow-2 apply
            // gate (`document_apply.rs`) will hard-skip the doc unless its registry
            // node arrives in the SAME message. Dropping the snapshot here silently
            // (export error → `None`) produces an invisible no-sync: the doc ships
            // but never materializes on the peer until a later registry sync or boot
            // reconcile heals it. Warn so that failure is at least visible.
            match self.registry().export(loro::ExportMode::snapshot()) {
                Ok(updates) => Some(updates),
                Err(e) => {
                    warn!(
                        "prepare_sync_response: failed to export registry snapshot \
                         while shipping a new-doc snapshot; receiver will skip the new \
                         doc until its node arrives: {}",
                        e
                    );
                    None
                }
            }
        } else if !their_registry_version.is_empty() {
            if let Ok(their_version) = loro::VersionVector::decode(their_registry_version) {
                match self
                    .registry()
                    .export(loro::ExportMode::updates(&their_version))
                {
                    Ok(updates) if !updates.is_empty() => Some(updates),
                    _ => None,
                }
            } else {
                // Invalid version - send full snapshot
                self.registry().export(loro::ExportMode::snapshot()).ok()
            }
        } else {
            // They don't have registry - send full snapshot
            self.registry().export(loro::ExportMode::snapshot()).ok()
        };

        Ok(SyncResponseData {
            registry_updates,
            document_updates,
        })
    }
}
