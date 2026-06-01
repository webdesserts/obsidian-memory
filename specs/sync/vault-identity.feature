Feature: Vault Identity
  Each vault has a stable VaultId that groups its replicas: it seeds the gossip
  topic and the mDNS mesh-grouping key, so devices sharing a VaultId discover and
  sync with each other. The VaultId is generated once and persisted in
  .sync/metadata.toml, independent of which client accesses the vault.

  The VaultId is NOT the Loro author. Each device authors its Loro operations
  under its own per-device PeerId (loro_author), so concurrent offline edits
  across devices never collide on OpIds. See [[Loro Peer ID Semantics]].

  Scenario: New vault gets a generated VaultId
    Given I initialize a new vault
    Then a VaultId should be generated and written to .sync/metadata.toml
    And the device should author Loro operations under its own per-device PeerId

  Scenario: Existing vault loads its VaultId
    Given a vault was previously initialized with a VaultId
    When I load the vault
    Then the same VaultId should be read from .sync/metadata.toml
    And the VaultId should seed the gossip topic and mDNS mesh grouping

  Scenario: VaultId is shared across clients on the same vault
    Given a vault with VaultId "abc123"
    When the daemon loads the vault
    And the plugin loads the same vault
    Then both should join the gossip topic seeded by VaultId "abc123"

  Scenario: VaultId survives daemon restarts
    Given a daemon initialized a vault with a VaultId
    When the daemon restarts
    Then the vault should load with the same VaultId
    And it should re-join the same gossip topic

  Scenario: Legacy vault without metadata.toml
    Given a vault's .sync/ directory exists but .sync/metadata.toml is missing
    When I load the vault
    Then a new VaultId should be generated
    And .sync/metadata.toml should be written with version = 1

  Scenario: Pairing initiator adopts the mesh VaultId
    Given a device with its own VaultId pairs into an existing mesh
    When pairing succeeds
    Then the device adopts the mesh's VaultId in .sync/metadata.toml
    And it re-joins the mesh's gossip topic
    And only the initiator adopts — the responder keeps its VaultId
