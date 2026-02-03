Feature: Peer Discovery Through Daemon
  As a user with multiple Obsidian vaults
  I want my plugins to discover each other through a shared daemon
  So that my notes sync across all my devices

  Background:
    Given a daemon running on "leda"
    And Plugin A connected to the daemon
    And Plugin B connected to the daemon

  Scenario: Plugins discover each other via daemon gossip
    When Plugin A opens the debug panel
    Then Plugin A should see Plugin B in the SWIM Membership list
    And Plugin B's entry should show its LAN address

  Scenario: Plugins connect directly after discovery
    Given Plugin A has discovered Plugin B through gossip
    When Plugin A attempts to connect to Plugin B's address
    Then Plugin A should establish a direct P2P connection to Plugin B
    And Plugin A should see Plugin B in Connected Peers

  Scenario: File edits sync between discovered plugins
    Given Plugin A and Plugin B are connected (directly or via daemon)
    When I edit a file in Plugin A's vault
    Then the change should appear in Plugin B's vault

  Scenario: Plugin advertises its server address
    Given Plugin A starts its WebSocket server on port 9427
    When Plugin A connects to the daemon
    Then Plugin A should advertise its LAN address in gossip messages
    And the address should be in the format "ws://<ip>:<port>"

  Scenario: Daemon relays gossip between plugins
    Given Plugin A is connected to the daemon
    When Plugin B connects to the daemon
    Then the daemon should send Plugin B's info to Plugin A
    And the daemon should send Plugin A's info to Plugin B

  Scenario: Gossip before handshake is ignored
    Given Plugin A is connecting to the daemon
    When gossip arrives before handshake completes
    Then the gossip should be dropped
    And no error should occur
