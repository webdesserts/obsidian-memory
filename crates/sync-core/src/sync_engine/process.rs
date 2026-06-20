use crate::events::SyncEvent;
use crate::fs::FileSystem;
use crate::sync::SyncMessage;
use crate::vault::Vault;

use tracing::debug;

use super::{Result, SyncEngineError};

impl<F: FileSystem> Vault<F> {
    /// Process an incoming sync message and return any outgoing response.
    ///
    /// Returns:
    /// - For SyncRequest: a SyncResponse with updates the peer is missing
    /// - For SyncResponse: applies updates and returns None
    /// - For DocumentUpdate: applies the update and returns None
    ///
    /// Also returns paths of documents that were modified.
    pub async fn process_sync_message(
        &self,
        data: &[u8],
    ) -> Result<(Option<Vec<u8>>, Vec<String>)> {
        // Ensure consistency before processing any sync message
        self.ensure_consistency().await?;

        self.emit(SyncEvent::MessageReceived {
            message_type: "SyncMessage".into(),
            size: data.len(),
            timestamp: self.now_ms(),
        });

        let msg: SyncMessage = bincode::deserialize(data)
            .map_err(|e| SyncEngineError::Deserialization(e.to_string()))?;

        match msg {
            SyncMessage::SyncRequest {
                registry_version,
                document_versions,
            } => {
                // Peer is requesting sync - respond with SyncExchange (symmetric protocol)
                let exchange = self
                    .prepare_sync_exchange(&registry_version, document_versions)
                    .await?;
                let exchange_bytes = bincode::serialize(&exchange)
                    .map_err(|e| SyncEngineError::Serialization(e.to_string()))?;
                Ok((Some(exchange_bytes), vec![]))
            }

            SyncMessage::SyncResponse {
                registry_updates,
                document_updates,
            } => {
                // Apply registry updates first (handles deletes/renames).
                // The returned deleted_paths filters out document updates that would
                // re-create files the registry just deleted.
                //
                // This registry-before-documents ordering is load-bearing for the Flow-2
                // apply gate in `document_apply.rs`: that gate hard-skips a new doc whose
                // registry node isn't present, so the node from this same message must be
                // applied here first. A future refactor that reorders these two calls would
                // break the gate. The send-side guarantee that the node ships with the doc
                // is C3 in `prepare.rs`.
                let deleted_paths = if let Some(reg_data) = registry_updates {
                    self.apply_registry_updates(&reg_data).await?
                } else {
                    vec![]
                };
                // Skip document updates for paths deleted in the registry
                let mut document_updates = document_updates;
                for path in &deleted_paths {
                    document_updates.remove(path);
                }
                // Then apply document updates
                let modified = self.apply_document_updates(document_updates).await?;

                // Emit DocumentUpdated for each modified path
                for path in &modified {
                    self.emit(SyncEvent::DocumentUpdated {
                        path: path.clone(),
                        timestamp: self.now_ms(),
                    });
                }

                Ok((None, modified))
            }

            SyncMessage::SyncExchange { response, request } => {
                // Peer responded to our SyncRequest with:
                // - response: updates we need from them
                // - request: their version vectors so we can send them updates

                debug!(
                    "SyncExchange: received {} document updates, {} version vectors",
                    response.document_updates.len(),
                    request.document_versions.len()
                );

                // Track which files we're receiving so we don't echo them back
                let received_files: std::collections::HashSet<String> =
                    response.document_updates.keys().cloned().collect();

                // Apply registry updates first (handles deletes/renames).
                // The returned deleted_paths filters out document updates that would
                // re-create files the registry just deleted.
                //
                // As in the SyncResponse arm: registry-before-documents is load-bearing for
                // the Flow-2 apply gate in `document_apply.rs` (a new doc whose node isn't
                // present is hard-skipped, so its node must be applied here first). Don't
                // reorder these two calls.
                let deleted_paths = if let Some(reg_data) = response.registry_updates {
                    self.apply_registry_updates(&reg_data).await?
                } else {
                    vec![]
                };
                // Skip document updates for paths deleted in the registry
                let mut document_updates = response.document_updates;
                for path in &deleted_paths {
                    document_updates.remove(path);
                }

                // Then apply document updates
                let modified = self.apply_document_updates(document_updates).await?;
                debug!(
                    "SyncExchange: modified {} files: {:?}",
                    modified.len(),
                    modified
                );

                // Emit DocumentUpdated for each modified path
                for path in &modified {
                    self.emit(SyncEvent::DocumentUpdated {
                        path: path.clone(),
                        timestamp: self.now_ms(),
                    });
                }

                // Then, prepare updates they need from us (excluding files we just received)
                let our_response = self
                    .prepare_sync_response_data_excluding(
                        &request.registry_version,
                        request.document_versions,
                        &received_files,
                    )
                    .await?;
                let response_msg = SyncMessage::SyncResponse {
                    registry_updates: our_response.registry_updates,
                    document_updates: our_response.document_updates,
                };
                let response_bytes = bincode::serialize(&response_msg)
                    .map_err(|e| SyncEngineError::Serialization(e.to_string()))?;

                Ok((Some(response_bytes), modified))
            }

            SyncMessage::DocumentUpdate { path, data, mtime } => {
                // Real-time update from peer
                let modified = self.apply_single_update(&path, &data, mtime).await?;

                if modified {
                    self.emit(SyncEvent::DocumentUpdated {
                        path: path.clone(),
                        timestamp: self.now_ms(),
                    });
                }

                Ok((None, if modified { vec![path] } else { vec![] }))
            }

            SyncMessage::FileDeleted { path } => {
                // Handle file deletion via tree operation
                debug!("Received file deletion for: {}", path);

                self.emit(SyncEvent::FileOp {
                    operation: "delete".into(),
                    path: path.clone(),
                    new_path: None,
                    timestamp: self.now_ms(),
                });

                // Mark as synced BEFORE deleting (for echo detection)
                self.mark_synced(&path);
                self.delete_file(&path).await?;
                Ok((None, vec![path]))
            }

            SyncMessage::FileRenamed { old_path, new_path } => {
                // Handle file rename via tree operation
                debug!("Received file rename: {} -> {}", old_path, new_path);

                self.emit(SyncEvent::FileOp {
                    operation: "rename".into(),
                    path: old_path.clone(),
                    new_path: Some(new_path.clone()),
                    timestamp: self.now_ms(),
                });

                // Mark both paths as synced BEFORE renaming (for echo detection)
                // Some file watchers emit delete for old_path + create for new_path
                self.mark_synced(&old_path);
                self.mark_synced(&new_path);
                self.rename_file(&old_path, &new_path).await?;
                Ok((None, vec![new_path]))
            }
        }
    }
}
