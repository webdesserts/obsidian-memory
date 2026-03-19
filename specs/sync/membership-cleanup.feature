Feature: Peer Membership
  The sync mesh tracks which devices are currently online or were previously
  seen. Membership is driven entirely by iroh-gossip events — no manual
  heartbeats or TTL sweeps are required.

  # --- State Transitions ---

  Scenario: Device appears when it joins the gossip swarm
    Given no device is currently online
    When a paired device joins the gossip swarm
    Then it should appear in the peer list as online

  Scenario: Device is marked offline when it leaves the gossip swarm
    Given a device is currently online
    When it disconnects from the gossip swarm
    Then it should be marked as offline in the peer list
    And it should remain visible (as offline) rather than disappearing

  Scenario: Device returns to online when it rejoins the gossip swarm
    Given a device was previously online and is now marked offline
    When the device reconnects to the gossip swarm
    Then it should return to online in the peer list

  # --- No Automatic Eviction ---

  Scenario: Offline devices remain in the peer list indefinitely
    Given a device has been offline for a long time
    Then it should still appear in the peer list as offline
    And it should NOT be automatically removed

  Scenario: Removing a device requires explicit unpairing
    Given a device is in the peer list
    When the user removes it via the sync panel
    Then it should be removed from the allowlist
    And it should no longer be able to sync

  # --- Multiple Devices ---

  Scenario: Multiple devices can be online simultaneously
    Given three devices are in the sync mesh
    When all three join the gossip swarm
    Then all three should appear as online

  Scenario: One device going offline does not affect others
    Given three devices are currently online
    When one device disconnects
    Then the disconnected device should be marked offline
    And the other two should remain online and continue syncing
