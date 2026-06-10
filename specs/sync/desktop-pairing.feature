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

  Scenario: Initiator pairs with a discovered mesh using the two-step flow
    Given the desktop app is running on Device A
    And a separate daemon is advertising mesh "M" on the LAN
    When the user clicks "Pair with nearby device…" on Device A
    Then a window opens with mesh "M" in the dropdown
    And the "Request pairing" button is enabled
    And no code entry step is visible yet
    When the user selects mesh "M" and clicks "Request pairing"
    Then the button disables and the mesh dropdown locks
    And the daemon connects to Device B, triggering B to display its 6-digit code
    And stage 2 becomes visible with the prompt "Enter the code shown on <Device B>"
    When the user enters the 6-digit code shown on Device B and clicks Pair
    Then the window confirms "Paired with <Device B>. Closing…" and closes
    And Device A's allowlist contains Device B's PeerId
    And Device A adopts mesh "M"'s VaultId in .sync/metadata.toml
    And Device A re-joins the gossip topic for mesh "M"
    And Device A pulls Device B's notes into its configured vault folder

  Scenario: Requesting a code reveals the code entry step
    Given the initiator window is open with mesh "M" discovered
    When the user selects mesh "M" and clicks "Request pairing"
    Then the daemon fires RequestPairing for that vault_id
    And on connect success the stage 2 code input appears
    And the code input is focused
    And the prompt names the responder device

  Scenario: Re-requesting after a failed connect starts fresh
    Given the initiator window is open with a mesh selected
    And the user clicks "Request pairing" but the connect fails
    Then an error is shown in the status area
    And the "Request pairing" button re-enables
    And the mesh dropdown unlocks
    When the user clicks "Request pairing" again
    Then a new RequestPairing command is sent cancelling the prior attempt

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

  Scenario: Initiator surfaces a clear error when no discovered peer is available at request time
    Given the user clicks "Request pairing" for a mesh that has no discovered peers
    Then RequestPairing fails with a "no discovered peers" message
    And the "Request pairing" button re-enables so the user can retry

  Scenario: Submitting a code without a prior RequestPairing returns a clear error
    Given the user somehow invokes SubmitCode without a prior RequestPairing
    Then the daemon replies with "no pairing request in progress"
    And the daemon does not hang

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
