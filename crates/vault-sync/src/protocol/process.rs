//! Inbound message dispatch: the seam's single entry point.
//!
//! [`Vault::process_message`] is the ONLY inbound-data path that mutates the
//! filesystem (§5), and it persists its effects before returning. It carries the
//! symmetric three-message handshake (`SyncRequest → SyncExchange → SyncResponse`)
//! and the real-time push arms (`DocUpdate`, `DocDeleted`).
//!
//! ## Registry-before-documents (INV-8) is load-bearing
//!
//! Both bulk arms apply Index updates BEFORE document updates. The Flow-2 apply gate
//! (`apply_doc.rs`) holds a brand-new document until its Index node is present, so
//! the node from THIS message must be applied first. A refactor that reorders these
//! two calls breaks the gate. The send-side guarantee that the node ships with the
//! doc is in `prepare.rs` (the full-Index-snapshot coupling).

use crate::events::SyncEvent;
use crate::fs::FileSystem;
use crate::vault::Vault;

use tracing::debug;

use super::{DocId, Result, SyncError, SyncMessage, SyncOutcome};

impl<F: FileSystem> Vault<F> {
    /// Process one inbound sync payload and return the [`SyncOutcome`] (the reply to
    /// ship back, if any, plus the documents whose materialized state changed).
    ///
    /// This is the receive seam: the consumer feeds every inbound `&[u8]` here. It is
    /// the only inbound-data path that touches the filesystem, and its effects are
    /// durable on return (it flushes the Index + each materialized document before
    /// returning) — unlike Flow-1's caller-flushed `on_file_changed`.
    pub async fn process_message(&self, data: &[u8]) -> Result<SyncOutcome> {
        // Reconcile any fs↔doc drift before importing remote data, so a merge lands
        // on a document that matches its on-disk `.md`.
        self.ensure_consistency().await?;

        self.emit(SyncEvent::MessageReceived {
            message_type: "SyncMessage".into(),
            size: data.len(),
            timestamp: self.now_ms(),
        });

        let msg: SyncMessage =
            bincode::deserialize(data).map_err(|e| SyncError::Deserialization(e.to_string()))?;

        match msg {
            SyncMessage::SyncRequest {
                catalog_digest,
                index_version,
                document_versions,
            } => {
                // The opener carries only a digest (no per-document VVs), so the no-op
                // case short-circuits in O(1) wire payload (§6.2 / OQ-C1). The
                // `ensure_consistency()` above runs FIRST so a boot-stale
                // `content_version` is repaired before the digest can match — otherwise
                // a divergent-but-unreconciled replica could falsely short-circuit
                // (§6.4).
                if catalog_digest == self.catalog_digest() {
                    // Identical merged state: nothing to transfer, the exchange ends.
                    let bytes = bincode::serialize(&SyncMessage::InSync)
                        .map_err(|e| SyncError::Serialization(e.to_string()))?;
                    return Ok(SyncOutcome {
                        reply: Some(bytes),
                        modified: vec![],
                    });
                }

                // Digests differ. The opener gave us no per-document VVs, so we can't
                // compute its minimal deltas yet — and calling the response builder with
                // the opener's EMPTY VVs would dump a FULL snapshot of every document.
                // Instead reveal OUR version vectors and let the opener drive the
                // exchange: it answers with a `SyncExchange` built from these VVs, and
                // the proven three-message handshake resumes unchanged.
                //
                // (`index_version` is intentionally unused here: the opener carries it
                // for symmetry/forward-use, but on a miss the opener — not us — drives
                // the first delta computation off the VVs we send back.)
                let _ = index_version;
                debug_assert!(
                    document_versions.is_empty(),
                    "the O(1) opener must carry no per-document VVs; the digest stands in for them"
                );
                let request = self.prepare_request_data().await?;
                let bytes = bincode::serialize(&SyncMessage::DigestMismatch { request })
                    .map_err(|e| SyncError::Serialization(e.to_string()))?;
                Ok(SyncOutcome {
                    reply: Some(bytes),
                    modified: vec![],
                })
            }

            SyncMessage::InSync => {
                // The peer reports our digests matched: nothing to apply, no reply.
                Ok(SyncOutcome::default())
            }

            SyncMessage::DigestMismatch { request } => {
                // The responder revealed its VVs after a digest miss. Build the normal
                // `SyncExchange` from them — exactly what the responder does today on a
                // VV-carrying `SyncRequest`: our updates for them + our own VVs so they
                // send us what we lack. NO digest re-check here (that would loop).
                let exchange = self
                    .prepare_sync_exchange(&request.index_version, request.document_versions)
                    .await?;
                let bytes = bincode::serialize(&exchange)
                    .map_err(|e| SyncError::Serialization(e.to_string()))?;
                Ok(SyncOutcome {
                    reply: Some(bytes),
                    modified: vec![],
                })
            }

            SyncMessage::SyncResponse {
                index_updates,
                document_updates,
            } => {
                let modified = self
                    .apply_response_updates(index_updates, document_updates)
                    .await?;
                Ok(SyncOutcome {
                    reply: None,
                    modified,
                })
            }

            SyncMessage::SyncExchange { response, request } => {
                debug!(
                    "SyncExchange: {} document updates, {} version vectors",
                    response.document_updates.len(),
                    request.document_versions.len()
                );

                // Track what we're receiving so we don't echo it back in our reply.
                let received: std::collections::HashSet<DocId> =
                    response.document_updates.keys().copied().collect();

                let modified = self
                    .apply_response_updates(response.index_updates, response.document_updates)
                    .await?;
                debug!("SyncExchange: modified {} documents", modified.len());

                // Compute and send what they're missing (excluding what we just got).
                let our_response = self
                    .prepare_response_data_excluding(
                        &request.index_version,
                        request.document_versions,
                        &received,
                    )
                    .await?;
                let response_msg = SyncMessage::SyncResponse {
                    index_updates: our_response.index_updates,
                    document_updates: our_response.document_updates,
                };
                let bytes = bincode::serialize(&response_msg)
                    .map_err(|e| SyncError::Serialization(e.to_string()))?;

                Ok(SyncOutcome {
                    reply: Some(bytes),
                    modified,
                })
            }

            SyncMessage::DocUpdate { uuid, data } => {
                let modified = self.apply_doc_update(&uuid, &data).await?;
                if modified {
                    // A live-push that materializes content can COMPLETE a collision
                    // whose other node arrived in a prior Index sync but whose body was
                    // Flow-2-gated until now: the cascade is gated on a path holding ≥2
                    // nodes, but it cannot resolve a collider whose content is still
                    // absent (DP-6 omits it from the view), so it leaves that collision
                    // pending. Firing the cascade here — on the now-merged Index +
                    // just-landed content — resolves it within this same
                    // `process_message`, so a live push honors INV-5's latency
                    // guarantee instead of waiting for the next full sync (DP-6). It is
                    // self-contained: its own cheap tree-scan gate no-ops when no path
                    // has ≥2 nodes, and it persists its own mutations. A `DocDeleted`
                    // only REMOVES a node, so it can never complete a collision and does
                    // not need this.
                    self.resolve_structural_conflicts().await?;

                    self.emit(SyncEvent::DocumentUpdated {
                        path: uuid.to_string(),
                        timestamp: self.now_ms(),
                    });
                }
                Ok(SyncOutcome {
                    reply: None,
                    modified: if modified { vec![uuid] } else { vec![] },
                })
            }

            SyncMessage::DocDeleted { uuid } => {
                debug!("Received document deletion for: {}", uuid);
                let deleted = self.apply_doc_deleted(&uuid).await?;
                if deleted {
                    self.emit(SyncEvent::FileOp {
                        operation: "delete".into(),
                        path: uuid.to_string(),
                        new_path: None,
                        timestamp: self.now_ms(),
                    });
                }
                Ok(SyncOutcome {
                    reply: None,
                    modified: if deleted { vec![uuid] } else { vec![] },
                })
            }
        }
    }

    /// Apply a response's Index updates THEN its document updates (the load-bearing
    /// INV-8 ordering), emitting `DocumentUpdated` for each modified document.
    ///
    /// Document updates for paths the Index just vacated are filtered out first (they
    /// would otherwise re-create a file at a path the Index emptied).
    async fn apply_response_updates(
        &self,
        index_updates: Option<Vec<u8>>,
        document_updates: std::collections::HashMap<DocId, Vec<u8>>,
    ) -> Result<Vec<DocId>> {
        // Index first (handles deletes/moves). The returned classification filters
        // out document updates that would resurrect a file the Index just deleted.
        let vacated = if let Some(index_data) = index_updates {
            self.apply_index_updates(&index_data).await?
        } else {
            Default::default()
        };

        // Drop document updates for DELETED documents (they would resurrect a
        // tombstoned file). MOVED documents are deliberately NOT filtered — their
        // update arrives under the same UUID and applies cleanly at the new path.
        let mut document_updates = document_updates;
        for uuid in vacated.deleted_uuids() {
            document_updates.remove(&uuid);
        }

        let modified = self.apply_doc_updates(document_updates).await?;

        // Fire the structural-conflict cascade ONCE on the fully-merged state, now that
        // BOTH the merged Index (applied above) and the colliding content (just
        // materialized by `apply_doc_updates`) are present — INV-5.0/5.3. A colliding
        // peer's content lands in this same message's document updates (INV-8
        // registry-before-documents), so firing earlier (inside `apply_index_updates`)
        // would see the node without its body. Cheap when there is no collision (a
        // tree-scan gate, no content load).
        self.resolve_structural_conflicts().await?;

        for uuid in &modified {
            self.emit(SyncEvent::DocumentUpdated {
                path: uuid.to_string(),
                timestamp: self.now_ms(),
            });
        }

        Ok(modified)
    }

    /// Reconcile fs↔document drift before importing remote data.
    ///
    /// The full reconcile (re-reading each pending `.md` and re-diffing it into its
    /// content doc so a merge lands on a document that matches its on-disk file) is
    /// the boot/consistency work that lands in the reconcile chunk (1g). In Phase 1
    /// the seam tests drive every local edit through `on_file_changed`, which keeps
    /// the cache in step with disk, so there is no out-of-band drift to reconcile and
    /// this is a no-op. It exists as the seam's documented hook so the call site and
    /// ordering (consistency BEFORE import) are in place for 1g to fill in.
    async fn ensure_consistency(&self) -> Result<()> {
        Ok(())
    }
}
