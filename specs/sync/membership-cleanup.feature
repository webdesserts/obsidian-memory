Feature: Membership Cleanup
  Dead SWIM members should be evicted after a timeout to prevent
  unbounded growth of the membership list and stale UI entries.

  Scenario: Dead member is evicted after TTL expires
    Given Peer A is in SWIM membership marked as Dead
    And Peer A has been Dead for longer than the eviction TTL
    When the eviction sweep runs
    Then Peer A should be removed from the membership list
    And Peer A should no longer appear in the debug panel

  Scenario: Dead member is kept before TTL expires
    Given Peer A is in SWIM membership marked as Dead
    And Peer A has been Dead for less than the eviction TTL
    When the eviction sweep runs
    Then Peer A should remain in the membership list

  Scenario: Alive members are not affected by eviction
    Given Peer A is in SWIM membership marked as Alive
    When the eviction sweep runs
    Then Peer A should remain in the membership list

  Scenario: Eviction runs periodically
    Given the daemon is running
    Then the eviction sweep should run on a regular interval
    And each sweep should remove all members past the TTL

  Scenario: Stale gossip does not resurrect evicted members permanently
    Given Peer A was evicted from membership
    When stale gossip arrives claiming Peer A is Alive
    Then Peer A may temporarily re-enter membership as Alive
    But if no actual connection exists, Peer A will eventually be
    marked Dead and evicted again after the TTL
