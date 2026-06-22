//! Boot-time move crash-recovery helpers (P4f-2b-ii).
//!
//! At daemon startup, the move-coalescer's crash-recovery journal
//! (`.sync/pending-moves.json`, written by P4f-2a) may hold buffered create/delete
//! halves of a native move that was in-flight when a previous run crashed. These
//! free functions own the journal side of recovery: read the journal, map its
//! DELETE records into the [`JournalReStitch`] inputs that `Vault::load_with_journal`
//! feeds into boot reconcile, and (after recovery completes) rewrite the journal
//! empty so the next boot recovers nothing.
//!
//! They are free functions in their own module — not `Daemon`/`Vault` methods — so
//! the recovery sequence is both production-correct (called from `startup_inner`)
//! and integration-testable (the test harness's `spawn_daemon_from_loaded` calls the
//! SAME functions, so production and test cannot diverge). The detect-and-tombstone
//! half that needs the loaded vault lives on `Daemon::finalize_recovered_journal`,
//! since `on_file_deleted` is a `&mut self` daemon sink.
//!
//! Exposed `pub` for the boot-recovery integration harness, mirroring the existing
//! `Daemon::set_fs` test-wiring precedent.

use crate::move_coalescer::{
    JournaledMove, PENDING_MOVES_FILE, PENDING_MOVES_VERSION, PendingKind, PendingMovesFile,
};
use tracing::{error, warn};
use uuid::Uuid;
use vault_sync::JournalReStitch;
use vault_sync::fs::FileSystem;

/// Read the move-coalescer's crash-recovery journal, tolerantly.
///
/// Recovery must NEVER abort boot, so every failure mode degrades to "no pending
/// moves" rather than propagating: an absent file, a corrupt/torn body that won't
/// deserialize, and a version mismatch all return `vec![]` (the mismatch and the
/// corrupt cases `warn!` first, since they signal a real on-disk problem worth a log
/// line; an absent file is the normal clean-boot case and stays quiet). This mirrors
/// the 2a write-side posture — the journal is an optimization for crash recovery, not
/// a source of truth whose loss can block the daemon.
pub async fn read_pending_journal<FS: FileSystem>(fs: &FS) -> Vec<JournaledMove> {
    let bytes = match fs.read(PENDING_MOVES_FILE).await {
        Ok(bytes) => bytes,
        // Absent journal is the clean-boot case — nothing to recover, no warning.
        Err(_) => return Vec::new(),
    };

    let file = match serde_json::from_slice::<PendingMovesFile>(&bytes) {
        Ok(file) => file,
        Err(e) => {
            warn!("Discarding corrupt move-recovery journal (could not parse): {e}");
            return Vec::new();
        }
    };

    if file.version != PENDING_MOVES_VERSION {
        warn!(
            "Discarding move-recovery journal: version {} != expected {}",
            file.version, PENDING_MOVES_VERSION
        );
        return Vec::new();
    }

    file.pending
}

/// Map the journal's DELETE records into the [`JournalReStitch`] inputs boot
/// reconcile consumes.
///
/// Only DELETE records become re-stitch inputs: a delete carries the move's source
/// UUID (the lineage to re-attach at the new path), whereas a create's standalone
/// meaning — "a brand-new file" — is already what reconcile's per-file loop does when
/// it finds an orphaned `.md` with no live node. So creates are skipped here and the
/// daemon ignores them entirely after this mapping (P4f-2b-ii §1).
///
/// Records that can't decode (bad hex `content_hash`, missing/unparseable `uuid`) are
/// skipped with a `warn!` rather than aborting: a skipped delete simply won't
/// re-stitch and falls through to the daemon's finalize, which re-checks the live-node
/// state regardless.
pub fn restitch_inputs(records: &[JournaledMove]) -> Vec<JournalReStitch> {
    let mut inputs = Vec::new();

    for record in records {
        if record.kind != PendingKind::Delete {
            continue; // creates are reconcile's job (§1)
        }

        let Some(content_hash) = hex_to_bytes32(&record.content_hash) else {
            warn!(
                "Skipping journaled delete for {}: malformed content_hash",
                record.path
            );
            continue;
        };

        // A delete ALWAYS carries its UUID (P4f-2a), but guard defensively: a missing
        // or unparseable UUID can't re-stitch a lineage, so skip rather than panic.
        let uuid = match record.uuid.as_deref().map(Uuid::parse_str) {
            Some(Ok(uuid)) => uuid,
            _ => {
                warn!(
                    "Skipping journaled delete for {}: missing or unparseable uuid",
                    record.path
                );
                continue;
            }
        };

        inputs.push(JournalReStitch {
            uuid,
            old_path: record.path.clone(),
            content_hash,
        });
    }

    inputs
}

/// Rewrite the crash-recovery journal as an empty pending set.
///
/// Called once AFTER the full recovery pass (re-stitch inside load, then the daemon
/// finalize) so the next boot recovers nothing. Writing an empty well-formed journal
/// rather than deleting the file matches every other journal write in the codebase
/// (`persist_pending_moves`) and reads back identically to an absent file (`pending:
/// []` → `vec![]`). A crash BEFORE this write leaves the intact journal on disk, so
/// the next boot simply re-runs the idempotent recovery pass.
pub async fn clear_pending_journal<FS: FileSystem>(fs: &FS) {
    let file = PendingMovesFile {
        version: PENDING_MOVES_VERSION,
        pending: Vec::new(),
    };
    let bytes = match serde_json::to_vec(&file) {
        Ok(bytes) => bytes,
        Err(e) => {
            error!("Failed to serialize empty move-recovery journal: {e}");
            return;
        }
    };
    if let Err(e) = fs.atomic_write(PENDING_MOVES_FILE, &bytes).await {
        error!("Failed to clear move-recovery journal: {e}");
    }
}

/// Decode the journal's lowercase-hex `content_hash` (64 chars) back into the
/// `[u8; 32]` reconcile's hash domain uses — the inverse of the coalescer's
/// `hex_lower`. Returns `None` on a wrong length or a non-hex nibble (a malformed
/// record is skipped, never fatal).
fn hex_to_bytes32(s: &str) -> Option<[u8; 32]> {
    if s.len() != 64 {
        return None;
    }
    let mut out = [0u8; 32];
    for (i, byte) in out.iter_mut().enumerate() {
        *byte = u8::from_str_radix(&s[i * 2..i * 2 + 2], 16).ok()?;
    }
    Some(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::move_coalescer::hex_lower;

    #[test]
    fn hex_to_bytes32_round_trips_hex_lower() {
        // A fixed, non-trivial 32-byte value: every byte distinct so a transposition
        // or off-by-one in the decode would change the result.
        let original: [u8; 32] = std::array::from_fn(|i| (i as u8).wrapping_mul(7).wrapping_add(3));
        let encoded = hex_lower(&original);
        assert_eq!(encoded.len(), 64);
        assert_eq!(hex_to_bytes32(&encoded), Some(original));
    }

    #[test]
    fn hex_to_bytes32_rejects_wrong_length() {
        // 63 chars (one nibble short) — a truncated/torn record.
        let short = "a".repeat(63);
        assert_eq!(hex_to_bytes32(&short), None);
    }

    #[test]
    fn hex_to_bytes32_rejects_non_hex() {
        // Correct length, but a non-hex character ('z') in the middle.
        let mut bad = "a".repeat(64);
        bad.replace_range(30..31, "z");
        assert_eq!(hex_to_bytes32(&bad), None);
    }
}
