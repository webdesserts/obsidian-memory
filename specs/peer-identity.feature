Feature: Peer Identity
  Each sync client has its own network identity (PeerId) for protocol
  operations like SWIM membership, gossip, and message routing. This is
  separate from the vault's author identity (VaultId).

  # --- Daemon ---

  Scenario: Daemon generates and persists a PeerId
    Given a daemon starts for the first time on a vault
    Then a PeerId should be generated and stored in .sync/daemon.toml
    And the PeerId should be used for SWIM membership and handshakes

  Scenario: Daemon reuses its PeerId across restarts
    Given a daemon was previously running with PeerId "66ab5b...9abf"
    When the daemon restarts
    Then it should read the same PeerId from .sync/daemon.toml
    And only one entry should appear in SWIM membership for this daemon

  Scenario: CLI flag overrides persisted PeerId
    Given a daemon has a persisted PeerId
    When the daemon starts with --peer-id "custom123"
    Then it should use "custom123" as its PeerId
    And .sync/daemon.toml should be updated with the new PeerId

  # --- Plugin ---

  Scenario: Plugin generates and persists a PeerId in localStorage
    Given the Obsidian plugin starts for the first time
    Then a PeerId should be generated and stored in localStorage
    And the PeerId should be keyed by vault name

  Scenario: Plugin reuses its PeerId across sessions
    Given the plugin was previously running with PeerId "cbc4f5...d4bd"
    When the plugin restarts
    Then it should use the same PeerId "cbc4f5...d4bd"

  # --- Identity Separation ---

  Scenario: Plugin and daemon on the same vault have different PeerIds
    Given a daemon is running on a vault with PeerId "daemon-id"
    And the plugin opens the same vault with PeerId "plugin-id"
    Then SWIM membership should show two distinct peers
    And both should share the same VaultId for CRDT authoring
