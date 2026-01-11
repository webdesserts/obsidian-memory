//! SyncEngine: Handles the sync protocol between peers.
//!
//! The sync protocol works as follows:
//!
//! 1. On connect, peer A sends a SyncRequest with its version vectors
//! 2. Peer B receives the request and responds with SyncExchange containing:
//!    - SyncResponse: updates A needs from B
//!    - SyncRequest: B's version vectors so A can send updates B needs
//! 3. Peer A processes the SyncExchange:
//!    - Applies updates from the response
//!    - Prepares and sends a final SyncResponse with updates B needs
//! 4. On file change, the editing peer broadcasts a DocumentUpdate to all peers
//!
//! This symmetric protocol enables full bidirectional sync in a single round-trip.

use crate::fs::FileSystem;
use crate::sync::{SyncMessage, SyncRequestData, SyncResponseData};
use crate::vault::Vault;

use std::collections::HashMap;
use thiserror::Error;
use tracing::debug;

#[derive(Debug, Error)]
pub enum SyncEngineError {
    #[error("Serialization error: {0}")]
    Serialization(String),

    #[error("Deserialization error: {0}")]
    Deserialization(String),

    #[error("Vault error: {0}")]
    Vault(#[from] crate::vault::VaultError),

    #[error("Document error: {0}")]
    Document(#[from] crate::document::DocumentError),
}

pub type Result<T> = std::result::Result<T, SyncEngineError>;

impl<F: FileSystem> Vault<F> {
    /// Prepare a sync request to send to a peer.
    ///
    /// Returns serialized bytes of a SyncRequest containing our version vectors
    /// for all known documents.
    pub async fn prepare_sync_request(&mut self) -> Result<Vec<u8>> {
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

        bincode::serialize(&msg)
            .map_err(|e| SyncEngineError::Serialization(e.to_string()))
    }

    /// Process an incoming sync message and return any outgoing response.
    ///
    /// Returns:
    /// - For SyncRequest: a SyncResponse with updates the peer is missing
    /// - For SyncResponse: applies updates and returns None
    /// - For DocumentUpdate: applies the update and returns None
    ///
    /// Also returns paths of documents that were modified.
    pub async fn process_sync_message(
        &mut self,
        data: &[u8],
    ) -> Result<(Option<Vec<u8>>, Vec<String>)> {
        let msg: SyncMessage = bincode::deserialize(data)
            .map_err(|e| SyncEngineError::Deserialization(e.to_string()))?;

        match msg {
            SyncMessage::SyncRequest {
                registry_version: _,
                document_versions,
            } => {
                // Peer is requesting sync - respond with SyncExchange (symmetric protocol)
                let exchange = self.prepare_sync_exchange(document_versions).await?;
                let exchange_bytes = bincode::serialize(&exchange)
                    .map_err(|e| SyncEngineError::Serialization(e.to_string()))?;
                Ok((Some(exchange_bytes), vec![]))
            }

            SyncMessage::SyncResponse {
                registry_updates: _,
                document_updates,
            } => {
                // Peer sent us updates (final step of exchange) - apply them
                let modified = self.apply_document_updates(document_updates).await?;
                Ok((None, modified))
            }

            SyncMessage::SyncExchange { response, request } => {
                // Peer responded to our SyncRequest with:
                // - response: updates we need from them
                // - request: their version vectors so we can send them updates
                
                debug!("SyncExchange: received {} document updates, {} version vectors", 
                    response.document_updates.len(), request.document_versions.len());
                
                // Track which files we're receiving so we don't echo them back
                let received_files: std::collections::HashSet<String> = 
                    response.document_updates.keys().cloned().collect();
                
                // First, apply their updates to us
                let modified = self.apply_document_updates(response.document_updates).await?;
                debug!("SyncExchange: modified {} files: {:?}", modified.len(), modified);
                
                // Then, prepare updates they need from us (excluding files we just received)
                let our_response = self.prepare_sync_response_data_excluding(
                    request.document_versions, 
                    &received_files
                ).await?;
                let response_msg = SyncMessage::SyncResponse {
                    registry_updates: our_response.registry_updates,
                    document_updates: our_response.document_updates,
                };
                let response_bytes = bincode::serialize(&response_msg)
                    .map_err(|e| SyncEngineError::Serialization(e.to_string()))?;
                
                Ok((Some(response_bytes), modified))
            }

            SyncMessage::DocumentUpdate { path, data } => {
                // Real-time update from peer
                let modified = self.apply_single_update(&path, &data).await?;
                Ok((None, if modified { vec![path] } else { vec![] }))
            }

            SyncMessage::FileDeleted { path } => {
                // TODO: Handle file deletion
                debug!("Received file deletion for: {}", path);
                Ok((None, vec![]))
            }
        }
    }

    /// Prepare a document update to broadcast after a file change.
    ///
    /// Returns None if the document hasn't been loaded/modified.
    pub async fn prepare_document_update(&mut self, path: &str) -> Result<Option<Vec<u8>>> {
        // Ensure document is loaded
        let doc = self.get_document(path).await?;
        
        // Export a snapshot (for now - could optimize to send incremental updates)
        let snapshot = doc.export_snapshot();

        let msg = SyncMessage::DocumentUpdate {
            path: path.to_string(),
            data: snapshot,
        };

        let bytes = bincode::serialize(&msg)
            .map_err(|e| SyncEngineError::Serialization(e.to_string()))?;

        Ok(Some(bytes))
    }

    /// Get the registry version vector as bytes.
    fn registry_version(&self) -> Vec<u8> {
        self.registry.state_vv().encode()
    }

    /// Prepare a SyncExchange in response to a SyncRequest.
    /// 
    /// This bundles:
    /// - Our response (updates they need from us)
    /// - Our request (our version vectors so they can send us updates)
    async fn prepare_sync_exchange(
        &mut self,
        their_versions: HashMap<String, Vec<u8>>,
    ) -> Result<SyncMessage> {
        // Prepare updates they need from us
        let response = self.prepare_sync_response_data(their_versions).await?;
        
        // Prepare our version vectors so they can send us updates
        let request = self.prepare_sync_request_data().await?;
        
        Ok(SyncMessage::SyncExchange { response, request })
    }

    /// Prepare sync request data (our version vectors).
    async fn prepare_sync_request_data(&mut self) -> Result<SyncRequestData> {
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
        &mut self,
        their_versions: HashMap<String, Vec<u8>>,
    ) -> Result<SyncResponseData> {
        self.prepare_sync_response_data_excluding(their_versions, &std::collections::HashSet::new()).await
    }
    
    /// Prepare sync response data, excluding specific files.
    /// 
    /// Used when responding to a SyncExchange - we exclude files we just received
    /// to avoid echoing them back. Loro's import creates a local change marker,
    /// so version-based comparison would incorrectly send updates for files
    /// we just imported.
    async fn prepare_sync_response_data_excluding(
        &mut self,
        their_versions: HashMap<String, Vec<u8>>,
        exclude: &std::collections::HashSet<String>,
    ) -> Result<SyncResponseData> {
        let mut document_updates = HashMap::new();

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
                    let updates = doc.export_updates(&their_version);
                    if !updates.is_empty() {
                        document_updates.insert(path, updates);
                    }
                }
            } else {
                // They don't have it - send full snapshot
                document_updates.insert(path, doc.export_snapshot());
            }
        }

        Ok(SyncResponseData {
            registry_updates: None, // TODO: implement registry sync
            document_updates,
        })
    }

    /// Apply document updates from a sync response.
    async fn apply_document_updates(
        &mut self,
        updates: HashMap<String, Vec<u8>>,
    ) -> Result<Vec<String>> {
        let mut modified = Vec::new();

        for (path, data) in updates {
            if self.apply_single_update(&path, &data).await? {
                modified.push(path);
            }
        }

        Ok(modified)
    }

    /// Apply a single document update.
    ///
    /// Returns true if the document was modified.
    async fn apply_single_update(&mut self, path: &str, data: &[u8]) -> Result<bool> {
        // Get or create the document
        let doc = self.get_document_mut(path).await?;
        
        // Get version before import
        let version_before = doc.version();
        
        debug!("apply_single_update: {} - data_len={}", path, data.len());
        
        // Import the update
        doc.import(data)?;
        
        // Check if version changed
        let version_after = doc.version();
        let modified = version_before != version_after;
        
        debug!("apply_single_update: {} - modified={}", path, modified);

        if modified {
            // Save the updated document to disk
            self.save_document(path).await?;
            debug!("apply_single_update: saved {} to disk", path);
        }

        Ok(modified)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fs::InMemoryFs;

    #[tokio::test]
    async fn test_sync_between_vaults_symmetric() {
        // Create two vaults with different files
        let fs1 = InMemoryFs::new();
        let fs2 = InMemoryFs::new();

        // Vault 1 has file1
        fs1.write("file1.md", b"# From Vault 1").await.unwrap();
        
        // Vault 2 has file2
        fs2.write("file2.md", b"# From Vault 2").await.unwrap();

        // Initialize both vaults (this indexes existing files)
        let mut vault1 = Vault::init(fs1, "peer1".to_string()).await.unwrap();
        let mut vault2 = Vault::init(fs2, "peer2".to_string()).await.unwrap();

        // Vault 1 sends sync request to Vault 2
        let request = vault1.prepare_sync_request().await.unwrap();

        // Vault 2 processes request and sends SyncExchange (response + its own request)
        let (exchange, _) = vault2.process_sync_message(&request).await.unwrap();
        assert!(exchange.is_some(), "Should return SyncExchange");

        // Vault 1 processes the exchange:
        // - Applies file2 from vault2
        // - Sends back SyncResponse with file1 for vault2
        let (final_response, modified1) = vault1.process_sync_message(&exchange.unwrap()).await.unwrap();
        assert!(final_response.is_some(), "Should return final SyncResponse");
        assert!(modified1.contains(&"file2.md".to_string()), "Vault1 should receive file2");

        // Vault 2 processes the final response
        let (none, modified2) = vault2.process_sync_message(&final_response.unwrap()).await.unwrap();
        assert!(none.is_none(), "No more messages needed");
        assert!(modified2.contains(&"file1.md".to_string()), "Vault2 should receive file1");

        // Verify both vaults have both files
        let doc1_in_vault2 = vault2.get_document("file1.md").await.unwrap();
        assert!(doc1_in_vault2.to_markdown().contains("From Vault 1"));

        let doc2_in_vault1 = vault1.get_document("file2.md").await.unwrap();
        assert!(doc2_in_vault1.to_markdown().contains("From Vault 2"));
    }

    #[tokio::test]
    async fn test_sync_empty_vault_receives_files() {
        // Vault 1 has files, Vault 2 is empty
        let fs1 = InMemoryFs::new();
        let fs2 = InMemoryFs::new();

        fs1.write("note1.md", b"# Note 1").await.unwrap();
        fs1.write("note2.md", b"# Note 2").await.unwrap();

        let mut vault1 = Vault::init(fs1, "peer1".to_string()).await.unwrap();
        let mut vault2 = Vault::init(fs2, "peer2".to_string()).await.unwrap();

        // Empty vault sends sync request
        let request = vault2.prepare_sync_request().await.unwrap();

        // Vault 1 responds with SyncExchange
        let (exchange, _) = vault1.process_sync_message(&request).await.unwrap();

        // Vault 2 processes exchange - should receive both files
        let (final_response, modified) = vault2.process_sync_message(&exchange.unwrap()).await.unwrap();
        
        assert!(modified.contains(&"note1.md".to_string()));
        assert!(modified.contains(&"note2.md".to_string()));
        
        // Final response exists (vault2 sends SyncResponse even if empty)
        assert!(final_response.is_some());
        
        // Vault 1 processes final response - nothing new (vault2 was empty)
        let (none, modified1) = vault1.process_sync_message(&final_response.unwrap()).await.unwrap();
        assert!(none.is_none(), "No more messages after SyncResponse");
        assert!(modified1.is_empty(), "Vault1 already had everything");
    }

    #[tokio::test]
    async fn test_document_update_broadcast() {
        let fs1 = InMemoryFs::new();
        let fs2 = InMemoryFs::new();

        let mut vault1 = Vault::init(fs1, "peer1".to_string()).await.unwrap();
        let mut vault2 = Vault::init(fs2, "peer2".to_string()).await.unwrap();

        // Create and sync initial content
        vault1.fs.write("note.md", b"Initial content").await.unwrap();
        vault1.on_file_changed("note.md").await.unwrap();

        // Full sync to get vault2 up to date
        let request = vault2.prepare_sync_request().await.unwrap();
        let (exchange, _) = vault1.process_sync_message(&request).await.unwrap();
        let (final_resp, _) = vault2.process_sync_message(&exchange.unwrap()).await.unwrap();
        if let Some(resp) = final_resp {
            vault1.process_sync_message(&resp).await.unwrap();
        }

        // Now vault1 makes a change
        vault1.fs.write("note.md", b"Updated content").await.unwrap();
        vault1.on_file_changed("note.md").await.unwrap();

        // Vault1 broadcasts a document update (real-time sync)
        let update = vault1.prepare_document_update("note.md").await.unwrap();
        assert!(update.is_some());

        // Vault2 receives the update
        let (_, modified) = vault2.process_sync_message(&update.unwrap()).await.unwrap();
        assert!(modified.contains(&"note.md".to_string()));

        // Verify content
        let doc = vault2.get_document("note.md").await.unwrap();
        assert!(doc.to_markdown().contains("Updated content"));
    }

    #[tokio::test]
    async fn test_version_includes_basic() {
        // Test the version_includes helper function with direct Loro operations
        use crate::document::NoteDocument;

        // Create a document and get its initial version
        let doc1 = NoteDocument::from_markdown("test.md", "# Hello").unwrap();
        let v1 = doc1.version().encode();

        // Create another document and import doc1's state
        let mut doc2 = NoteDocument::new("test.md");
        doc2.import(&doc1.export_snapshot()).unwrap();
        let v2 = doc2.version().encode();

        // v2 should include v1 (it has all ops from doc1)
        assert!(
            Vault::<InMemoryFs>::version_includes(&v2, &v1),
            "After import, v2 should include v1"
        );

        // Note: v1 does NOT include v2 because v2's import creates
        // operations under v2's peer ID that v1 hasn't seen.
        // This is correct Loro behavior - import adds to version vector.
    }

    #[tokio::test]
    async fn test_sync_applies_updates_correctly() {
        // Test that sync correctly applies updates without creating duplicates
        let fs1 = InMemoryFs::new();
        let fs2 = InMemoryFs::new();

        let mut vault1 = Vault::init(fs1, "peer1".to_string()).await.unwrap();
        let mut vault2 = Vault::init(fs2, "peer2".to_string()).await.unwrap();

        // Vault1 creates a file
        vault1.fs.write("note.md", b"# Original").await.unwrap();
        vault1.on_file_changed("note.md").await.unwrap();

        // Sync to vault2
        let request = vault2.prepare_sync_request().await.unwrap();
        let (exchange, _) = vault1.process_sync_message(&request).await.unwrap();
        let (_, modified) = vault2.process_sync_message(&exchange.unwrap()).await.unwrap();

        // Vault2 should have received the file
        assert!(modified.contains(&"note.md".to_string()));

        // Verify content matches
        let doc1 = vault1.get_document("note.md").await.unwrap();
        let doc2 = vault2.get_document("note.md").await.unwrap();
        assert_eq!(doc1.to_markdown(), doc2.to_markdown());

        // Apply the same sync again - should be a no-op
        let request2 = vault2.prepare_sync_request().await.unwrap();
        let (exchange2, _) = vault1.process_sync_message(&request2).await.unwrap();
        let (_, modified2) = vault2.process_sync_message(&exchange2.unwrap()).await.unwrap();

        // Nothing should be modified (already in sync)
        assert!(modified2.is_empty(), "Re-sync should not modify anything");
    }

    #[tokio::test]
    async fn test_document_update_is_idempotent() {
        // Test that receiving the same DocumentUpdate twice doesn't cause issues
        let fs1 = InMemoryFs::new();
        let fs2 = InMemoryFs::new();

        let mut vault1 = Vault::init(fs1, "peer1".to_string()).await.unwrap();
        let mut vault2 = Vault::init(fs2, "peer2".to_string()).await.unwrap();

        // Vault1 creates a file
        vault1.fs.write("note.md", b"# Content").await.unwrap();
        vault1.on_file_changed("note.md").await.unwrap();

        // Get the document update
        let update = vault1.prepare_document_update("note.md").await.unwrap().unwrap();

        // Apply to vault2 first time
        let (_, modified1) = vault2.process_sync_message(&update).await.unwrap();
        assert!(modified1.contains(&"note.md".to_string()), "First apply should modify");

        // Apply the same update again
        let (_, modified2) = vault2.process_sync_message(&update).await.unwrap();
        assert!(modified2.is_empty(), "Second apply should be no-op (idempotent)");

        // Content should still be correct
        let doc = vault2.get_document("note.md").await.unwrap();
        assert!(doc.to_markdown().contains("# Content"));
    }
}
