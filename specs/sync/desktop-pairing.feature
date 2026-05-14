Feature: Desktop tray pairing
  The Obsidian Memory macOS desktop app surfaces a pairing flow from its menu
  bar tray so a user can add a LAN-reachable device to the vault mesh. The
  same app also receives inbound pairing requests from other devices on the
  network and shows them in a responder window. Pairing is LAN-only in v0.5.x;
  cross-network pairing is deferred.

  # --- Tray status ---

  Scenario: Tray status reflects current peer count
    Given the desktop app is running with no peers
    Then the tray status should read "Status: Idle"
    When a peer joins the gossip swarm
    Then the tray status should read "Status: Connected · 1 peer"
    When a second peer joins
    Then the tray status should read "Status: Connected · 2 peers"
    When all peers leave
    Then the tray status should return to "Status: Idle"

  Scenario: Tray menu exposes a single entry point for outgoing pairing
    Given the desktop app is running
    Then the tray menu should contain a "Pair with nearby device…" item
    And clicking it should open the pairing initiator window

  # --- Initiator flow ---

  Scenario: Initiator pairs with a discovered mesh on the LAN
    Given the desktop app is running on Device A
    And a separate daemon is advertising mesh "M" on the LAN
    When the user clicks "Pair with nearby device…" on Device A
    Then a window opens listing mesh "M" in the dropdown
    When the user enters the 6-digit code shown on Device B
    And clicks Pair
    Then the window confirms success and closes
    And Device A's allowlist contains Device B's PeerId

  Scenario: Initiator scan times out with no meshes nearby
    Given the desktop app is running with no nearby meshes
    When the user clicks "Pair with nearby device…"
    Then the window scans for 10 seconds
    Then it shows "No meshes found."

  Scenario: Initiator window cancels the active session on close
    Given the initiator window is scanning for meshes
    When the user closes the window via the X button or Cmd+W
    Then the daemon receives DaemonCommand::CancelInitiate
    And the active discovery scan terminates
    And starting a fresh pairing session is possible immediately

  Scenario: Initiator surfaces a clear error when no discovered peer matches
    Given the user submits a 6-digit code with no mesh selected
    Or the user submits a code for a mesh that has not yet been discovered
    Then the pairing attempt should fail with a "no discovered peers" message
    And the Pair button should re-enable so the user can retry

  # --- Responder flow ---

  Scenario: Responder window appears when a remote device requests pairing
    Given the desktop app is running on Device B
    And no pairing session is active on Device B
    When a remote device sends a pairing request
    Then a macOS notification "Pair request" with the requesting device name appears
    And a window opens showing the 6-digit code and a 5-minute countdown
    And the user does NOT need to click an "Approve" button
    When the remote device submits the correct code
    Then the responder window briefly displays "Pairing complete." and auto-closes
    And Device B's allowlist contains the remote device

  Scenario: Responder rejects an inbound pairing request
    Given a responder window is showing a 6-digit code
    When the user clicks Reject
    Then the responder window closes
    And the daemon drops the active pairing session
    And the requesting device receives PairingFailed
    And the user can immediately accept a new pairing request

  Scenario: Responder window X button counts as a reject
    Given a responder window is showing a 6-digit code
    When the user closes the window via the X button or Cmd+W
    Then the daemon receives DaemonCommand::RejectInbound
    And the requesting device receives PairingFailed
    And the user can immediately accept a new pairing request

  Scenario: Responder window auto-closes after the 5-minute expiry
    Given a responder window has been showing a code for 5 minutes
    Then the window closes automatically
    And the pairing session terminates without adding any peer

  Scenario: Responder window auto-closes when the daemon reports failure
    Given a responder window is showing a 6-digit code
    When the initiator submits the wrong code
    Then the daemon emits PairingUiEvent::InboundFailed with a reason
    And the responder window briefly displays "Pairing failed: <reason>" and auto-closes

  Scenario: Concurrent pairing sessions are rejected at the daemon
    Given a responder window is already showing a code
    When a second remote device sends a pairing request
    Then the daemon drops the second request immediately
    And no second responder window appears

  # --- macOS notification ---

  Scenario: macOS notification fires only for inbound requests
    Given the desktop app is running
    When an InboundRequest pairing event fires
    Then a single macOS notification posts with the requesting device name
    And no notification fires for InboundCompleted or InboundFailed events

  Scenario: Notification dispatch failure does not block the responder window
    Given the user has denied macOS notification permission for this bundle
    When an InboundRequest pairing event fires
    Then the responder window still opens
    And the failed notification dispatch is logged at WARN

  # --- Relay configuration ---

  Scenario: Relay URL advertises the machine's LAN IP not 0.0.0.0
    Given the desktop app starts with LAN IP 192.168.68.59 detectable
    When the embedded relay starts on 0.0.0.0:3340
    Then daemon.toml's relay_url is "http://192.168.68.59:3340/"
    And peers reading daemon.toml see a routable URL
