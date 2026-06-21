//! The reusable in-memory test harness for the `vault-sync` integration tests.
//!
//! Every test wires N `Vault<Arc<InMemoryFs>>` replicas and pumps opaque byte
//! payloads between them by hand — the seam is bytes, so a `Vec<u8>` channel IS
//! the transport (spec §5, §9 universal harness rule). **No test ever touches a
//! real on-disk vault — everything runs against `InMemoryFs`.**
//!
//! Each integration-test binary (`tests/*.rs`) compiles this module independently
//! and uses only the subset of helpers it needs, so `#![allow(dead_code)]`
//! suppresses the per-binary "never used" warnings for the helpers a given binary
//! doesn't reach. The helpers drive the public `Vault` API only — no reach-ins to
//! `pub(crate)` internals — so they're reusable across every test surface (the
//! convergence/flow tests in `sync.rs`, the reconcile tests in `reconcile.rs`, and
//! the cross-chunk acceptance roll-up in `rollup.rs`), and they stay reusable for the
//! Phase-2 conflict and Phase-3 compare suites.

#![allow(dead_code)]

use std::collections::HashMap;
use std::sync::Arc;

use uuid::Uuid;
use vault_sync::{DocId, FileSystem, InMemoryFs, SyncMessage, Vault};

/// The two sides of a two-vault handshake, as Loro author ids (Loro peer ids).
/// `author(1)` and `author(2)` produce these; they're exposed as constants too for
/// the tests that build a `Vault::init` / `SyncMessage` inline (outside a helper).
pub const AUTHOR_A: u64 = 0x0101_0101_0101_0101;
pub const AUTHOR_B: u64 = 0x0202_0202_0202_0202;
/// The single-vault author (same value as `AUTHOR_A`), for the reconcile surface
/// where only one replica exists.
pub const AUTHOR: u64 = AUTHOR_A;

pub type Fs = Arc<InMemoryFs>;
pub type V = Vault<Fs>;

/// A deterministic Loro author id built from a single byte, e.g. `author(1)` /
/// `author(2)` for the two sides of a handshake (each byte of the 8-byte id is `n`,
/// so `author(1) == AUTHOR_A`). Keeps the per-replica authorship distinct so a
/// merged document's version vector carries one entry per device (the
/// independent-authorship tripwire).
pub fn author(n: u8) -> u64 {
    u64::from_ne_bytes([n; 8])
}

/// Build two empty in-memory vaults, A authored by `author(1)`, B by `author(2)`,
/// returning each vault alongside its retained `Arc<InMemoryFs>` so a test can write
/// files and inspect on-disk bytes directly.
pub async fn two_vaults() -> (V, V, Fs, Fs) {
    let fs_a = Arc::new(InMemoryFs::new());
    let fs_b = Arc::new(InMemoryFs::new());
    let a = Vault::init(Arc::clone(&fs_a), author(1)).await.unwrap();
    let b = Vault::init(Arc::clone(&fs_b), author(2)).await.unwrap();
    (a, b, fs_a, fs_b)
}

/// Build a single empty in-memory vault authored by `author(1)`, returning its
/// retained `Arc<InMemoryFs>` so a test can stage on-disk state directly. The
/// single-vault counterpart to [`two_vaults`] for the reconcile / boot tests.
pub async fn one_vault() -> (V, Fs) {
    let fs = Arc::new(InMemoryFs::new());
    let vault = Vault::init(Arc::clone(&fs), author(1)).await.unwrap();
    (vault, fs)
}

/// Drop `vault` and load a fresh one over the same filesystem, simulating a process
/// restart from a cold cache (the `.loro`/registry persist on disk while the
/// in-memory document cache starts empty). Authored by `author(1)` to match
/// [`one_vault`]. Drives boot reconciliation the way a real restart does.
pub async fn reload(vault: V, fs: &Fs) -> V {
    drop(vault);
    Vault::load(Arc::clone(fs), author(1)).await.unwrap()
}

/// Drive the complete three-message handshake with `a` as initiator and return
/// `(modified_a, modified_b)` — the documents each side received.
///
/// A sends a SyncRequest → B answers with a SyncExchange (its response + its own
/// request) → A applies it and replies with a final SyncResponse → B applies that.
/// The protocol always produces the final SyncResponse, so the unwrap chain is exact;
/// getting this order wrong is the latent "pass-by-luck" risk this helper removes.
/// Tests that assert on the intermediate messages keep the manual form.
pub async fn full_sync(a: &V, b: &V) -> (Vec<DocId>, Vec<DocId>) {
    let request = a.prepare_request().await.unwrap();
    let exchange = b.process_message(&request).await.unwrap();
    let after_exchange = a.process_message(&exchange.reply.unwrap()).await.unwrap();
    let after_final = b
        .process_message(&after_exchange.reply.unwrap())
        .await
        .unwrap();
    (after_exchange.modified, after_final.modified)
}

/// Pump the handshake in BOTH directions to quiescence (A→B then B→A), so divergent
/// edits on each side land on the other.
pub async fn sync_both_ways(a: &V, b: &V) {
    full_sync(a, b).await;
    full_sync(b, a).await;
}

/// Write `content` to `path` on `fs` and document it into `vault` (Flow-1), then
/// flush the Index (Flow-1 is caller-flushed). The fs-as-truth local-write path.
pub async fn write_and_index(vault: &V, fs: &Fs, path: &str, content: &str) {
    fs.write(path, content.as_bytes()).await.unwrap();
    vault.on_file_changed(path).await.unwrap();
    vault.save_index().await.unwrap();
}

/// Move a `.md` on disk AND in `vault`'s Index — the local move flow a real editor
/// drives (the editor relocates the file, the watcher moves the Index node). The
/// content `.loro` is path-independent, so it is never touched. Flushes the Index.
///
/// Ensures the destination directory exists first (a real `rename` into a missing
/// directory would fail; the `InMemoryFs` `rename` does not auto-create parents, so
/// the test creates them as the editor's move would).
pub async fn move_file(vault: &V, fs: &Fs, from: &str, to: &str) {
    if let Some((parent, _)) = to.rsplit_once('/') {
        fs.mkdir(parent).await.unwrap();
    }
    fs.rename(from, to).await.unwrap();
    vault.index().move_node(from, to).unwrap();
    vault.save_index().await.unwrap();
}

/// Move a whole FOLDER on disk AND in `vault`'s Index — the local folder-move flow
/// a real editor drives (the editor relocates the directory, the daemon's
/// folder-move detection maps it to one `move_subtree`). Like [`move_file`], the
/// content `.loro`s are path-independent, so none are touched.
///
/// Relocates every descendant `.md` on disk from `old_prefix/...` to
/// `new_prefix/...` (so A's disk stays consistent with its Index, exactly as a real
/// folder rename leaves it), then applies the single structural `move_subtree` to
/// the Index and flushes. The on-disk renames here stand in for the daemon's
/// event-coalescing (P4); P2c is the lib primitive, so the test drives it directly.
pub async fn move_subtree(vault: &V, fs: &Fs, old_prefix: &str, new_prefix: &str) {
    let old_dir = format!("{old_prefix}/");
    let descendants: Vec<String> = vault
        .list_files()
        .await
        .unwrap()
        .into_iter()
        .filter(|p| p.starts_with(&old_dir))
        .collect();

    for from in &descendants {
        let to = format!("{}{}", new_prefix, &from[old_prefix.len()..]);
        if let Some((parent, _)) = to.rsplit_once('/') {
            fs.mkdir(parent).await.unwrap();
        }
        fs.rename(from, &to).await.unwrap();
    }

    vault.index().move_subtree(old_prefix, new_prefix).unwrap();
    vault.save_index().await.unwrap();
}

/// The UUID a path currently resolves to in `vault`'s Index (panics if no node).
pub fn uuid_at(vault: &V, path: &str) -> Uuid {
    let node = vault
        .index()
        .node_for_path(path)
        .unwrap_or_else(|| panic!("no Index node for {path}"));
    vault
        .index()
        .node_uuid(&node)
        .unwrap_or_else(|| panic!("node for {path} carries no UUID"))
}

/// Read a `.md` file's bytes from a filesystem (panics if absent).
pub async fn read_md(fs: &Fs, path: &str) -> Vec<u8> {
    fs.read(path)
        .await
        .unwrap_or_else(|_| panic!("no file at {path}"))
}

/// The canonical materialized markdown of the document currently at `path` — the
/// exact bytes the document renders to, used to stage a "matching" `.md` at another
/// path (a native move leaves byte-identical content).
pub async fn materialized_markdown(vault: &V, path: &str) -> String {
    vault.get_document(path).await.unwrap().to_markdown()
}

/// Deserialize a wire payload into a `SyncMessage` for byte-level assertions.
pub fn decode(bytes: &[u8]) -> SyncMessage {
    bincode::deserialize(bytes).expect("payload is a valid SyncMessage")
}

/// The total bytes of document-content carried by a message (the sum of every
/// `document_updates` value). Zero means no document content crossed the wire — the
/// assertion the INV-1 zero-content-on-move guarantee is checked against.
pub fn document_content_bytes(msg: &SyncMessage) -> usize {
    let updates: &HashMap<DocId, Vec<u8>> = match msg {
        SyncMessage::SyncResponse {
            document_updates, ..
        } => document_updates,
        SyncMessage::SyncExchange { response, .. } => &response.document_updates,
        SyncMessage::DocUpdate { data, .. } => return data.len(),
        _ => return 0,
    };
    updates.values().map(|v| v.len()).sum()
}

// ============================ N-replica quiescence driver ============================

/// Pump full-sync handshakes between every ordered pair of `replicas` until a full
/// round produces no change anywhere — the AC-INV-4 / AC-§5 quiescence driver.
///
/// The seam is opaque bytes, so this is exactly how a daemon settles a mesh: open a
/// handshake with each peer, ship the bytes, feed every reply back. Each "round"
/// runs `full_sync` once in each direction for every ordered pair `(i, j)`; the loop
/// terminates when an entire round reports zero `modified` documents on any side
/// (the network is quiescent — no replica learned anything new). A round cap guards
/// against a non-converging bug turning the test into a hang (it panics with a clear
/// message rather than spinning forever).
///
/// Returns the number of rounds it took to reach quiescence (a useful signal: a
/// healthy convergence settles in a small, bounded number of rounds).
pub async fn pump_to_quiescence(replicas: &[&V]) -> usize {
    // A generous bound: convergence for the sizes these tests use settles in a
    // handful of rounds. Exceeding it means a real non-convergence bug, not a slow
    // test — fail loudly instead of hanging CI.
    const MAX_ROUNDS: usize = 100;

    for round in 1..=MAX_ROUNDS {
        let mut changed_this_round = false;
        for i in 0..replicas.len() {
            for j in 0..replicas.len() {
                if i == j {
                    continue;
                }
                let (modified_initiator, modified_responder) =
                    full_sync(replicas[i], replicas[j]).await;
                if !modified_initiator.is_empty() || !modified_responder.is_empty() {
                    changed_this_round = true;
                }
            }
        }
        if !changed_this_round {
            return round;
        }
    }
    panic!(
        "pump_to_quiescence did not converge within {MAX_ROUNDS} rounds — \
         a non-convergence bug, not a slow test"
    );
}

// ============================ lossy transport wrapper ============================

/// A tiny deterministic PRNG (SplitMix64) so the lossy and shuffle wrappers are
/// fully reproducible — a seeded run is the same every time, so a failure is a real
/// bug, never transport-coin-flip flake. Seeding it from a fixed `u64` makes the
/// whole randomized exchange replayable.
pub struct DeterministicRng(u64);

impl DeterministicRng {
    /// Seed the PRNG. The same seed yields the same drop/dup/reorder/shuffle decisions
    /// on every run and every machine.
    pub fn new(seed: u64) -> Self {
        Self(seed)
    }

    /// The next pseudo-random `u64` (SplitMix64 — small, fast, well-distributed; this
    /// is test entropy, not crypto).
    fn next_u64(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^ (z >> 31)
    }

    /// A fair coin that comes up `true` with the given probability in `[0.0, 1.0]`.
    pub fn chance(&mut self, p: f64) -> bool {
        (self.next_u64() as f64 / u64::MAX as f64) < p
    }

    /// Fisher–Yates shuffle `items` in place using this PRNG.
    pub fn shuffle<T>(&mut self, items: &mut [T]) {
        for i in (1..items.len()).rev() {
            let j = (self.next_u64() % (i as u64 + 1)) as usize;
            items.swap(i, j);
        }
    }
}

/// Faults a lossy transport injects on each payload it carries.
#[derive(Clone, Copy)]
pub struct LossProfile {
    /// Probability a payload is dropped entirely (never delivered).
    pub drop: f64,
    /// Probability a delivered payload is ALSO delivered a second time (a duplicate).
    pub duplicate: f64,
}

impl LossProfile {
    /// A moderately hostile channel: some drops, some duplicates. Combined with the
    /// out-of-order pairing in `pump_lossy_to_quiescence`, this exercises the full
    /// drop/dup/reorder triad of the §5 seam contract.
    pub fn hostile() -> Self {
        Self {
            drop: 0.3,
            duplicate: 0.3,
        }
    }
}

/// Pump full-sync handshakes between every ordered pair of `replicas` over a LOSSY
/// channel — payloads are randomly dropped, duplicated, and delivered out of pair
/// order — and converge anyway (AC-§5: the lib tolerates an unreliable transport
/// because every exchange re-derives what differs from version vectors).
///
/// **Why two phases.** A heavily-lossy channel can *drive* convergence but cannot
/// reliably *detect* quiescence: with a non-trivial drop/dup rate across N² pairs, a
/// completely fault-free round is astronomically rare, so a "stop when a clean round
/// changed nothing" loop would spin forever even after the replicas had already
/// converged. So each iteration runs a LOSSY round (exercise drop/dup/reorder
/// tolerance) followed by a CLEAN settling round (no injected faults): the clean round
/// both delivers anything the lossy round dropped AND detects quiescence — when a
/// clean round changes nothing, every replica is genuinely converged. This mirrors a
/// real flaky network, which also eventually gets clean delivery windows; "eventual
/// convergence" (the §5 wording) is exactly the clean window flushing the backlog.
///
/// The `seed` makes the loss pattern reproducible — a failure is a real
/// non-convergence bug, never transport coin-flip. Each round visits the ordered pairs
/// in a freshly-shuffled order (reorder across exchanges, not just within a stream). A
/// round cap guards against a true non-convergence bug turning the test into a hang.
///
/// Returns the number of lossy/clean iterations taken (bounded; lossy convergence
/// takes more than the clean path but settles quickly once a clean window lands).
pub async fn pump_lossy_to_quiescence(replicas: &[&V], profile: LossProfile, seed: u64) -> usize {
    const MAX_ITERATIONS: usize = 1000;
    let mut rng = DeterministicRng::new(seed);

    for iteration in 1..=MAX_ITERATIONS {
        // Phase 1 — a LOSSY round: drive convergence while dropping/duplicating
        // payloads and reordering the pair visits. Its return is ignored for the
        // termination decision (a lossy round can't prove quiescence); its job is to
        // exercise the seam's fault tolerance.
        lossy_round(replicas, &mut rng, profile).await;

        // Phase 2 — a CLEAN settling round: no injected faults. It delivers whatever
        // Phase 1 dropped and reports whether anything changed. A clean round that
        // changes nothing means every replica is converged.
        if !clean_round(replicas, &mut rng).await {
            return iteration;
        }
    }
    panic!(
        "pump_lossy_to_quiescence did not converge within {MAX_ITERATIONS} iterations — \
         a non-convergence bug, not lossy-transport noise"
    );
}

/// One LOSSY round: visit every ordered pair (in a freshly-shuffled order) and run a
/// fault-injecting handshake for each. Faults and changes are intentionally not
/// reported — a lossy round drives convergence but cannot prove quiescence.
async fn lossy_round(replicas: &[&V], rng: &mut DeterministicRng, profile: LossProfile) {
    let mut pairs = shuffled_pairs(replicas.len(), rng);
    for (i, j) in pairs.drain(..) {
        let _ = lossy_full_sync(replicas[i], replicas[j], rng, profile).await;
    }
}

/// One CLEAN round: visit every ordered pair (in a freshly-shuffled order) and run a
/// fault-free `full_sync` for each. Returns `true` if ANY exchange changed a replica —
/// `false` means the mesh is quiescent.
async fn clean_round(replicas: &[&V], rng: &mut DeterministicRng) -> bool {
    let mut changed = false;
    let mut pairs = shuffled_pairs(replicas.len(), rng);
    for (i, j) in pairs.drain(..) {
        let (modified_initiator, modified_responder) = full_sync(replicas[i], replicas[j]).await;
        if !modified_initiator.is_empty() || !modified_responder.is_empty() {
            changed = true;
        }
    }
    changed
}

/// Every ordered pair `(i, j)` with `i != j`, shuffled — so no pair has a privileged
/// position within a round (reordering across exchanges).
fn shuffled_pairs(n: usize, rng: &mut DeterministicRng) -> Vec<(usize, usize)> {
    let mut pairs: Vec<(usize, usize)> = (0..n)
        .flat_map(|i| (0..n).map(move |j| (i, j)))
        .filter(|(i, j)| i != j)
        .collect();
    rng.shuffle(&mut pairs);
    pairs
}

/// Run ONE handshake over the lossy channel, injecting drops and duplicates per
/// `profile`. Every payload is fed through [`maybe_deliver`]; a dropped payload aborts
/// the rest of THIS handshake (the lib recovers on a later round), a duplicated payload
/// is re-fed to the same receiver (it must tolerate re-delivery without corruption —
/// INV-10). The outcome is intentionally not reported: a lossy handshake drives
/// convergence but the clean settling round is what proves quiescence.
async fn lossy_full_sync(a: &V, b: &V, rng: &mut DeterministicRng, profile: LossProfile) {
    // A→B: the opening request.
    let request = a.prepare_request().await.unwrap();
    let Some(exchange) = maybe_deliver(b, &request, rng, profile).await else {
        return;
    };
    let Some(exchange_reply) = exchange.reply else {
        return;
    };

    // B→A: the exchange reply.
    let Some(after_exchange) = maybe_deliver(a, &exchange_reply, rng, profile).await else {
        return;
    };
    let Some(final_reply) = after_exchange.reply else {
        return;
    };

    // A→B: the final response.
    let _ = maybe_deliver(b, &final_reply, rng, profile).await;
}

/// Deliver `payload` to `receiver` under the loss profile: with probability
/// `profile.drop` the payload is dropped (returns `None`); otherwise it is applied, and
/// with probability `profile.duplicate` it is applied a SECOND time (the receiver must
/// tolerate the duplicate — its second application is discarded but must not corrupt
/// state or panic, INV-10).
async fn maybe_deliver(
    receiver: &V,
    payload: &[u8],
    rng: &mut DeterministicRng,
    profile: LossProfile,
) -> Option<vault_sync::SyncOutcome> {
    if rng.chance(profile.drop) {
        return None;
    }
    let outcome = receiver.process_message(payload).await.unwrap();
    if rng.chance(profile.duplicate) {
        // Re-deliver the SAME bytes: idempotent application must not corrupt state.
        let _ = receiver.process_message(payload).await.unwrap();
    }
    Some(outcome)
}

// ============================ byte-counting wrapper ============================

/// Records the document-content byte sizes of every payload that crosses a counting
/// channel, so a test can assert how much content (if any) a sync transferred — the
/// instrument behind AC-INV-1 ("a move transfers zero content bytes") and the future
/// AC-§6 compare/no-op-cheapness check (P3).
#[derive(Default)]
pub struct ByteCounter {
    /// One entry per delivered payload: its document-content byte total (the sum of
    /// every `document_updates` value, via [`document_content_bytes`]).
    pub document_content_per_message: Vec<usize>,
}

impl ByteCounter {
    pub fn new() -> Self {
        Self::default()
    }

    /// Total document-content bytes across every payload counted so far.
    pub fn total_document_content_bytes(&self) -> usize {
        self.document_content_per_message.iter().sum()
    }

    /// Run a full handshake A→B, recording the document-content byte total of EVERY
    /// payload (the request carries none, but the exchange and final response can), so
    /// a test can assert e.g. "this whole sync moved zero document content" after a
    /// pure move. Returns `(modified_a, modified_b)` like [`full_sync`].
    pub async fn full_sync_counting(&mut self, a: &V, b: &V) -> (Vec<DocId>, Vec<DocId>) {
        let request = a.prepare_request().await.unwrap();
        self.record(&request);

        let exchange = b.process_message(&request).await.unwrap();
        let exchange_reply = exchange.reply.unwrap();
        self.record(&exchange_reply);

        let after_exchange = a.process_message(&exchange_reply).await.unwrap();
        let final_reply = after_exchange.reply.unwrap();
        self.record(&final_reply);

        let after_final = b.process_message(&final_reply).await.unwrap();
        (after_exchange.modified, after_final.modified)
    }

    /// Decode `payload` and record its document-content byte total.
    fn record(&mut self, payload: &[u8]) {
        self.document_content_per_message
            .push(document_content_bytes(&decode(payload)));
    }
}
