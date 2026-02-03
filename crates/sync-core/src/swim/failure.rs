//! SWIM failure detection.
//!
//! Implements failure detection using ping/ack with indirect probing:
//! 1. Periodically ping a random peer
//! 2. If no ack within timeout, send indirect ping via K other peers
//! 3. If indirect ping also fails, mark target as "suspected"
//! 4. If suspicion timeout expires, mark as "dead"

use crate::PeerId;
use std::collections::HashMap;
use std::time::Duration;

/// Configuration for failure detection.
#[derive(Debug, Clone)]
pub struct FailureDetectorConfig {
    /// How often to ping a random peer (default: 1s)
    pub ping_interval: Duration,
    /// How long to wait for ack before indirect ping (default: 500ms)
    pub ping_timeout: Duration,
    /// Number of peers to ask for indirect ping (default: 3)
    pub indirect_peers: usize,
    /// Time before suspected → dead (default: 5s)
    pub suspicion_timeout: Duration,
}

impl Default for FailureDetectorConfig {
    fn default() -> Self {
        Self {
            ping_interval: Duration::from_secs(1),
            ping_timeout: Duration::from_millis(500),
            indirect_peers: 3,
            suspicion_timeout: Duration::from_secs(5),
        }
    }
}

/// State of a pending ping.
#[derive(Debug, Clone)]
pub struct PendingPing {
    /// Target peer being pinged
    pub target: PeerId,
    /// When the ping was sent (milliseconds since epoch or Instant)
    pub sent_at: u64,
    /// Whether we've started indirect probing
    pub indirect_started: bool,
    /// Peers we've asked for indirect ping
    pub indirect_peers: Vec<PeerId>,
    /// How many indirect responses we've received
    pub indirect_responses: usize,
}

/// State of a suspected peer.
#[derive(Debug, Clone)]
pub struct Suspicion {
    /// When suspicion started (milliseconds since epoch)
    pub started_at: u64,
    /// Incarnation at time of suspicion
    pub incarnation: u64,
    /// Optional buddy assigned to verify
    pub buddy: Option<PeerId>,
}

/// Event emitted by the failure detector.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FailureEvent {
    /// Need to send a ping to this peer
    SendPing { target: PeerId, seq: u64 },
    /// Need to send indirect ping request to these peers
    SendPingReq {
        target: PeerId,
        via: Vec<PeerId>,
        seq: u64,
    },
    /// Peer is now suspected (not responding)
    PeerSuspected { peer_id: PeerId, incarnation: u64 },
    /// Peer confirmed dead (failed to refute suspicion)
    PeerDead { peer_id: PeerId },
    /// Peer is alive (refuted suspicion)
    PeerAlive { peer_id: PeerId, incarnation: u64 },
}

/// Failure detector for SWIM protocol.
///
/// Tracks pending pings and suspected peers, emitting events when
/// state changes occur. The caller is responsible for:
/// - Calling `tick()` periodically
/// - Forwarding ping/ack messages
/// - Acting on emitted events
pub struct FailureDetector {
    config: FailureDetectorConfig,
    /// Next sequence number for pings
    next_seq: u64,
    /// Pending pings indexed by sequence number
    pending_pings: HashMap<u64, PendingPing>,
    /// Suspected peers indexed by peer ID
    suspicions: HashMap<PeerId, Suspicion>,
    /// Last time we initiated a ping cycle (milliseconds)
    last_ping_cycle: u64,
}

impl FailureDetector {
    /// Create a new failure detector.
    pub fn new(config: FailureDetectorConfig) -> Self {
        Self {
            config,
            next_seq: 1,
            pending_pings: HashMap::new(),
            suspicions: HashMap::new(),
            last_ping_cycle: 0,
        }
    }

    /// Create with default configuration.
    pub fn with_defaults() -> Self {
        Self::new(FailureDetectorConfig::default())
    }

    /// Get the configuration.
    pub fn config(&self) -> &FailureDetectorConfig {
        &self.config
    }

    /// Start a ping to a target peer.
    ///
    /// Returns the sequence number to use for the ping.
    pub fn start_ping(&mut self, target: PeerId, now_ms: u64) -> u64 {
        let seq = self.next_seq;
        self.next_seq += 1;

        self.pending_pings.insert(
            seq,
            PendingPing {
                target,
                sent_at: now_ms,
                indirect_started: false,
                indirect_peers: Vec::new(),
                indirect_responses: 0,
            },
        );

        seq
    }

    /// Record that we received an ack for a ping.
    ///
    /// Returns true if this clears a pending ping.
    pub fn receive_ack(&mut self, seq: u64) -> bool {
        self.pending_pings.remove(&seq).is_some()
    }

    /// Record that we received a PingReqAck (indirect probe result).
    ///
    /// Returns events if this resolves the probe (alive or all failed).
    pub fn receive_ping_req_ack(
        &mut self,
        target: PeerId,
        seq: u64,
        alive: bool,
    ) -> Vec<FailureEvent> {
        let mut events = Vec::new();

        if let Some(pending) = self.pending_pings.get_mut(&seq) {
            if pending.target != target {
                return events;
            }

            if alive {
                // Target responded via indirect - clear suspicion
                self.pending_pings.remove(&seq);
                if let Some(suspicion) = self.suspicions.remove(&target) {
                    events.push(FailureEvent::PeerAlive {
                        peer_id: target,
                        incarnation: suspicion.incarnation + 1,
                    });
                }
            } else {
                pending.indirect_responses += 1;

                // If all indirect probes failed, suspect the peer
                if pending.indirect_responses >= pending.indirect_peers.len() {
                    let incarnation = 0; // Will be updated by caller
                    self.pending_pings.remove(&seq);
                    events.push(FailureEvent::PeerSuspected {
                        peer_id: target,
                        incarnation,
                    });
                }
            }
        }

        events
    }

    /// Check for timed-out pings and suspicions.
    ///
    /// Call this periodically (e.g., every 100ms) to detect failures.
    /// Returns events for any state transitions.
    pub fn check_timeouts(&mut self, now_ms: u64) -> Vec<FailureEvent> {
        let mut events = Vec::new();
        let ping_timeout_ms = self.config.ping_timeout.as_millis() as u64;
        let suspicion_timeout_ms = self.config.suspicion_timeout.as_millis() as u64;

        // Find pings that have timed out
        let mut start_indirect = Vec::new();
        let mut suspect = Vec::new();

        for (seq, pending) in &self.pending_pings {
            let elapsed = now_ms.saturating_sub(pending.sent_at);

            if !pending.indirect_started && elapsed >= ping_timeout_ms {
                // Direct ping timed out - need indirect probing
                start_indirect.push(*seq);
            } else if pending.indirect_started && elapsed >= ping_timeout_ms * 3 {
                // Indirect probing also timed out
                suspect.push((*seq, pending.target));
            }
        }

        // Mark pings as needing indirect probing (caller must provide peers)
        for seq in start_indirect {
            if let Some(pending) = self.pending_pings.get_mut(&seq) {
                pending.indirect_started = true;
            }
        }

        // Suspect peers whose indirect probes timed out
        for (seq, target) in suspect {
            self.pending_pings.remove(&seq);
            events.push(FailureEvent::PeerSuspected {
                peer_id: target,
                incarnation: 0,
            });
        }

        // Check for expired suspicions → dead
        let mut dead = Vec::new();
        for (peer_id, suspicion) in &self.suspicions {
            let elapsed = now_ms.saturating_sub(suspicion.started_at);
            if elapsed >= suspicion_timeout_ms {
                dead.push(*peer_id);
            }
        }

        for peer_id in dead {
            self.suspicions.remove(&peer_id);
            events.push(FailureEvent::PeerDead { peer_id });
        }

        events
    }

    /// Start suspicion of a peer.
    ///
    /// Call this when direct + indirect pings fail.
    pub fn suspect(&mut self, peer_id: PeerId, incarnation: u64, now_ms: u64) {
        self.suspicions.insert(
            peer_id,
            Suspicion {
                started_at: now_ms,
                incarnation,
                buddy: None,
            },
        );
    }

    /// Clear suspicion of a peer (they proved alive).
    pub fn clear_suspicion(&mut self, peer_id: &PeerId) -> Option<Suspicion> {
        self.suspicions.remove(peer_id)
    }

    /// Check if a peer is currently suspected.
    pub fn is_suspected(&self, peer_id: &PeerId) -> bool {
        self.suspicions.contains_key(peer_id)
    }

    /// Get peers that need indirect probing.
    ///
    /// Returns (seq, target) for each ping that timed out and needs indirect probes.
    pub fn pending_indirect_probes(&self) -> Vec<(u64, PeerId)> {
        self.pending_pings
            .iter()
            .filter(|(_, p)| p.indirect_started && p.indirect_peers.is_empty())
            .map(|(seq, p)| (*seq, p.target))
            .collect()
    }

    /// Record that we've started indirect probing via specific peers.
    pub fn set_indirect_peers(&mut self, seq: u64, peers: Vec<PeerId>) {
        if let Some(pending) = self.pending_pings.get_mut(&seq) {
            pending.indirect_peers = peers;
        }
    }

    /// Get number of pending pings.
    pub fn pending_ping_count(&self) -> usize {
        self.pending_pings.len()
    }

    /// Get number of suspected peers.
    pub fn suspicion_count(&self) -> usize {
        self.suspicions.len()
    }

    /// Check if it's time for a new ping cycle.
    pub fn should_ping(&self, now_ms: u64) -> bool {
        let interval_ms = self.config.ping_interval.as_millis() as u64;
        now_ms.saturating_sub(self.last_ping_cycle) >= interval_ms
    }

    /// Mark that we've started a ping cycle.
    pub fn mark_ping_cycle(&mut self, now_ms: u64) {
        self.last_ping_cycle = now_ms;
    }

    /// Assign a buddy to verify a suspected peer.
    pub fn assign_buddy(&mut self, suspected: PeerId, buddy: PeerId) {
        if let Some(suspicion) = self.suspicions.get_mut(&suspected) {
            suspicion.buddy = Some(buddy);
        }
    }

    /// Get the assigned buddy for a suspected peer.
    pub fn get_buddy(&self, suspected: &PeerId) -> Option<PeerId> {
        self.suspicions.get(suspected).and_then(|s| s.buddy)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn peer_a() -> PeerId {
        "a1b2c3d4e5f67890".parse().unwrap()
    }

    fn peer_b() -> PeerId {
        "1234567890abcdef".parse().unwrap()
    }

    fn peer_c() -> PeerId {
        "fedcba0987654321".parse().unwrap()
    }

    fn test_config() -> FailureDetectorConfig {
        FailureDetectorConfig {
            ping_interval: Duration::from_millis(1000),
            ping_timeout: Duration::from_millis(500),
            indirect_peers: 3,
            suspicion_timeout: Duration::from_millis(5000),
        }
    }

    // ==================== Basic ping/ack ====================

    #[test]
    fn test_start_ping() {
        let mut detector = FailureDetector::new(test_config());

        let seq1 = detector.start_ping(peer_a(), 1000);
        let seq2 = detector.start_ping(peer_b(), 1001);

        assert_eq!(seq1, 1);
        assert_eq!(seq2, 2);
        assert_eq!(detector.pending_ping_count(), 2);
    }

    #[test]
    fn test_receive_ack() {
        let mut detector = FailureDetector::new(test_config());

        let seq = detector.start_ping(peer_a(), 1000);
        assert_eq!(detector.pending_ping_count(), 1);

        let cleared = detector.receive_ack(seq);
        assert!(cleared);
        assert_eq!(detector.pending_ping_count(), 0);
    }

    #[test]
    fn test_receive_ack_unknown_seq() {
        let mut detector = FailureDetector::new(test_config());

        let cleared = detector.receive_ack(999);
        assert!(!cleared);
    }

    // ==================== Ping timeout → indirect probing ====================

    #[test]
    fn test_ping_timeout_starts_indirect() {
        let mut detector = FailureDetector::new(test_config());

        let seq = detector.start_ping(peer_a(), 1000);

        // Before timeout - nothing happens
        let events = detector.check_timeouts(1400);
        assert!(events.is_empty());

        // After timeout - should mark as needing indirect
        let events = detector.check_timeouts(1600);
        assert!(events.is_empty()); // No events yet, just marks indirect_started

        // Should be in pending_indirect_probes
        let probes = detector.pending_indirect_probes();
        assert_eq!(probes.len(), 1);
        assert_eq!(probes[0], (seq, peer_a()));
    }

    #[test]
    fn test_indirect_probe_success() {
        let mut detector = FailureDetector::new(test_config());

        let seq = detector.start_ping(peer_a(), 1000);

        // Timeout and start indirect
        detector.check_timeouts(1600);
        detector.set_indirect_peers(seq, vec![peer_b(), peer_c()]);

        // Receive positive indirect response
        let events = detector.receive_ping_req_ack(peer_a(), seq, true);

        // Should clear the pending ping
        assert_eq!(detector.pending_ping_count(), 0);

        // If there was a suspicion, it would emit PeerAlive
        // (but we didn't suspect yet in this test)
        assert!(events.is_empty());
    }

    #[test]
    fn test_indirect_probe_all_fail() {
        let mut detector = FailureDetector::new(test_config());

        let seq = detector.start_ping(peer_a(), 1000);

        // Timeout and start indirect
        detector.check_timeouts(1600);
        detector.set_indirect_peers(seq, vec![peer_b(), peer_c()]);

        // First negative response
        let events = detector.receive_ping_req_ack(peer_a(), seq, false);
        assert!(events.is_empty());
        assert_eq!(detector.pending_ping_count(), 1);

        // Second negative response - all failed
        let events = detector.receive_ping_req_ack(peer_a(), seq, false);
        assert_eq!(events.len(), 1);
        assert!(matches!(
            events[0],
            FailureEvent::PeerSuspected { peer_id, .. } if peer_id == peer_a()
        ));
        assert_eq!(detector.pending_ping_count(), 0);
    }

    // ==================== Suspicion timeout → dead ====================

    #[test]
    fn test_suspicion_timeout_marks_dead() {
        let mut detector = FailureDetector::new(test_config());

        detector.suspect(peer_a(), 1, 1000);
        assert!(detector.is_suspected(&peer_a()));
        assert_eq!(detector.suspicion_count(), 1);

        // Before timeout
        let events = detector.check_timeouts(4000);
        assert!(events.is_empty());

        // After timeout
        let events = detector.check_timeouts(6100);
        assert_eq!(events.len(), 1);
        assert!(matches!(
            events[0],
            FailureEvent::PeerDead { peer_id } if peer_id == peer_a()
        ));

        assert!(!detector.is_suspected(&peer_a()));
        assert_eq!(detector.suspicion_count(), 0);
    }

    #[test]
    fn test_clear_suspicion() {
        let mut detector = FailureDetector::new(test_config());

        detector.suspect(peer_a(), 5, 1000);
        assert!(detector.is_suspected(&peer_a()));

        let suspicion = detector.clear_suspicion(&peer_a());
        assert!(suspicion.is_some());
        assert_eq!(suspicion.unwrap().incarnation, 5);

        assert!(!detector.is_suspected(&peer_a()));
    }

    // ==================== Buddy system ====================

    #[test]
    fn test_assign_buddy() {
        let mut detector = FailureDetector::new(test_config());

        detector.suspect(peer_a(), 1, 1000);
        detector.assign_buddy(peer_a(), peer_b());

        let buddy = detector.get_buddy(&peer_a());
        assert_eq!(buddy, Some(peer_b()));
    }

    #[test]
    fn test_buddy_for_non_suspected() {
        let detector = FailureDetector::new(test_config());

        let buddy = detector.get_buddy(&peer_a());
        assert!(buddy.is_none());
    }

    // ==================== Ping cycle timing ====================

    #[test]
    fn test_should_ping() {
        let mut detector = FailureDetector::new(test_config());

        // Initially should ping (enough time has "passed" since epoch 0)
        assert!(detector.should_ping(1000));

        // Mark that we pinged at 1000
        detector.mark_ping_cycle(1000);

        // Too soon (only 500ms elapsed)
        assert!(!detector.should_ping(1500));

        // Ready again (1000ms elapsed = ping_interval)
        assert!(detector.should_ping(2000));
    }

    // ==================== Indirect timeout ====================

    #[test]
    fn test_indirect_timeout_suspects() {
        let mut detector = FailureDetector::new(test_config());

        let seq = detector.start_ping(peer_a(), 1000);

        // Start indirect probing
        detector.check_timeouts(1600);
        detector.set_indirect_peers(seq, vec![peer_b()]);

        // Indirect also times out (ping_timeout * 3 = 1500ms from original send)
        let events = detector.check_timeouts(3000);

        assert_eq!(events.len(), 1);
        assert!(matches!(
            events[0],
            FailureEvent::PeerSuspected { peer_id, .. } if peer_id == peer_a()
        ));
    }

    // ==================== Edge cases ====================

    #[test]
    fn test_receive_ping_req_ack_wrong_target() {
        let mut detector = FailureDetector::new(test_config());

        let seq = detector.start_ping(peer_a(), 1000);
        detector.check_timeouts(1600);
        detector.set_indirect_peers(seq, vec![peer_b()]);

        // Response for wrong target - ignored
        let events = detector.receive_ping_req_ack(peer_c(), seq, true);
        assert!(events.is_empty());
        assert_eq!(detector.pending_ping_count(), 1);
    }

    #[test]
    fn test_receive_ping_req_ack_unknown_seq() {
        let mut detector = FailureDetector::new(test_config());

        let events = detector.receive_ping_req_ack(peer_a(), 999, true);
        assert!(events.is_empty());
    }

    #[test]
    fn test_multiple_suspicions() {
        let mut detector = FailureDetector::new(test_config());

        detector.suspect(peer_a(), 1, 1000);
        detector.suspect(peer_b(), 2, 1500);
        detector.suspect(peer_c(), 1, 2000);

        assert_eq!(detector.suspicion_count(), 3);

        // A expires first
        let events = detector.check_timeouts(6100);
        assert_eq!(events.len(), 1);
        assert!(matches!(events[0], FailureEvent::PeerDead { peer_id } if peer_id == peer_a()));

        // B expires
        let events = detector.check_timeouts(6600);
        assert_eq!(events.len(), 1);
        assert!(matches!(events[0], FailureEvent::PeerDead { peer_id } if peer_id == peer_b()));

        // C expires
        let events = detector.check_timeouts(7100);
        assert_eq!(events.len(), 1);
        assert!(matches!(events[0], FailureEvent::PeerDead { peer_id } if peer_id == peer_c()));
    }

    #[test]
    fn test_indirect_clears_existing_suspicion() {
        let mut detector = FailureDetector::new(test_config());

        // Peer was already suspected
        detector.suspect(peer_a(), 5, 1000);

        // New ping attempt
        let seq = detector.start_ping(peer_a(), 2000);
        detector.check_timeouts(2600);
        detector.set_indirect_peers(seq, vec![peer_b()]);

        // Indirect succeeds - should clear suspicion
        let events = detector.receive_ping_req_ack(peer_a(), seq, true);

        assert_eq!(events.len(), 1);
        assert!(matches!(
            events[0],
            FailureEvent::PeerAlive { peer_id, incarnation } if peer_id == peer_a() && incarnation == 6
        ));

        assert!(!detector.is_suspected(&peer_a()));
    }
}
