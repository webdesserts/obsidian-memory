//! Membership list for tracking known peers.
//!
//! The membership list is the core data structure for SWIM. It tracks:
//! - All known peers in the mesh
//! - Their current state (Alive, Suspected, Dead)
//! - Incarnation numbers for conflict resolution

use super::{GossipUpdate, PeerInfo};
use crate::PeerId;
use std::collections::HashMap;

/// Maximum number of pending gossip updates before oldest are dropped
const MAX_GOSSIP_QUEUE_SIZE: usize = 100;

/// State of a member in the membership list.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MemberState {
    /// Peer is believed to be alive
    Alive,
    /// Peer failed to respond, might be dead
    Suspected,
    /// Peer confirmed dead (failed to refute suspicion)
    Dead,
    /// Peer explicitly removed from mesh
    Removed,
}

/// A member in the membership list.
#[derive(Debug, Clone)]
pub struct Member {
    /// Peer information
    pub info: PeerInfo,
    /// Current state
    pub state: MemberState,
    /// Incarnation number (increases when refuting suspicion)
    pub incarnation: u64,
    /// Which peer told us about this member (for debugging/tracing)
    pub discovered_via: Option<PeerId>,
}

impl Member {
    /// Create a new alive member.
    pub fn new(info: PeerInfo, incarnation: u64) -> Self {
        Self {
            info,
            state: MemberState::Alive,
            incarnation,
            discovered_via: None,
        }
    }

    /// Create a new member discovered via another peer.
    pub fn discovered_from(info: PeerInfo, incarnation: u64, via: PeerId) -> Self {
        Self {
            info,
            state: MemberState::Alive,
            incarnation,
            discovered_via: Some(via),
        }
    }

    /// Check if this member is connectable (has an address).
    pub fn is_server(&self) -> bool {
        self.info.address.is_some()
    }

    /// Check if this member is client-only (no address).
    pub fn is_client_only(&self) -> bool {
        self.info.address.is_none()
    }
}

/// Membership list tracking all known peers.
///
/// This is the core data structure for SWIM gossip. It handles:
/// - Adding/removing members
/// - Processing gossip updates
/// - Generating gossip to send
pub struct MembershipList {
    /// Our own peer ID
    local_peer_id: PeerId,
    /// Our incarnation number (increases when we refute suspicion)
    local_incarnation: u64,
    /// Our advertised address (None = client-only)
    local_address: Option<String>,
    /// All known members indexed by peer ID
    members: HashMap<PeerId, Member>,
    /// Pending gossip updates to propagate
    pending_gossip: Vec<GossipUpdate>,
    /// Maximum gossip updates to piggyback per message
    gossip_fanout: usize,
}

impl MembershipList {
    /// Create a new membership list with default incarnation (1).
    pub fn new(local_peer_id: PeerId, local_address: Option<String>) -> Self {
        Self::with_incarnation(local_peer_id, local_address, 1)
    }

    /// Create a new membership list with a specific incarnation number.
    ///
    /// Use this when restoring from persisted state to maintain incarnation
    /// continuity across restarts.
    pub fn with_incarnation(
        local_peer_id: PeerId,
        local_address: Option<String>,
        incarnation: u64,
    ) -> Self {
        Self {
            local_peer_id,
            local_incarnation: incarnation,
            local_address,
            members: HashMap::new(),
            pending_gossip: Vec::new(),
            gossip_fanout: 3,
        }
    }

    /// Get our local peer ID.
    pub fn local_peer_id(&self) -> PeerId {
        self.local_peer_id
    }

    /// Get our local incarnation.
    pub fn local_incarnation(&self) -> u64 {
        self.local_incarnation
    }

    /// Get our local PeerInfo.
    pub fn local_info(&self) -> PeerInfo {
        PeerInfo::new(self.local_peer_id, self.local_address.clone())
    }

    /// Set the gossip fanout (max updates per message).
    pub fn set_gossip_fanout(&mut self, fanout: usize) {
        self.gossip_fanout = fanout;
    }

    /// Update our local address after construction.
    ///
    /// Use this when the server port is only known after startup.
    pub fn set_local_address(&mut self, address: String) {
        self.local_address = Some(address);
    }

    /// Add a member to the list.
    ///
    /// Returns true if this is a new member or state changed.
    pub fn add(&mut self, info: PeerInfo, incarnation: u64) -> bool {
        self.add_impl(info, incarnation, None)
    }

    /// Add a member discovered from another peer.
    pub fn add_discovered(&mut self, info: PeerInfo, incarnation: u64, via: PeerId) -> bool {
        self.add_impl(info, incarnation, Some(via))
    }

    fn add_impl(&mut self, info: PeerInfo, incarnation: u64, via: Option<PeerId>) -> bool {
        let peer_id = info.peer_id;

        // Ignore updates about ourselves
        if peer_id == self.local_peer_id {
            return false;
        }

        if let Some(existing) = self.members.get_mut(&peer_id) {
            // Never resurrect Removed peers via gossip - they must reconnect directly
            if existing.state == MemberState::Removed {
                return false;
            }

            // Update if incarnation is higher
            if incarnation > existing.incarnation {
                existing.info = info;
                existing.incarnation = incarnation;
                existing.state = MemberState::Alive;
                if via.is_some() {
                    existing.discovered_via = via;
                }
                return true;
            }

            // Same incarnation: handle state transitions and address merging
            if incarnation == existing.incarnation {
                let mut changed = false;

                if existing.state != MemberState::Alive {
                    existing.state = MemberState::Alive;
                    changed = true;
                }

                // Merge address when existing has none (e.g., handshake registered
                // peer without address, then gossip arrives with it)
                if existing.info.address.is_none() && info.address.is_some() {
                    existing.info.address = info.address;
                    changed = true;
                }

                if changed {
                    if via.is_some() {
                        existing.discovered_via = via;
                    }
                }

                return changed;
            }

            false
        } else {
            // New member
            let member = match via {
                Some(v) => Member::discovered_from(info, incarnation, v),
                None => Member::new(info, incarnation),
            };
            self.members.insert(peer_id, member);
            true
        }
    }

    /// Remove a member from the list.
    ///
    /// Returns the removed member if it existed.
    pub fn remove(&mut self, peer_id: PeerId) -> Option<Member> {
        // Don't remove ourselves
        if peer_id == self.local_peer_id {
            return None;
        }
        self.members.remove(&peer_id)
    }

    /// Get a member by peer ID.
    pub fn get(&self, peer_id: &PeerId) -> Option<&Member> {
        self.members.get(peer_id)
    }

    /// Get a mutable reference to a member.
    pub fn get_mut(&mut self, peer_id: &PeerId) -> Option<&mut Member> {
        self.members.get_mut(peer_id)
    }

    /// Check if a peer is in the list.
    pub fn contains(&self, peer_id: &PeerId) -> bool {
        self.members.contains_key(peer_id)
    }

    /// Get all members.
    pub fn members(&self) -> impl Iterator<Item = &Member> {
        self.members.values()
    }

    /// Get all alive members.
    pub fn alive_members(&self) -> impl Iterator<Item = &Member> {
        self.members
            .values()
            .filter(|m| m.state == MemberState::Alive)
    }

    /// Get all members with addresses (server-capable peers).
    pub fn server_members(&self) -> impl Iterator<Item = &Member> {
        self.members.values().filter(|m| m.is_server())
    }

    /// Check if a peer is removed (collective forgetting).
    ///
    /// Returns true if the peer is in the list and marked as Removed.
    /// We should NOT attempt to reconnect to removed peers.
    pub fn is_removed(&self, peer_id: &PeerId) -> bool {
        self.members
            .get(peer_id)
            .map(|m| m.state == MemberState::Removed)
            .unwrap_or(false)
    }

    /// Check if a peer is dead (failure detected).
    ///
    /// Returns true if the peer is in the list and marked as Dead.
    /// Unlike Removed, we MAY attempt to reconnect to dead peers.
    pub fn is_dead(&self, peer_id: &PeerId) -> bool {
        self.members
            .get(peer_id)
            .map(|m| m.state == MemberState::Dead)
            .unwrap_or(false)
    }

    /// Get peers that should be reconnected to.
    /// Returns server peers that we should attempt to reconnect to.
    ///
    /// Includes Alive and Dead peers (failure detection means we should retry).
    /// Excludes Removed peers (collective forgetting means no auto-reconnect).
    /// Excludes client-only peers (they can't accept incoming connections).
    pub fn reconnectable_peers(&self) -> impl Iterator<Item = &Member> {
        self.members.values().filter(|m| {
            m.state != MemberState::Removed
                && m.is_server()
        })
    }

    /// Number of members (excluding ourselves).
    pub fn len(&self) -> usize {
        self.members.len()
    }

    /// Check if the list is empty.
    pub fn is_empty(&self) -> bool {
        self.members.is_empty()
    }

    /// Mark a peer as suspected.
    ///
    /// Returns true if state changed.
    pub fn suspect(&mut self, peer_id: PeerId, incarnation: u64) -> bool {
        if peer_id == self.local_peer_id {
            // We're being suspected - refute by increasing incarnation
            // Use saturating_add to prevent overflow attacks
            self.local_incarnation = self.local_incarnation.max(incarnation).saturating_add(1);
            self.queue_gossip(GossipUpdate::alive(self.local_info(), self.local_incarnation));
            return false;
        }

        if let Some(member) = self.members.get_mut(&peer_id) {
            // Only suspect if incarnation matches and currently alive
            if incarnation >= member.incarnation && member.state == MemberState::Alive {
                member.state = MemberState::Suspected;
                member.incarnation = incarnation;
                return true;
            }
        }
        false
    }

    /// Mark a peer as dead.
    ///
    /// Returns true if state changed.
    pub fn mark_dead(&mut self, peer_id: PeerId) -> bool {
        if peer_id == self.local_peer_id {
            return false;
        }

        if let Some(member) = self.members.get_mut(&peer_id)
            && member.state != MemberState::Dead
        {
            member.state = MemberState::Dead;
            return true;
        }
        false
    }

    /// Mark a peer as dead, only if incarnation matches or is newer.
    ///
    /// This prevents stale dead messages from incorrectly marking alive peers.
    /// Returns true if state changed.
    pub fn mark_dead_with_incarnation(&mut self, peer_id: PeerId, incarnation: u64) -> bool {
        if peer_id == self.local_peer_id {
            return false;
        }

        if let Some(member) = self.members.get_mut(&peer_id) {
            // Only accept if incarnation matches or is newer, and not already dead
            if incarnation >= member.incarnation && member.state != MemberState::Dead {
                member.state = MemberState::Dead;
                member.incarnation = incarnation;
                return true;
            }
        }
        false
    }

    /// Mark a peer as explicitly removed (collective forgetting).
    ///
    /// Automatically queues a `Removed` gossip update for propagation.
    /// Returns true if state changed.
    pub fn mark_removed(&mut self, peer_id: PeerId) -> bool {
        if peer_id == self.local_peer_id {
            return false;
        }

        if let Some(member) = self.members.get_mut(&peer_id)
            && member.state != MemberState::Removed
        {
            member.state = MemberState::Removed;
            // Queue gossip so Removed state spreads through the mesh
            self.queue_gossip(GossipUpdate::removed(peer_id));
            return true;
        }
        false
    }

    /// Queue a gossip update for propagation.
    pub fn queue_gossip(&mut self, update: GossipUpdate) {
        // Drop oldest if queue is full (FIFO eviction)
        if self.pending_gossip.len() >= MAX_GOSSIP_QUEUE_SIZE {
            self.pending_gossip.remove(0);
        }
        self.pending_gossip.push(update);
    }

    /// Get gossip updates to piggyback on the next message.
    ///
    /// Returns up to `gossip_fanout` updates and removes them from the queue.
    pub fn drain_gossip(&mut self) -> Vec<GossipUpdate> {
        let count = self.gossip_fanout.min(self.pending_gossip.len());
        self.pending_gossip.drain(0..count).collect()
    }

    /// Process incoming gossip updates.
    ///
    /// Returns list of newly discovered peers (for auto-connect).
    pub fn process_gossip(&mut self, updates: &[GossipUpdate], from: PeerId) -> Vec<PeerInfo> {
        let mut new_peers = Vec::new();

        for update in updates {
            match update {
                GossipUpdate::Alive { peer, incarnation } => {
                    let was_new = self.add_discovered(peer.clone(), *incarnation, from);
                    if was_new && peer.peer_id != self.local_peer_id {
                        new_peers.push(peer.clone());
                    }
                }
                GossipUpdate::Suspect {
                    peer_id,
                    incarnation,
                } => {
                    self.suspect(*peer_id, *incarnation);
                }
                GossipUpdate::Dead { peer_id, incarnation } => {
                    self.mark_dead_with_incarnation(*peer_id, *incarnation);
                }
                GossipUpdate::Removed { peer_id } => {
                    self.mark_removed(*peer_id);
                }
            }
        }

        new_peers
    }

    /// Generate Alive gossip updates for all known members.
    ///
    /// Used when responding to a new peer to share full membership.
    pub fn generate_full_gossip(&self) -> Vec<GossipUpdate> {
        // Start with ourselves
        let mut updates = vec![GossipUpdate::alive(self.local_info(), self.local_incarnation)];

        // Add all alive members
        for member in self.alive_members() {
            updates.push(GossipUpdate::alive(member.info.clone(), member.incarnation));
        }

        updates
    }

    /// Pick a random alive member for pinging.
    ///
    /// Returns None if no alive members.
    pub fn pick_random_member(&self) -> Option<&Member> {
        use rand::seq::IndexedRandom;

        let alive: Vec<_> = self.alive_members().collect();
        alive.choose(&mut rand::rng()).copied()
    }

    /// Pick K random members for indirect pinging.
    ///
    /// Excludes the target peer. Returns fewer than K if not enough members.
    pub fn pick_k_random_members(&self, k: usize, exclude: PeerId) -> Vec<&Member> {
        use rand::seq::SliceRandom;

        let mut candidates: Vec<_> = self
            .alive_members()
            .filter(|m| m.info.peer_id != exclude)
            .collect();

        candidates.shuffle(&mut rand::rng());
        candidates.truncate(k);
        candidates
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn local_id() -> PeerId {
        "0000000000000001".parse().unwrap()
    }

    fn peer_a() -> PeerId {
        "a1b2c3d4e5f67890".parse().unwrap()
    }

    fn peer_b() -> PeerId {
        "1234567890abcdef".parse().unwrap()
    }

    fn peer_c() -> PeerId {
        "fedcba0987654321".parse().unwrap()
    }

    // ==================== Basic membership operations ====================

    #[test]
    fn test_new_membership_list() {
        let list = MembershipList::new(local_id(), Some("ws://localhost:8080".into()));

        assert_eq!(list.local_peer_id(), local_id());
        assert_eq!(list.local_incarnation(), 1);
        assert!(list.is_empty());
        assert_eq!(list.len(), 0);
    }

    #[test]
    fn test_membership_list_with_incarnation() {
        let list =
            MembershipList::with_incarnation(local_id(), Some("ws://localhost:8080".into()), 42);

        assert_eq!(list.local_peer_id(), local_id());
        assert_eq!(list.local_incarnation(), 42);
        assert!(list.is_empty());
    }

    #[test]
    fn test_set_local_address() {
        let mut list = MembershipList::new(local_id(), None);
        assert!(list.local_info().address.is_none());

        list.set_local_address("ws://192.168.1.10:9427".to_string());
        assert_eq!(
            list.local_info().address,
            Some("ws://192.168.1.10:9427".to_string())
        );
    }

    #[test]
    fn test_add_member() {
        let mut list = MembershipList::new(local_id(), None);

        let info = PeerInfo::new(peer_a(), Some("ws://a:8080".into()));
        let added = list.add(info.clone(), 1);

        assert!(added);
        assert_eq!(list.len(), 1);
        assert!(list.contains(&peer_a()));

        let member = list.get(&peer_a()).unwrap();
        assert_eq!(member.info.peer_id, peer_a());
        assert_eq!(member.incarnation, 1);
        assert_eq!(member.state, MemberState::Alive);
    }

    #[test]
    fn test_add_client_only_member() {
        let mut list = MembershipList::new(local_id(), None);

        let info = PeerInfo::client_only(peer_a());
        list.add(info, 1);

        let member = list.get(&peer_a()).unwrap();
        assert!(member.is_client_only());
        assert!(!member.is_server());
    }

    #[test]
    fn test_add_server_member() {
        let mut list = MembershipList::new(local_id(), None);

        let info = PeerInfo::new(peer_a(), Some("ws://a:8080".into()));
        list.add(info, 1);

        let member = list.get(&peer_a()).unwrap();
        assert!(member.is_server());
        assert!(!member.is_client_only());
    }

    #[test]
    fn test_add_duplicate_same_incarnation() {
        let mut list = MembershipList::new(local_id(), None);

        let info = PeerInfo::new(peer_a(), Some("ws://a:8080".into()));
        list.add(info.clone(), 1);

        // Same incarnation doesn't change state
        let added = list.add(info, 1);
        assert!(!added);
        assert_eq!(list.len(), 1);
    }

    #[test]
    fn test_add_duplicate_higher_incarnation() {
        let mut list = MembershipList::new(local_id(), None);

        let info = PeerInfo::new(peer_a(), Some("ws://old:8080".into()));
        list.add(info, 1);

        // Higher incarnation updates the member
        let new_info = PeerInfo::new(peer_a(), Some("ws://new:8080".into()));
        let added = list.add(new_info, 2);

        assert!(added);
        let member = list.get(&peer_a()).unwrap();
        assert_eq!(member.incarnation, 2);
        assert_eq!(member.info.address, Some("ws://new:8080".into()));
    }

    #[test]
    fn test_add_duplicate_lower_incarnation_ignored() {
        let mut list = MembershipList::new(local_id(), None);

        let info = PeerInfo::new(peer_a(), Some("ws://a:8080".into()));
        list.add(info, 5);

        // Lower incarnation is ignored
        let old_info = PeerInfo::new(peer_a(), Some("ws://old:8080".into()));
        let added = list.add(old_info, 3);

        assert!(!added);
        let member = list.get(&peer_a()).unwrap();
        assert_eq!(member.incarnation, 5);
    }

    #[test]
    fn test_add_ignores_self() {
        let mut list = MembershipList::new(local_id(), None);

        let info = PeerInfo::new(local_id(), Some("ws://self:8080".into()));
        let added = list.add(info, 1);

        assert!(!added);
        assert!(list.is_empty());
    }

    #[test]
    fn test_remove_member() {
        let mut list = MembershipList::new(local_id(), None);

        list.add(PeerInfo::new(peer_a(), None), 1);
        list.add(PeerInfo::new(peer_b(), None), 1);

        let removed = list.remove(peer_a());
        assert!(removed.is_some());
        assert_eq!(list.len(), 1);
        assert!(!list.contains(&peer_a()));
        assert!(list.contains(&peer_b()));
    }

    #[test]
    fn test_remove_nonexistent() {
        let mut list = MembershipList::new(local_id(), None);
        let removed = list.remove(peer_a());
        assert!(removed.is_none());
    }

    #[test]
    fn test_remove_self_ignored() {
        let mut list = MembershipList::new(local_id(), None);
        let removed = list.remove(local_id());
        assert!(removed.is_none());
    }

    // ==================== Member state transitions ====================

    #[test]
    fn test_suspect_member() {
        let mut list = MembershipList::new(local_id(), None);

        list.add(PeerInfo::new(peer_a(), None), 1);
        let changed = list.suspect(peer_a(), 1);

        assert!(changed);
        let member = list.get(&peer_a()).unwrap();
        assert_eq!(member.state, MemberState::Suspected);
    }

    #[test]
    fn test_suspect_with_old_incarnation_ignored() {
        let mut list = MembershipList::new(local_id(), None);

        list.add(PeerInfo::new(peer_a(), None), 5);
        let changed = list.suspect(peer_a(), 3);

        assert!(!changed);
        let member = list.get(&peer_a()).unwrap();
        assert_eq!(member.state, MemberState::Alive);
    }

    #[test]
    fn test_suspect_self_refutes() {
        let mut list = MembershipList::new(local_id(), Some("ws://local:8080".into()));
        assert_eq!(list.local_incarnation(), 1);

        // Someone suspects us with incarnation 5
        let changed = list.suspect(local_id(), 5);

        // We should refute by increasing our incarnation
        assert!(!changed);
        assert_eq!(list.local_incarnation(), 6);

        // Should queue an Alive gossip to broadcast
        let gossip = list.drain_gossip();
        assert_eq!(gossip.len(), 1);
        if let GossipUpdate::Alive { incarnation, .. } = &gossip[0] {
            assert_eq!(*incarnation, 6);
        } else {
            panic!("Expected Alive gossip");
        }
    }

    #[test]
    fn test_mark_dead() {
        let mut list = MembershipList::new(local_id(), None);

        list.add(PeerInfo::new(peer_a(), None), 1);
        let changed = list.mark_dead(peer_a());

        assert!(changed);
        let member = list.get(&peer_a()).unwrap();
        assert_eq!(member.state, MemberState::Dead);
    }

    #[test]
    fn test_mark_dead_already_dead() {
        let mut list = MembershipList::new(local_id(), None);

        list.add(PeerInfo::new(peer_a(), None), 1);
        list.mark_dead(peer_a());
        let changed = list.mark_dead(peer_a());

        assert!(!changed);
    }

    #[test]
    fn test_mark_dead_self_ignored() {
        let mut list = MembershipList::new(local_id(), None);
        let changed = list.mark_dead(local_id());
        assert!(!changed);
    }

    #[test]
    fn test_mark_removed() {
        let mut list = MembershipList::new(local_id(), None);

        list.add(PeerInfo::new(peer_a(), None), 1);
        let changed = list.mark_removed(peer_a());

        assert!(changed);
        let member = list.get(&peer_a()).unwrap();
        assert_eq!(member.state, MemberState::Removed);
    }

    #[test]
    fn test_alive_refutes_suspicion() {
        let mut list = MembershipList::new(local_id(), None);

        list.add(PeerInfo::new(peer_a(), None), 1);
        list.suspect(peer_a(), 1);

        // Higher incarnation Alive refutes suspicion
        let changed = list.add(PeerInfo::new(peer_a(), None), 2);

        assert!(changed);
        let member = list.get(&peer_a()).unwrap();
        assert_eq!(member.state, MemberState::Alive);
        assert_eq!(member.incarnation, 2);
    }

    // ==================== Gossip processing ====================

    #[test]
    fn test_process_gossip_alive() {
        let mut list = MembershipList::new(local_id(), None);

        let updates = vec![GossipUpdate::alive(
            PeerInfo::new(peer_a(), Some("ws://a:8080".into())),
            1,
        )];

        let new_peers = list.process_gossip(&updates, peer_b());

        assert_eq!(new_peers.len(), 1);
        assert_eq!(new_peers[0].peer_id, peer_a());
        assert!(list.contains(&peer_a()));
    }

    #[test]
    fn test_process_gossip_sets_discovered_via() {
        let mut list = MembershipList::new(local_id(), None);

        let updates = vec![GossipUpdate::alive(PeerInfo::client_only(peer_a()), 1)];
        list.process_gossip(&updates, peer_b());

        let member = list.get(&peer_a()).unwrap();
        assert_eq!(member.discovered_via, Some(peer_b()));
    }

    #[test]
    fn test_process_gossip_suspect() {
        let mut list = MembershipList::new(local_id(), None);

        list.add(PeerInfo::new(peer_a(), None), 1);

        let updates = vec![GossipUpdate::suspect(peer_a(), 1)];
        list.process_gossip(&updates, peer_b());

        let member = list.get(&peer_a()).unwrap();
        assert_eq!(member.state, MemberState::Suspected);
    }

    #[test]
    fn test_process_gossip_dead() {
        let mut list = MembershipList::new(local_id(), None);

        list.add(PeerInfo::new(peer_a(), None), 1);

        let updates = vec![GossipUpdate::dead(peer_a(), 1)];
        list.process_gossip(&updates, peer_b());

        let member = list.get(&peer_a()).unwrap();
        assert_eq!(member.state, MemberState::Dead);
    }

    #[test]
    fn test_process_gossip_removed() {
        let mut list = MembershipList::new(local_id(), None);

        list.add(PeerInfo::new(peer_a(), None), 1);

        let updates = vec![GossipUpdate::removed(peer_a())];
        list.process_gossip(&updates, peer_b());

        let member = list.get(&peer_a()).unwrap();
        assert_eq!(member.state, MemberState::Removed);
    }

    #[test]
    fn test_process_gossip_about_self_ignored() {
        let mut list = MembershipList::new(local_id(), None);

        let updates = vec![GossipUpdate::alive(PeerInfo::client_only(local_id()), 1)];
        let new_peers = list.process_gossip(&updates, peer_a());

        assert!(new_peers.is_empty());
        assert!(list.is_empty());
    }

    // ==================== Gossip generation ====================

    #[test]
    fn test_generate_full_gossip() {
        let mut list = MembershipList::new(local_id(), Some("ws://local:8080".into()));

        list.add(PeerInfo::new(peer_a(), Some("ws://a:8080".into())), 1);
        list.add(PeerInfo::new(peer_b(), None), 2);

        let gossip = list.generate_full_gossip();

        // Should include ourselves + 2 members
        assert_eq!(gossip.len(), 3);

        // First should be ourselves
        if let GossipUpdate::Alive { peer, incarnation } = &gossip[0] {
            assert_eq!(peer.peer_id, local_id());
            assert_eq!(*incarnation, 1);
        } else {
            panic!("Expected Alive for self");
        }
    }

    #[test]
    fn test_generate_full_gossip_excludes_dead() {
        let mut list = MembershipList::new(local_id(), None);

        list.add(PeerInfo::new(peer_a(), None), 1);
        list.add(PeerInfo::new(peer_b(), None), 1);
        list.mark_dead(peer_a());

        let gossip = list.generate_full_gossip();

        // Should include ourselves + 1 alive member (not dead)
        assert_eq!(gossip.len(), 2);
    }

    #[test]
    fn test_drain_gossip() {
        let mut list = MembershipList::new(local_id(), None);

        list.queue_gossip(GossipUpdate::alive(PeerInfo::client_only(peer_a()), 1));
        list.queue_gossip(GossipUpdate::alive(PeerInfo::client_only(peer_b()), 1));

        let gossip = list.drain_gossip();
        assert_eq!(gossip.len(), 2);

        // Queue should be empty now
        let gossip2 = list.drain_gossip();
        assert!(gossip2.is_empty());
    }

    #[test]
    fn test_drain_gossip_respects_fanout() {
        let mut list = MembershipList::new(local_id(), None);
        list.set_gossip_fanout(2);

        // Queue more than fanout
        list.queue_gossip(GossipUpdate::alive(PeerInfo::client_only(peer_a()), 1));
        list.queue_gossip(GossipUpdate::alive(PeerInfo::client_only(peer_b()), 2));
        list.queue_gossip(GossipUpdate::alive(PeerInfo::client_only(peer_c()), 3));

        let gossip1 = list.drain_gossip();
        assert_eq!(gossip1.len(), 2);

        let gossip2 = list.drain_gossip();
        assert_eq!(gossip2.len(), 1);
    }

    // ==================== Random member selection ====================

    #[test]
    fn test_pick_random_member_empty() {
        let list = MembershipList::new(local_id(), None);
        assert!(list.pick_random_member().is_none());
    }

    #[test]
    fn test_pick_random_member() {
        let mut list = MembershipList::new(local_id(), None);

        list.add(PeerInfo::new(peer_a(), None), 1);

        let member = list.pick_random_member();
        assert!(member.is_some());
        assert_eq!(member.unwrap().info.peer_id, peer_a());
    }

    #[test]
    fn test_pick_random_excludes_dead() {
        let mut list = MembershipList::new(local_id(), None);

        list.add(PeerInfo::new(peer_a(), None), 1);
        list.mark_dead(peer_a());

        assert!(list.pick_random_member().is_none());
    }

    #[test]
    fn test_pick_k_random_members() {
        let mut list = MembershipList::new(local_id(), None);

        list.add(PeerInfo::new(peer_a(), None), 1);
        list.add(PeerInfo::new(peer_b(), None), 1);
        list.add(PeerInfo::new(peer_c(), None), 1);

        // Exclude peer_a, should get up to 2 from {b, c}
        let members = list.pick_k_random_members(2, peer_a());
        assert_eq!(members.len(), 2);
        assert!(members.iter().all(|m| m.info.peer_id != peer_a()));
    }

    #[test]
    fn test_pick_k_random_fewer_than_k() {
        let mut list = MembershipList::new(local_id(), None);

        list.add(PeerInfo::new(peer_a(), None), 1);

        // Ask for 3, but only 1 available (after excluding nobody)
        let members = list.pick_k_random_members(3, peer_b());
        assert_eq!(members.len(), 1);
    }

    // ==================== Iterators ====================

    #[test]
    fn test_alive_members_iterator() {
        let mut list = MembershipList::new(local_id(), None);

        list.add(PeerInfo::new(peer_a(), None), 1);
        list.add(PeerInfo::new(peer_b(), None), 1);
        list.mark_dead(peer_a());

        let alive: Vec<_> = list.alive_members().collect();
        assert_eq!(alive.len(), 1);
        assert_eq!(alive[0].info.peer_id, peer_b());
    }

    #[test]
    fn test_server_members_iterator() {
        let mut list = MembershipList::new(local_id(), None);

        list.add(PeerInfo::new(peer_a(), Some("ws://a:8080".into())), 1);
        list.add(PeerInfo::client_only(peer_b()), 1);

        let servers: Vec<_> = list.server_members().collect();
        assert_eq!(servers.len(), 1);
        assert_eq!(servers[0].info.peer_id, peer_a());
    }

    // ==================== State query helpers ====================

    #[test]
    fn test_is_removed() {
        let mut list = MembershipList::new(local_id(), None);

        list.add(PeerInfo::new(peer_a(), None), 1);
        assert!(!list.is_removed(&peer_a()));

        list.mark_removed(peer_a());
        assert!(list.is_removed(&peer_a()));
    }

    #[test]
    fn test_is_removed_unknown_peer() {
        let list = MembershipList::new(local_id(), None);
        assert!(!list.is_removed(&peer_a()));
    }

    #[test]
    fn test_is_dead() {
        let mut list = MembershipList::new(local_id(), None);

        list.add(PeerInfo::new(peer_a(), None), 1);
        assert!(!list.is_dead(&peer_a()));

        list.mark_dead(peer_a());
        assert!(list.is_dead(&peer_a()));
    }

    #[test]
    fn test_is_dead_unknown_peer() {
        let list = MembershipList::new(local_id(), None);
        assert!(!list.is_dead(&peer_a()));
    }

    #[test]
    fn test_reconnectable_peers() {
        let mut list = MembershipList::new(local_id(), None);

        // Add server peers (have addresses)
        list.add(PeerInfo::new(peer_a(), Some("ws://a:8080".into())), 1);
        list.add(PeerInfo::new(peer_b(), Some("ws://b:8080".into())), 1);

        // Add client-only peer (no address)
        list.add(PeerInfo::client_only(peer_c()), 1);

        let reconnectable: Vec<_> = list.reconnectable_peers().collect();

        // Should include both server peers, but not client-only
        assert_eq!(reconnectable.len(), 2);
        assert!(reconnectable.iter().any(|m| m.info.peer_id == peer_a()));
        assert!(reconnectable.iter().any(|m| m.info.peer_id == peer_b()));
    }

    #[test]
    fn test_reconnectable_peers_includes_dead() {
        let mut list = MembershipList::new(local_id(), None);

        list.add(PeerInfo::new(peer_a(), Some("ws://a:8080".into())), 1);
        list.add(PeerInfo::new(peer_b(), Some("ws://b:8080".into())), 1);

        // Mark one as dead - should still be reconnectable (failure detection)
        list.mark_dead(peer_a());

        let reconnectable: Vec<_> = list.reconnectable_peers().collect();
        assert_eq!(reconnectable.len(), 2);
    }

    #[test]
    fn test_reconnectable_peers_excludes_removed() {
        let mut list = MembershipList::new(local_id(), None);

        list.add(PeerInfo::new(peer_a(), Some("ws://a:8080".into())), 1);
        list.add(PeerInfo::new(peer_b(), Some("ws://b:8080".into())), 1);

        // Mark one as removed - should NOT be reconnectable (collective forgetting)
        list.mark_removed(peer_a());

        let reconnectable: Vec<_> = list.reconnectable_peers().collect();
        assert_eq!(reconnectable.len(), 1);
        assert_eq!(reconnectable[0].info.peer_id, peer_b());
    }

    // ==================== Bug fix regression tests ====================

    #[test]
    fn test_removed_not_resurrected_by_alive_gossip() {
        let mut list = MembershipList::new(local_id(), None);

        // Add peer and mark as removed (collective forgetting)
        list.add(PeerInfo::new(peer_a(), Some("ws://a:8080".into())), 1);
        list.mark_removed(peer_a());
        assert!(list.is_removed(&peer_a()));

        // Try to resurrect via Alive gossip with higher incarnation
        let changed = list.add(PeerInfo::new(peer_a(), Some("ws://a:8080".into())), 5);

        // Should NOT be resurrected
        assert!(!changed);
        assert!(list.is_removed(&peer_a()));
        let member = list.get(&peer_a()).unwrap();
        assert_eq!(member.state, MemberState::Removed);
        assert_eq!(member.incarnation, 1); // Incarnation unchanged
    }

    #[test]
    fn test_removed_not_resurrected_by_gossip_processing() {
        let mut list = MembershipList::new(local_id(), None);

        // Add peer and mark as removed
        list.add(PeerInfo::new(peer_a(), Some("ws://a:8080".into())), 1);
        list.mark_removed(peer_a());

        // Process Alive gossip about the removed peer
        let updates = vec![GossipUpdate::alive(
            PeerInfo::new(peer_a(), Some("ws://new:8080".into())),
            10,
        )];
        let new_peers = list.process_gossip(&updates, peer_b());

        // Should not report as new peer and should remain removed
        assert!(new_peers.is_empty());
        assert!(list.is_removed(&peer_a()));
    }

    #[test]
    fn test_gossip_queue_bounded() {
        let mut list = MembershipList::new(local_id(), None);

        // Queue more than MAX_GOSSIP_QUEUE_SIZE updates
        for i in 0..150 {
            let peer_id = format!("{:016x}", i).parse().unwrap();
            list.queue_gossip(GossipUpdate::alive(PeerInfo::client_only(peer_id), 1));
        }

        // Drain all gossip (set high fanout to get everything)
        list.set_gossip_fanout(200);
        let gossip = list.drain_gossip();

        // Should be capped at MAX_GOSSIP_QUEUE_SIZE
        assert_eq!(gossip.len(), MAX_GOSSIP_QUEUE_SIZE);

        // Should have dropped oldest entries (first 50)
        // The remaining entries should be from index 50-149
        if let GossipUpdate::Alive { peer, .. } = &gossip[0] {
            // First entry should be peer 50 (0x32)
            let expected: PeerId = format!("{:016x}", 50).parse().unwrap();
            assert_eq!(peer.peer_id, expected);
        } else {
            panic!("Expected Alive gossip");
        }
    }

    // ==================== mark_removed gossip auto-queue ====================

    #[test]
    fn test_mark_removed_queues_gossip() {
        let mut list = MembershipList::new(local_id(), None);

        list.add(PeerInfo::new(peer_a(), None), 1);
        list.mark_removed(peer_a());

        // Should have queued a Removed gossip update
        let gossip = list.drain_gossip();
        assert_eq!(gossip.len(), 1);
        if let GossipUpdate::Removed { peer_id } = &gossip[0] {
            assert_eq!(*peer_id, peer_a());
        } else {
            panic!("Expected Removed gossip, got {:?}", gossip[0]);
        }
    }

    #[test]
    fn test_removed_gossip_spreads_to_other_peers() {
        // Alice marks Carol as Removed
        let mut alice = MembershipList::new(local_id(), None);
        let carol_id = peer_c();

        alice.add(PeerInfo::new(carol_id, Some("ws://carol:8080".into())), 1);
        alice.mark_removed(carol_id);

        // Alice sends gossip to Bob
        let gossip = alice.drain_gossip();

        // Bob processes the gossip
        let mut bob = MembershipList::new(peer_b(), None);
        bob.add(PeerInfo::new(carol_id, Some("ws://carol:8080".into())), 1);

        bob.process_gossip(&gossip, local_id());

        // Bob should now have Carol marked as Removed
        assert!(bob.is_removed(&carol_id));
    }

    #[test]
    fn test_mark_removed_no_gossip_for_unknown_peer() {
        let mut list = MembershipList::new(local_id(), None);

        // Mark unknown peer as removed - should not queue gossip
        list.mark_removed(peer_a());

        let gossip = list.drain_gossip();
        assert!(gossip.is_empty());
    }

    // ==================== Same-incarnation address merge ====================

    #[test]
    fn test_same_incarnation_merges_address_when_existing_has_none() {
        let mut list = MembershipList::new(local_id(), None);

        // Add peer without address (e.g., from handshake without address)
        list.add(PeerInfo::client_only(peer_a()), 1);
        assert!(list.get(&peer_a()).unwrap().info.address.is_none());

        // Gossip arrives with address at same incarnation
        let changed = list.add(PeerInfo::new(peer_a(), Some("ws://a:8080".into())), 1);

        assert!(changed);
        assert_eq!(
            list.get(&peer_a()).unwrap().info.address,
            Some("ws://a:8080".into())
        );
    }

    #[test]
    fn test_same_incarnation_does_not_overwrite_existing_address() {
        let mut list = MembershipList::new(local_id(), None);

        // Add peer with address
        list.add(PeerInfo::new(peer_a(), Some("ws://original:8080".into())), 1);

        // Another update at same incarnation with different address
        let changed = list.add(PeerInfo::new(peer_a(), Some("ws://different:8080".into())), 1);

        assert!(!changed);
        assert_eq!(
            list.get(&peer_a()).unwrap().info.address,
            Some("ws://original:8080".into())
        );
    }

    #[test]
    fn test_same_incarnation_does_not_clear_address_with_none() {
        let mut list = MembershipList::new(local_id(), None);

        // Add peer with address
        list.add(PeerInfo::new(peer_a(), Some("ws://a:8080".into())), 1);

        // Gossip arrives without address at same incarnation
        let changed = list.add(PeerInfo::client_only(peer_a()), 1);

        assert!(!changed);
        assert_eq!(
            list.get(&peer_a()).unwrap().info.address,
            Some("ws://a:8080".into())
        );
    }

    #[test]
    fn test_process_gossip_returns_peer_when_address_merged() {
        let mut list = MembershipList::new(local_id(), None);

        // Add peer without address (e.g., registered during handshake)
        list.add(PeerInfo::client_only(peer_a()), 1);

        // Gossip arrives with address — should return peer for auto-connect
        let updates = vec![GossipUpdate::alive(
            PeerInfo::new(peer_a(), Some("ws://a:8080".into())),
            1,
        )];
        let new_peers = list.process_gossip(&updates, peer_b());

        assert_eq!(new_peers.len(), 1);
        assert_eq!(new_peers[0].peer_id, peer_a());
        assert_eq!(new_peers[0].address, Some("ws://a:8080".into()));
    }

    #[test]
    fn test_same_incarnation_merges_address_even_when_suspected() {
        let mut list = MembershipList::new(local_id(), None);

        // Add peer without address, then suspect it
        list.add(PeerInfo::client_only(peer_a()), 1);
        list.suspect(peer_a(), 1);
        assert_eq!(list.get(&peer_a()).unwrap().state, MemberState::Suspected);

        // Gossip arrives with address at same incarnation — should merge address AND transition to Alive
        let changed = list.add(PeerInfo::new(peer_a(), Some("ws://a:8080".into())), 1);

        assert!(changed);
        let member = list.get(&peer_a()).unwrap();
        assert_eq!(member.state, MemberState::Alive);
        assert_eq!(member.info.address, Some("ws://a:8080".into()));
    }

    #[test]
    fn test_mark_removed_no_duplicate_gossip() {
        let mut list = MembershipList::new(local_id(), None);

        list.add(PeerInfo::new(peer_a(), None), 1);
        list.mark_removed(peer_a());

        // Drain first gossip
        let _ = list.drain_gossip();

        // Second call should not queue another gossip (already Removed)
        list.mark_removed(peer_a());
        let gossip = list.drain_gossip();
        assert!(gossip.is_empty());
    }
}
