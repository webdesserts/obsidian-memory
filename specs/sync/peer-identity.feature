Feature: Peer Identity
  Each sync device has its own network identity derived from an ed25519 keypair.
  The PeerId is the device's ed25519 public key (displayed as a 64-char hex string).
  This is separate from the vault's author identity (VaultId).

  # --- Daemon ---

  Scenario: Daemon generates and persists an identity key on first run
    Given a daemon starts for the first time on a vault
    Then a new ed25519 keypair should be generated and stored in .sync/daemon.key
    And the derived PeerId should be written to .sync/daemon.toml
    And the PeerId should be used as the daemon's identity in the gossip swarm

  Scenario: Daemon reuses its identity key across restarts
    Given a daemon was previously running with an identity key
    When the daemon restarts
    Then it should load the same secret key from .sync/daemon.key
    And use the same PeerId derived from that key

  Scenario: CLI flag overrides the default identity key path
    Given a daemon has a persisted identity key at .sync/daemon.key
    When the daemon starts with --identity-key /path/to/other.key
    Then it should load the key from the specified file
    And use the PeerId derived from that key
    And .sync/daemon.toml should reflect the new PeerId

  # --- Plugin ---

  Scenario: Plugin generates and persists an identity key in localStorage
    Given the Obsidian plugin starts for the first time on a vault
    Then a new ed25519 secret key should be generated and stored in localStorage
    And the key should be keyed by vault path

  Scenario: Plugin reuses its identity key across sessions
    Given the plugin was previously running with an identity key
    When the plugin restarts
    Then it should load the same key from localStorage
    And use the same PeerId

  # --- Identity Separation ---

  Scenario: Plugin and daemon on the same vault have different PeerIds
    Given a daemon is running on a vault
    And the plugin opens the same vault
    Then each should have a distinct PeerId
    And both should share the same VaultId for CRDT authoring

  Scenario: PeerId is used as the Loro author identity via FNV-1a hash
    Given a device has a PeerId (64-char hex ed25519 pubkey)
    Then the device derives a non-zero u64 from the PeerId via FNV-1a hash
    And that u64 is used as the Loro CRDT peer ID for all document edits
