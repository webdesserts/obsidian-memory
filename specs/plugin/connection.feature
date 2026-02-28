Feature: Peer connection
  The sync panel lets users connect to peers by entering an address.
  Bare addresses (without a protocol prefix) default to wss:// since
  WebSocket has no automatic protocol upgrade like HTTP→HTTPS.

  Scenario: Bare hostname gets wss:// prefix
    When the user enters "umbra.computer/sync"
    Then the connection uses "wss://umbra.computer/sync"

  Scenario: Bare IP with port gets wss:// prefix
    When the user enters "192.168.1.100:8765"
    Then the connection uses "wss://192.168.1.100:8765"

  Scenario: Bare hostname without path gets wss:// prefix
    When the user enters "umbra.computer"
    Then the connection uses "wss://umbra.computer"

  Scenario: Explicit wss:// is preserved
    When the user enters "wss://example.com/sync"
    Then the connection uses "wss://example.com/sync"

  Scenario: Explicit ws:// is preserved
    When the user enters "ws://local:8765"
    Then the connection uses "ws://local:8765"
