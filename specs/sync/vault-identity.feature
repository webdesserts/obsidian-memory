Feature: Vault Identity
  Each vault copy has a stable author identity (VaultId) used for CRDT
  operations. This identity is generated once and persisted in
  .sync/metadata.toml, independent of which client accesses the vault.

  Scenario: New vault gets a generated VaultId
    Given I initialize a new vault
    Then a VaultId should be generated and written to .sync/metadata.toml
    And the VaultId should be used as the Loro author identity

  Scenario: Existing vault loads its VaultId
    Given a vault was previously initialized with a VaultId
    When I load the vault
    Then the same VaultId should be read from .sync/metadata.toml
    And Loro documents should use that VaultId as their author

  Scenario: VaultId is shared across clients on the same vault
    Given a vault with VaultId "abc123"
    When the daemon loads the vault
    And the plugin loads the same vault
    Then both should use VaultId "abc123" for CRDT operations

  Scenario: VaultId survives daemon restarts
    Given a daemon initialized a vault with a VaultId
    When the daemon restarts
    Then the vault should load with the same VaultId
    And no duplicate author entries should appear in Loro version vectors

  Scenario: Legacy vault without metadata.toml
    Given a vault's .sync/ directory exists but .sync/metadata.toml is missing
    When I load the vault
    Then a new VaultId should be generated
    And .sync/metadata.toml should be written with version = 1
