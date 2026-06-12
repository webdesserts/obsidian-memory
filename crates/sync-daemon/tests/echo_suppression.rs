/// Tests for the sync-flag echo suppression fix.
///
/// Before the fix, `on_file_modified` and `on_file_deleted` early-returned when
/// `vault.consume_sync_flag(path)` was true. A stale-armed flag (armed by an
/// inbound sync with no follow-up watcher event; TTL 30s) caused a real local
/// edit to skip `vault.on_file_changed` — leaving a stale .loro — and a real
/// local delete to produce no registry tombstone or broadcast.
///
/// The fix: remove both early-returns. Always apply the vault primitive (they are
/// echo-safe/idempotent). `Vault::on_file_changed` now returns `Result<bool>`
/// indicating whether the document body or frontmatter actually changed; the
/// daemon gates its broadcast on that bool. `Vault::delete_file` similarly
/// returns `Result<bool>` (true when a live node was tombstoned).
///
/// These tests use NativeFs on a `tempfile::tempdir` to match the production
/// codepath (InMemoryFs masks the class of bug the prior ENOENT fix addressed).
mod echo_suppression {
    use std::sync::Arc;

    use sync_core::fs::FileSystem;
    use sync_core::peer_id::PeerId;
    use sync_core::Vault;
    use sync_daemon::NativeFs;
    use tempfile::tempdir;

    /// Deterministic author seed.
    fn test_author() -> PeerId {
        PeerId::from_secret_bytes([99u8; 32])
    }

    // ── on_file_changed return value ──────────────────────────────────────────

    /// `on_file_changed` returns `true` when the file body has changed.
    #[tokio::test]
    async fn on_file_changed_returns_true_when_body_changes() {
        let dir = tempdir().expect("tempdir");
        let fs = Arc::new(NativeFs::new(dir.path().to_path_buf()));
        let vault = Vault::init(fs.clone(), test_author())
            .await
            .expect("vault init");

        // Create and index the file with initial content.
        fs.write("note.md", b"# Original").await.expect("write");
        let first = vault
            .on_file_changed("note.md")
            .await
            .expect("first on_file_changed");
        // New document creation always counts as a change.
        assert!(first, "on_file_changed should return true for a new document");

        // Now update the file content on disk and call on_file_changed again.
        fs.write("note.md", b"# Modified").await.expect("overwrite");
        let second = vault
            .on_file_changed("note.md")
            .await
            .expect("second on_file_changed");
        assert!(second, "on_file_changed should return true when body changes");
    }

    /// Calling `on_file_changed` twice with identical content returns `false`
    /// the second time — the echo-safe diff detects no delta.
    ///
    /// This is what makes the new broadcast-gating safe: if the OS watcher fires
    /// a spurious event after sync writes the file, the second call is a no-op
    /// and `changed = false` suppresses the broadcast.
    #[tokio::test]
    async fn on_file_changed_returns_false_for_unchanged_echo() {
        let dir = tempdir().expect("tempdir");
        let fs = Arc::new(NativeFs::new(dir.path().to_path_buf()));
        let vault = Vault::init(fs.clone(), test_author())
            .await
            .expect("vault init");

        fs.write("note.md", b"# Stable content").await.expect("write");

        // Index the file for the first time.
        let first = vault
            .on_file_changed("note.md")
            .await
            .expect("first on_file_changed");
        assert!(first, "on_file_changed should return true for a new document");

        // Call again with identical disk content — no diff → false.
        let second = vault
            .on_file_changed("note.md")
            .await
            .expect("second on_file_changed with same content");
        assert!(
            !second,
            "on_file_changed should return false when content is unchanged (echo suppression)"
        );
    }

    /// Proves the Vault primitive is flag-agnostic: `mark_synced` has no effect on
    /// `on_file_changed`, which always applies the edit and advances the .loro snapshot
    /// on disk. The daemon-handler regression (early-return swallowing real edits) is
    /// covered by `test_local_edit_during_armed_flag_propagates_to_peer` in
    /// `daemon_integration.rs`.
    ///
    /// The on-disk assertion uses a cold `Vault::load` over the same tempdir — this
    /// exercises the real NativeFs read path and confirms the .loro snapshot persisted,
    /// not just that the in-memory cache advanced.
    #[tokio::test]
    async fn local_edit_during_armed_flag_still_updates_loro() {
        let dir = tempdir().expect("tempdir");
        let fs = Arc::new(NativeFs::new(dir.path().to_path_buf()));
        let vault = Vault::init(fs.clone(), test_author())
            .await
            .expect("vault init");

        // Index the file with initial content.
        fs.write("note.md", b"# Initial").await.expect("write initial");
        vault
            .on_file_changed("note.md")
            .await
            .expect("index initial");

        // Arm the sync flag — simulates the lingering-flag window after inbound sync.
        vault.mark_synced("note.md");

        // Edit the file on disk.
        fs.write("note.md", b"# Edited after sync").await.expect("write edit");

        // The edit must be applied regardless of the armed flag.
        let changed = vault
            .on_file_changed("note.md")
            .await
            .expect("on_file_changed after edit");
        assert!(
            changed,
            "on_file_changed must return true for a real edit even when sync flag was armed"
        );

        // Drop the first vault and open a cold second vault over the same tempdir.
        // This proves the .loro snapshot was actually persisted to disk — not just
        // that the in-memory document cache advanced.
        drop(vault);
        let cold_vault = Vault::load(fs, test_author())
            .await
            .expect("cold vault load");
        let doc = cold_vault
            .get_document("note.md")
            .await
            .expect("get document from cold vault");
        let body_text = doc.body().to_string();
        assert!(
            body_text.contains("Edited after sync"),
            "cold vault must see the edited content — .loro snapshot must have been persisted; got: {:?}",
            body_text
        );
    }
}
