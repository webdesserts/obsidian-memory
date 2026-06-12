/// Integration test: ENOENT during reconcile must not abort the inbound batch.
///
/// Background: `NativeFs` maps every IO error to `FsError::Io`, but the
/// skip-deleted-file guard in `ensure_consistency` only matches
/// `FsError::NotFound`. On native vaults, an ENOENT during `reconcile_single`
/// of a path that was deleted between `mark_synced` and the next
/// `process_sync_message` call aborts the entire message — including registry
/// deletions for unrelated files in the same batch. `InMemoryFs` returns proper
/// `NotFound`, which is why existing tests are green (test/prod divergence).
///
/// Production error string:
///   "Failed to process pulled change from <peer>: Vault error: Filesystem
///    error: IO error: No such file or directory (os error 2)"
mod reconcile_missing_file {
    use std::sync::Arc;

    use sync_core::fs::FileSystem;
    use sync_core::{SyncMessage, Vault};
    use sync_core::peer_id::PeerId;
    use sync_daemon::NativeFs;
    use tempfile::tempdir;

    /// Deterministic author seed.
    fn test_author() -> PeerId {
        PeerId::from_secret_bytes([42u8; 32])
    }

    /// Reproduce the ENOENT-aborts-inbound-batch bug on NativeFs vaults.
    ///
    /// Steps:
    /// 1. Create two real markdown files, init/index the vault so both have
    ///    loro docs (reconcile_single needs a .loro file to read).
    /// 2. Mark `victim.md` synced + pending-reconcile, then delete it from
    ///    disk directly — simulating the FileDeleted handler's sequence.
    /// 3. Feed an inbound `FileDeleted { path: "keep.md" }` message.
    /// 4. Assert `process_sync_message` returns `Ok` AND `keep.md` was removed
    ///    from the vault's registry.
    ///
    /// Pre-fix: step 3 returns `Err("… IO error: No such file or directory
    /// (os error 2)")` before the message is even deserialized — the entire
    /// batch including the `keep.md` registry deletion is silently dropped.
    #[tokio::test]
    async fn process_sync_message_survives_missing_reconcile_path() {
        let dir = tempdir().expect("tempdir");
        let fs = Arc::new(NativeFs::new(dir.path().to_path_buf()));
        let vault = Vault::init(fs.clone(), test_author())
            .await
            .expect("vault init");

        // Create both files and index them so each has a .loro doc on disk.
        fs.write("keep.md", b"# Keep").await.expect("write keep.md");
        fs.write("victim.md", b"# Victim").await.expect("write victim.md");

        vault
            .on_file_changed("keep.md")
            .await
            .expect("index keep.md");
        vault
            .on_file_changed("victim.md")
            .await
            .expect("index victim.md");

        // Simulate the FileDeleted handler: mark the path synced (which also
        // inserts it into pending_reconcile), then remove it from disk. The
        // next call to ensure_consistency will try reconcile_single("victim.md")
        // and hit ENOENT.
        vault.mark_synced("victim.md");
        std::fs::remove_file(dir.path().join("victim.md"))
            .expect("remove victim.md from disk");

        // Build an inbound FileDeleted message for keep.md. SyncMessage is
        // publicly constructible from outside sync-core.
        let msg = SyncMessage::FileDeleted {
            path: "keep.md".to_string(),
        };
        let msg_bytes = bincode::serialize(&msg).expect("serialize");

        // Pre-fix: returns Err("… IO error: No such file or directory (os error 2)")
        // before keep.md's deletion is processed — the whole batch is dropped.
        let (_, modified) = vault
            .process_sync_message(&msg_bytes)
            .await
            .expect(
                "process_sync_message must not abort due to ENOENT on a pending-reconcile \
                 path (pre-fix: Vault error: Filesystem error: IO error: No such file or \
                 directory (os error 2))"
            );

        // The inbound FileDeleted for keep.md must have applied: the vault reports
        // keep.md in the modified paths list. `delete_file` removes only the CRDT
        // node and `.loro` doc; physical `.md` removal is the daemon's responsibility
        // after processing, so asserting the file is gone from disk would be wrong here.
        assert!(
            modified.contains(&"keep.md".to_string()),
            "keep.md should appear in modified paths after the inbound FileDeleted \
             message applied; got: {:?}",
            modified
        );
    }
}
