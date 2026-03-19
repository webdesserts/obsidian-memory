Feature: Peer Discovery and Sync
  Devices on the same vault discover each other via mDNS on the local network.
  Pairing is required before two devices can sync — an unpaired device is
  rejected even if it can reach the mesh. Once paired, sync happens
  automatically via QUIC streams triggered by gossip notifications.

  # --- Discovery ---

  Scenario: Device advertises its mesh on the LAN via mDNS
    Given a daemon is running with a vault
    Then it should advertise the mesh name and vault ID over mDNS
    And other devices on the same LAN can discover it

  Scenario: Nearby meshes are visible before pairing
    Given two devices are on the same LAN
    And they share the same vault ID
    When a new device listens for nearby meshes
    Then it should see the mesh listed with its name and how many devices are online

  Scenario: Devices with different vault IDs appear as separate meshes
    Given two devices on the same LAN each have different vault IDs
    When a third device discovers nearby meshes
    Then it should see two separate mesh entries, one for each vault ID

  # --- Pairing Flow ---

  Scenario: Pairing a new device to the mesh
    Given a new device discovers a mesh on the LAN
    When it initiates pairing with a mesh member
    Then the mesh member should display a 6-digit confirmation code
    And when the user enters the correct code on the new device
    Then pairing should succeed
    And both devices should be added to each other's allowlist
    And the new device should receive the mesh member list to bootstrap sync

  Scenario: Wrong confirmation code is rejected
    Given a pairing session is in progress
    When the new device submits an incorrect code
    Then pairing should fail with a rejection message
    And the new device should NOT be added to the allowlist

  Scenario: Confirmation code expires after 5 minutes
    Given a pairing session was started 5 minutes ago
    When the new device tries to submit the code
    Then the session should be expired
    And pairing should fail

  Scenario: Only one pairing session at a time
    Given a pairing session is already active on the mesh member
    When a second device initiates a new pairing request
    Then the second request should be rejected
    And the first session should continue unaffected

  Scenario: Rate limiting prevents brute-force code guessing
    Given a device attempts pairing and submits incorrect codes
    When 5 failed attempts occur within 5 minutes
    Then further attempts from that device should be rejected
    And the rate limit should reset after 5 minutes

  Scenario: HMAC is bound to the requesting device's transport identity
    Given a pairing session is active for device A
    When a different device B tries to submit a code computed for device A
    Then the HMAC verification should fail
    And device B should not be paired

  # --- Allowlist Enforcement ---

  Scenario: Unpaired device cannot sync
    Given the mesh has at least one paired device
    When an unpaired device attempts to sync
    Then the sync request should be rejected
    And an error should be logged

  Scenario: Open-until-first-pair: mesh accepts all before any pairing
    Given no devices have been paired yet
    When any device attempts to sync
    Then the sync request should be accepted
    And after the first pairing, all future sync requests require allowlist membership

  Scenario: Allowlist is propagated to all mesh members
    Given two devices are already paired with each other
    When a third device pairs with one of them
    Then the new device should be added to both devices' allowlists
    And all three devices can sync with each other

  Scenario: Trust within the mesh is transitive
    Given device A and device B are paired
    When device B pairs with device C and propagates the allowlist update
    Then device A should accept device C for syncing

  # --- Sync ---

  Scenario: File edit syncs to paired devices
    Given two devices are paired and connected
    When a file is edited on one device
    Then the change notification should be broadcast via gossip
    And the other device should pull the update via a QUIC stream
    And the file should appear updated on the other device

  Scenario: Full sync on reconnect
    Given two paired devices were disconnected
    When one device rejoins the gossip swarm
    Then a full sync should be triggered automatically
    And both devices should converge to the same vault state

  Scenario: Sync works over relay when direct connection is unavailable
    Given two paired devices cannot connect directly (NAT or different networks)
    And a relay server is configured
    Then sync should succeed via the relay
    And the protocol behaves identically to direct QUIC sync

  Scenario: Unpaired device cannot trigger a sync via gossip
    Given the mesh has paired devices
    When an unpaired device broadcasts a change notification
    Then the mesh members should reject the notification
    And no sync should be performed for that message
