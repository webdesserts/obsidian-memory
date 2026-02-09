Feature: Peer Discovery
  A "peer" is any participant in the sync mesh — either an Obsidian plugin
  or a standalone daemon. The discovery protocol treats them identically:
  any peer can run a WebSocket server, advertise an address, and relay
  gossip. Peers without a server (e.g., mobile) are "client-only" and
  can only receive connections, not accept them.

  As a user with multiple devices
  I want my peers to discover each other and connect directly when possible
  So that my notes sync across all my devices

  # --- Address Propagation ---

  Scenario: Peer includes its address in the handshake
    Given Peer A has a WebSocket server at "ws://192.168.1.10:9427"
    When Peer A connects to Peer B
    Then Peer A's handshake should include its advertised address
    And Peer B should register Peer A with that address

  Scenario: Peer registers the address from a received handshake
    Given Peer A has a WebSocket server at "ws://192.168.1.10:9427"
    When Peer A connects to Peer B and sends its handshake
    Then Peer B should add Peer A to its known peers with address "ws://192.168.1.10:9427"

  Scenario: Peer broadcasts a new connection's address to existing connections
    Given Peer A is connected to Peer B
    And Peer C has a WebSocket server at "ws://192.168.1.20:9427"
    When Peer C connects to Peer B
    Then Peer B should broadcast Peer C's address to Peer A
    And Peer A should see Peer C as a known peer with address "ws://192.168.1.20:9427"

  Scenario: Peer sends known addresses to newly connecting peers
    Given Peer A is connected to Peer B
    And Peer A advertises address "ws://192.168.1.10:9427"
    When Peer C connects to Peer B
    Then Peer B should tell Peer C about all known peers
    And Peer C should see Peer A as a known peer with address "ws://192.168.1.10:9427"

  Scenario: Address from gossip is merged when handshake had no address
    Given Peer B registered Peer A without an address
    When Peer B receives gossip from Peer A containing its address
    Then Peer B should update Peer A's entry in its known peers with the address
    And subsequent gossip from Peer B should include Peer A's address

  Scenario: Existing address is not overwritten by a missing address
    Given Peer B knows Peer A's address is "ws://192.168.1.10:9427"
    When Peer B receives gossip about Peer A without an address
    Then Peer A's address should remain "ws://192.168.1.10:9427"

  Scenario: Updated address replaces the old one
    Given Peer B knows Peer A's address is "ws://192.168.1.10:9427"
    When Peer A reconnects with a new address "ws://10.0.0.5:9427"
    Then Peer B should update Peer A's known address to "ws://10.0.0.5:9427"
    And Peer B should gossip the new address to other peers

  Scenario: Connection order does not affect address propagation
    Given Peer A has a WebSocket server at "ws://192.168.1.10:9427"
    And Peer B has a WebSocket server at "ws://192.168.1.20:9427"
    When Peer A connects to Peer C
    And then Peer B connects to Peer C
    Then Peer A should see Peer B as a known peer with address "ws://192.168.1.20:9427"
    And Peer B should see Peer A as a known peer with address "ws://192.168.1.10:9427"

  Scenario: Addresses propagate across multiple hops
    Given Peer A has a WebSocket server at "ws://192.168.1.10:9427"
    And Peer A is connected to Peer B
    And Peer B is connected to Peer C
    But Peer A is NOT connected to Peer C
    When Peer B gossips about Peer A to Peer C
    Then Peer C should see Peer A as a known peer with address "ws://192.168.1.10:9427"

  # --- Direct Connections ---

  Scenario: Auto-connect to newly discovered peer on same LAN
    Given Peer A and Peer B are on the same LAN
    And Peer B has a WebSocket server
    When Peer A discovers Peer B's address through gossip
    Then Peer A should automatically connect to Peer B
    And Peer A should see Peer B in Connected Peers

  Scenario: Auto-connect is skipped for client-only peers
    Given Peer C has no WebSocket server
    When Peer A discovers Peer C through gossip
    Then Peer C should appear as a known peer marked "client-only"
    And Peer A should NOT attempt to connect to Peer C

  Scenario: Auto-connect is skipped for already-connected peers
    Given Peer A is already directly connected to Peer B
    When Peer A receives gossip about Peer B
    Then Peer A should NOT open a duplicate connection to Peer B

  Scenario: Gossip-triggered auto-connect is one-shot
    Given Peer A discovers Peer B's address through gossip
    And Peer B's address is unreachable
    When the connection attempt fails
    Then Peer A should NOT automatically retry the connection
    And Peer B should remain in Peer A's known peers for future reconnection

  # --- Reconnection ---

  Scenario: Periodic sweep retries known but disconnected peers
    Given Peer A was previously connected to Peer B
    And Peer B went offline and was marked Dead
    When the periodic reconnection sweep runs
    Then Peer A should attempt to reconnect to Peer B's last known address

  Scenario: Reconnection uses bounded backoff
    Given Peer A is attempting to reconnect to Peer B
    And the reconnection keeps failing
    Then Peer A should increase the delay between attempts
    And Peer A should eventually stop trying after a timeout

  Scenario: Reconnected peer resumes normal sync
    Given Peer A had marked Peer B as Dead
    When Peer B comes back online and the reconnection sweep succeeds
    Then Peer B should transition back to Alive
    And Peer A and Peer B should sync normally

  # --- Backwards Compatibility ---

  Scenario: Old peer without address in handshake
    Given Peer A is running an older version without address in handshake
    When Peer A connects to Peer B
    Then Peer B should register Peer A without an address
    And Peer A's address should be updated when gossip arrives with it

  # --- Connection Handshake ---

  Scenario: Peer is not connected until handshake completes
    Given Peer A accepts a TCP connection from Peer B
    When Peer B has not yet sent its handshake
    Then Peer B should NOT appear in Peer A's connected peers
    And messages from Peer B should be dropped

  Scenario: Peer is identified by its real peer ID after handshake
    Given Peer A accepts a connection from Peer B
    When Peer B sends a handshake with peer ID "peer-b-uuid"
    Then Peer A should know Peer B as "peer-b-uuid"
    And messages from Peer B should be attributed to "peer-b-uuid"

  Scenario: Connection dropped before handshake has no effect
    Given Peer A accepts a connection
    When the connection closes before handshake completes
    Then no peer should appear or disappear from Peer A's connected peers

  Scenario: Gossip before handshake is ignored
    Given Peer A is connecting to Peer B
    When gossip arrives before handshake completes
    Then the gossip should be dropped
    And no error should occur

  # --- Edge Cases ---

  Scenario: Cross-network peers cannot connect directly
    Given Peer A is on network 192.168.1.x
    And Peer B is on network 10.0.0.x
    And both are connected to Peer C
    When Peer A discovers Peer B's LAN address through gossip
    Then Peer A should attempt to connect to Peer B
    But the connection should fail
    And sync should continue working through Peer C

  # --- Sync ---

  Scenario: File edits sync between connected peers
    Given Peer A and Peer B are connected (directly or through other peers)
    When I edit a file in Peer A's vault
    Then the change should appear in Peer B's vault
