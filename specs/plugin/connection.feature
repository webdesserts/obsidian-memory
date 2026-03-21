Feature: Plugin Connection
  The Obsidian plugin creates an iroh sync node backed by an ed25519 identity
  key stored in localStorage. It joins the vault's gossip topic to sync
  with paired devices, and discovers nearby meshes on the LAN to initiate
  pairing with new devices.

  # --- Identity ---

  Scenario: Plugin creates an identity key on first load
    Given the plugin loads a vault for the first time
    Then a new ed25519 secret key should be generated
    And stored in localStorage keyed by vault path
    And the node's public key (PeerId) should be derived from it

  Scenario: Plugin reuses its identity key across restarts
    Given the plugin has previously stored an identity key for a vault
    When the plugin reloads the vault
    Then it should load the same key from localStorage
    And connect with the same PeerId

  # --- Gossip and Sync ---

  Scenario: Plugin joins gossip on vault open
    Given the plugin has paired devices in its allowlist
    When the plugin opens a vault
    Then it should join the vault's gossip topic
    And use the allowlist members as bootstrap nodes

  Scenario: Plugin receives file changes from peers
    Given the plugin is connected to the gossip swarm
    When a paired device edits a file
    Then the plugin should receive a change notification via gossip
    And pull the updated file content via a QUIC stream

  Scenario: Plugin broadcasts file changes to peers
    Given the plugin is connected to the gossip swarm
    When the user edits a file in Obsidian
    Then the plugin should broadcast a change notification via gossip
    And paired devices should pull the update

  # --- Pairing ---

  Scenario: Plugin discovers nearby meshes via mDNS (desktop only)
    Given the plugin is running on a desktop device
    And another device is advertising a mesh on the same LAN
    Then the plugin should display the nearby mesh in the sync panel

  Scenario: Plugin pairs with a nearby mesh using a confirmation code
    Given the plugin has discovered a nearby mesh
    When the user initiates pairing and enters the code shown on the mesh device
    Then the pairing should succeed
    And the mesh devices should be added to the plugin's allowlist
    And the plugin should be added to the mesh devices' allowlists

  Scenario: Removing a device unpairs it
    Given a device is listed as paired in the sync panel
    When the user removes it
    Then the device should be removed from the local allowlist
    And it should no longer be able to sync with this vault

  # --- Relay ---

  Scenario: Plugin never connects to public relay servers
    Given the plugin is configured with the no-public-relays policy
    When the plugin creates an iroh sync node
    Then the node should use RelayMode::Disabled or a self-hosted relay
    And no traffic should route through iroh's public relay infrastructure

  Scenario: Plugin starts its own relay when daemon relay is unavailable
    Given no daemon is running (no relay_url in daemon.toml)
    When the plugin starts networking
    Then it should start a local iroh-compatible relay server on a dynamic port
    And use that relay URL for its sync node

  Scenario: Plugin uses daemon relay when available
    Given a daemon is running with an embedded relay
    And daemon.toml contains a relay_url
    When the plugin starts networking
    Then it should use the daemon's relay URL for its sync node
    And not start its own relay server

  Scenario: Plugin relay shuts down cleanly on plugin unload
    Given the plugin is hosting its own relay server
    When the plugin is unloaded or Obsidian closes
    Then the relay should send a Restarting frame to connected clients
    And shut down the HTTP server gracefully

  Scenario: Plugin relay startup failure does not block sync
    Given the relay server fails to start (e.g., port bind error)
    When the plugin starts networking
    Then a warning should be logged
    And the sync node should start with RelayMode::Disabled
    And sync should still work for peers reachable via direct QUIC on LAN

  Scenario: Cross-network peers with no shared relay cannot sync
    Given two paired devices are on different networks
    And no shared relay server is reachable from both devices
    When the devices attempt to sync
    Then the sync connection should fail to establish
    And the sync panel should indicate the peer is unreachable
