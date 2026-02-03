//! Buddy system for SWIM failure detection (Lifeguard extension).
//!
//! When a peer is suspected (failed to respond to pings), we assign a random
//! "buddy" to independently verify the suspicion. This reduces false positives
//! from temporary network issues between specific peer pairs.
//!
//! # How it works
//!
//! 1. Peer A suspects Peer B (direct + indirect pings failed)
//! 2. A assigns a random Peer C as "buddy" for B
//! 3. A sends BuddyRequest(B) to C
//! 4. C pings B directly
//! 5. C sends BuddyResponse(B, alive) back to A
//! 6. If alive, A clears suspicion of B. If not, suspicion continues.

use crate::PeerId;
use std::collections::HashMap;

/// Tracks buddy assignments for suspected peers.
pub struct BuddyTracker {
    /// Maps suspected peer → assigned buddy
    assignments: HashMap<PeerId, BuddyAssignment>,
    /// Maps buddy → list of peers they're verifying
    buddy_to_targets: HashMap<PeerId, Vec<PeerId>>,
}

/// A buddy assignment for verifying a suspected peer.
#[derive(Debug, Clone)]
pub struct BuddyAssignment {
    /// The peer assigned as buddy
    pub buddy: PeerId,
    /// When the request was sent (ms since epoch)
    pub sent_at: u64,
    /// Whether we've received a response
    pub responded: bool,
    /// Whether target was alive (only valid when responded is true)
    pub alive: bool,
}

/// Result of buddy verification.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BuddyVerification {
    /// Buddy confirmed target is alive
    Alive,
    /// Buddy confirmed target is dead
    Dead,
    /// Verification pending (no response yet)
    Pending,
    /// No buddy assigned
    NoBuddy,
}

impl BuddyTracker {
    /// Create a new buddy tracker.
    pub fn new() -> Self {
        Self {
            assignments: HashMap::new(),
            buddy_to_targets: HashMap::new(),
        }
    }

    /// Assign a buddy to verify a suspected peer.
    ///
    /// Returns the previous buddy if one was already assigned.
    pub fn assign(&mut self, suspected: PeerId, buddy: PeerId, now_ms: u64) -> Option<PeerId> {
        let prev = self.remove(suspected);

        self.assignments.insert(
            suspected,
            BuddyAssignment {
                buddy,
                sent_at: now_ms,
                responded: false,
                alive: false,
            },
        );

        self.buddy_to_targets
            .entry(buddy)
            .or_default()
            .push(suspected);

        prev
    }

    /// Remove buddy assignment for a peer.
    ///
    /// Returns the buddy that was assigned, if any.
    pub fn remove(&mut self, suspected: PeerId) -> Option<PeerId> {
        if let Some(assignment) = self.assignments.remove(&suspected) {
            if let Some(targets) = self.buddy_to_targets.get_mut(&assignment.buddy) {
                targets.retain(|&t| t != suspected);
                if targets.is_empty() {
                    self.buddy_to_targets.remove(&assignment.buddy);
                }
            }
            Some(assignment.buddy)
        } else {
            None
        }
    }

    /// Get the buddy assigned to verify a suspected peer.
    pub fn get_buddy(&self, suspected: &PeerId) -> Option<PeerId> {
        self.assignments.get(suspected).map(|a| a.buddy)
    }

    /// Get the assignment details for a suspected peer.
    pub fn get_assignment(&self, suspected: &PeerId) -> Option<&BuddyAssignment> {
        self.assignments.get(suspected)
    }

    /// Check if a peer has a buddy assigned.
    pub fn has_buddy(&self, suspected: &PeerId) -> bool {
        self.assignments.contains_key(suspected)
    }

    /// Get all peers a buddy is responsible for verifying.
    pub fn get_targets_for_buddy(&self, buddy: &PeerId) -> Vec<PeerId> {
        self.buddy_to_targets
            .get(buddy)
            .cloned()
            .unwrap_or_default()
    }

    /// Record a buddy response.
    ///
    /// Returns the suspected peer if valid, None if buddy wasn't assigned.
    pub fn record_response(&mut self, buddy: PeerId, target: PeerId, alive: bool) -> Option<PeerId> {
        if let Some(assignment) = self.assignments.get_mut(&target) {
            if assignment.buddy == buddy {
                assignment.responded = true;
                assignment.alive = alive;
                return Some(target);
            }
        }
        None
    }

    /// Check verification status for a suspected peer.
    pub fn verification_status(&self, suspected: &PeerId) -> BuddyVerification {
        match self.assignments.get(suspected) {
            Some(assignment) if assignment.responded => {
                if assignment.alive {
                    BuddyVerification::Alive
                } else {
                    BuddyVerification::Dead
                }
            }
            Some(_) => BuddyVerification::Pending,
            None => BuddyVerification::NoBuddy,
        }
    }

    /// Get assignments that have timed out.
    ///
    /// Returns suspected peers whose buddy hasn't responded within timeout.
    pub fn timed_out(&self, now_ms: u64, timeout_ms: u64) -> Vec<PeerId> {
        self.assignments
            .iter()
            .filter(|(_, a)| !a.responded && now_ms.saturating_sub(a.sent_at) >= timeout_ms)
            .map(|(peer_id, _)| *peer_id)
            .collect()
    }

    /// Number of active buddy assignments.
    pub fn len(&self) -> usize {
        self.assignments.len()
    }

    /// Check if there are no active assignments.
    pub fn is_empty(&self) -> bool {
        self.assignments.is_empty()
    }

    /// Clear all buddy assignments.
    pub fn clear(&mut self) {
        self.assignments.clear();
        self.buddy_to_targets.clear();
    }
}

impl Default for BuddyTracker {
    fn default() -> Self {
        Self::new()
    }
}

/// Select a buddy for verifying a suspected peer.
///
/// Picks a random alive peer that is:
/// - Not the suspected peer
/// - Not ourselves
/// - Preferably not already a buddy for many peers (load balancing)
pub fn select_buddy<'a, I>(
    suspected: PeerId,
    local_peer_id: PeerId,
    candidates: I,
    tracker: &BuddyTracker,
) -> Option<PeerId>
where
    I: Iterator<Item = &'a PeerId>,
{
    use rand::seq::IndexedRandom;

    // Filter to valid candidates
    let valid: Vec<_> = candidates
        .filter(|&&peer_id| peer_id != suspected && peer_id != local_peer_id)
        .copied()
        .collect();

    if valid.is_empty() {
        return None;
    }

    // Prefer peers with fewer existing buddy assignments (load balancing)
    // For simplicity, just pick randomly from the bottom half by load
    let mut with_load: Vec<_> = valid
        .iter()
        .map(|&peer_id| (peer_id, tracker.get_targets_for_buddy(&peer_id).len()))
        .collect();

    with_load.sort_by_key(|(_, load)| *load);

    // Take bottom half (lower load)
    let half = (with_load.len() / 2).max(1);
    let low_load: Vec<_> = with_load.iter().take(half).map(|(id, _)| *id).collect();

    low_load.choose(&mut rand::rng()).copied()
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

    fn peer_d() -> PeerId {
        "0000000000000001".parse().unwrap()
    }

    // ==================== Basic assignment ====================

    #[test]
    fn test_assign_buddy() {
        let mut tracker = BuddyTracker::new();

        let prev = tracker.assign(peer_a(), peer_b(), 1000);

        assert!(prev.is_none());
        assert!(tracker.has_buddy(&peer_a()));
        assert_eq!(tracker.get_buddy(&peer_a()), Some(peer_b()));
        assert_eq!(tracker.len(), 1);
    }

    #[test]
    fn test_reassign_buddy() {
        let mut tracker = BuddyTracker::new();

        tracker.assign(peer_a(), peer_b(), 1000);
        let prev = tracker.assign(peer_a(), peer_c(), 2000);

        assert_eq!(prev, Some(peer_b()));
        assert_eq!(tracker.get_buddy(&peer_a()), Some(peer_c()));
        assert_eq!(tracker.len(), 1);
    }

    #[test]
    fn test_remove_buddy() {
        let mut tracker = BuddyTracker::new();

        tracker.assign(peer_a(), peer_b(), 1000);
        let removed = tracker.remove(peer_a());

        assert_eq!(removed, Some(peer_b()));
        assert!(!tracker.has_buddy(&peer_a()));
        assert!(tracker.is_empty());
    }

    #[test]
    fn test_remove_nonexistent() {
        let mut tracker = BuddyTracker::new();
        let removed = tracker.remove(peer_a());
        assert!(removed.is_none());
    }

    // ==================== Buddy-to-targets mapping ====================

    #[test]
    fn test_buddy_targets() {
        let mut tracker = BuddyTracker::new();

        // peer_c is buddy for both peer_a and peer_b
        tracker.assign(peer_a(), peer_c(), 1000);
        tracker.assign(peer_b(), peer_c(), 1000);

        let targets = tracker.get_targets_for_buddy(&peer_c());
        assert_eq!(targets.len(), 2);
        assert!(targets.contains(&peer_a()));
        assert!(targets.contains(&peer_b()));
    }

    #[test]
    fn test_buddy_targets_after_remove() {
        let mut tracker = BuddyTracker::new();

        tracker.assign(peer_a(), peer_c(), 1000);
        tracker.assign(peer_b(), peer_c(), 1000);
        tracker.remove(peer_a());

        let targets = tracker.get_targets_for_buddy(&peer_c());
        assert_eq!(targets.len(), 1);
        assert!(!targets.contains(&peer_a()));
        assert!(targets.contains(&peer_b()));
    }

    // ==================== Response recording ====================

    #[test]
    fn test_record_response() {
        let mut tracker = BuddyTracker::new();

        tracker.assign(peer_a(), peer_b(), 1000);

        let result = tracker.record_response(peer_b(), peer_a(), true);
        assert_eq!(result, Some(peer_a()));

        let assignment = tracker.get_assignment(&peer_a()).unwrap();
        assert!(assignment.responded);
        assert!(assignment.alive);
    }

    #[test]
    fn test_record_response_wrong_buddy() {
        let mut tracker = BuddyTracker::new();

        tracker.assign(peer_a(), peer_b(), 1000);

        // peer_c wasn't assigned as buddy
        let result = tracker.record_response(peer_c(), peer_a(), true);
        assert!(result.is_none());
    }

    #[test]
    fn test_record_response_no_assignment() {
        let mut tracker = BuddyTracker::new();

        let result = tracker.record_response(peer_b(), peer_a(), true);
        assert!(result.is_none());
    }

    // ==================== Verification status ====================

    #[test]
    fn test_verification_status_no_buddy() {
        let tracker = BuddyTracker::new();
        assert_eq!(tracker.verification_status(&peer_a()), BuddyVerification::NoBuddy);
    }

    #[test]
    fn test_verification_status_pending() {
        let mut tracker = BuddyTracker::new();
        tracker.assign(peer_a(), peer_b(), 1000);
        assert_eq!(tracker.verification_status(&peer_a()), BuddyVerification::Pending);
    }

    #[test]
    fn test_verification_status_alive() {
        let mut tracker = BuddyTracker::new();
        tracker.assign(peer_a(), peer_b(), 1000);
        tracker.record_response(peer_b(), peer_a(), true);
        assert_eq!(tracker.verification_status(&peer_a()), BuddyVerification::Alive);
    }

    #[test]
    fn test_verification_status_dead() {
        let mut tracker = BuddyTracker::new();
        tracker.assign(peer_a(), peer_b(), 1000);
        tracker.record_response(peer_b(), peer_a(), false);
        assert_eq!(tracker.verification_status(&peer_a()), BuddyVerification::Dead);
    }

    // ==================== Timeout detection ====================

    #[test]
    fn test_timed_out() {
        let mut tracker = BuddyTracker::new();

        tracker.assign(peer_a(), peer_b(), 1000);
        tracker.assign(peer_c(), peer_d(), 1500);

        // At 3000, only peer_a has timed out (2000ms timeout)
        let timed_out = tracker.timed_out(3000, 2000);
        assert_eq!(timed_out.len(), 1);
        assert!(timed_out.contains(&peer_a()));

        // At 4000, both have timed out
        let timed_out = tracker.timed_out(4000, 2000);
        assert_eq!(timed_out.len(), 2);
    }

    #[test]
    fn test_timed_out_with_response() {
        let mut tracker = BuddyTracker::new();

        tracker.assign(peer_a(), peer_b(), 1000);
        tracker.record_response(peer_b(), peer_a(), true);

        // Response received, so not timed out
        let timed_out = tracker.timed_out(5000, 2000);
        assert!(timed_out.is_empty());
    }

    // ==================== Buddy selection ====================

    #[test]
    fn test_select_buddy_excludes_suspected() {
        let tracker = BuddyTracker::new();
        let local = peer_d();
        let candidates = vec![peer_a(), peer_b(), peer_c()];

        // Select buddy for peer_a - should not be peer_a
        let buddy = select_buddy(peer_a(), local, candidates.iter(), &tracker);

        assert!(buddy.is_some());
        assert_ne!(buddy.unwrap(), peer_a());
    }

    #[test]
    fn test_select_buddy_excludes_local() {
        let tracker = BuddyTracker::new();
        let local = peer_b();
        let candidates = vec![peer_a(), peer_b(), peer_c()];

        // Select buddy for peer_a - should not be local (peer_b)
        for _ in 0..20 {
            let buddy = select_buddy(peer_a(), local, candidates.iter(), &tracker);
            assert!(buddy.is_some());
            assert_ne!(buddy.unwrap(), peer_b());
        }
    }

    #[test]
    fn test_select_buddy_no_candidates() {
        let tracker = BuddyTracker::new();
        let local = peer_a();
        let candidates: Vec<PeerId> = vec![];

        let buddy = select_buddy(peer_b(), local, candidates.iter(), &tracker);
        assert!(buddy.is_none());
    }

    #[test]
    fn test_select_buddy_only_invalid_candidates() {
        let tracker = BuddyTracker::new();
        let local = peer_a();
        // Only candidates are local and suspected
        let candidates = vec![peer_a(), peer_b()];

        let buddy = select_buddy(peer_b(), local, candidates.iter(), &tracker);
        assert!(buddy.is_none());
    }

    #[test]
    fn test_select_buddy_load_balancing() {
        let mut tracker = BuddyTracker::new();
        let local = peer_d();

        // peer_c already has 2 assignments, peer_b has 0
        tracker.assign(peer_a(), peer_c(), 1000);
        tracker.assign("1111111111111111".parse().unwrap(), peer_c(), 1000);

        let candidates = vec![peer_b(), peer_c()];

        // Run multiple times - should prefer peer_b (lower load)
        let mut b_count = 0;
        let mut c_count = 0;
        for _ in 0..50 {
            if let Some(buddy) = select_buddy(peer_a(), local, candidates.iter(), &tracker) {
                if buddy == peer_b() {
                    b_count += 1;
                } else {
                    c_count += 1;
                }
            }
        }

        // peer_b should be selected most of the time (load balancing)
        assert!(b_count > c_count, "Expected more b ({}) than c ({})", b_count, c_count);
    }

    // ==================== Clear ====================

    #[test]
    fn test_clear() {
        let mut tracker = BuddyTracker::new();

        tracker.assign(peer_a(), peer_b(), 1000);
        tracker.assign(peer_c(), peer_d(), 1000);

        tracker.clear();

        assert!(tracker.is_empty());
        assert!(!tracker.has_buddy(&peer_a()));
        assert!(tracker.get_targets_for_buddy(&peer_b()).is_empty());
    }
}
