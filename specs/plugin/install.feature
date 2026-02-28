Feature: Plugin Installation and Auto-Update
  The `memory obsidian install` command writes a bootstrap harness into the
  vault's plugin directory. On first load, the harness downloads the full
  plugin from GitHub releases. The full plugin then self-updates via GitHub
  releases once per 24 hours.

  # --- Bootstrap Install (CLI) ---

  Scenario: Bootstrap install writes harness and manifest
    Given a vault exists at "~/notes" with a `.obsidian/` directory
    When I run `memory obsidian install --vault ~/notes`
    Then the directory `~/notes/.obsidian/plugins/obsidian-p2p-sync/` should exist
    And `main.js` should contain the bootstrap harness code
    And `manifest.json` should have the CLI's version number
    And it should print instructions to enable the plugin in Obsidian

  Scenario: Overwrite protection
    Given a vault with an existing `main.js` that is not the harness
    When I run `memory obsidian install --vault ~/notes`
    Then it should refuse to overwrite and suggest `--force`

  # --- First-Load Download (Harness) ---

  Scenario: Harness downloads plugin on first load
    Given the bootstrap harness is installed in a vault
    When Obsidian loads the plugin
    Then it should show a "Downloading..." status bar item
    And it should fetch the latest release from GitHub
    And it should download `obsidian-plugin-checksums.json`
    And it should download `obsidian-plugin-main.js`, `obsidian-plugin-manifest.json`, and `obsidian-plugin-ws-server.js`

  Scenario: Checksum verification passes
    Given the harness has downloaded all plugin assets
    When SHA-256 checksums match `obsidian-plugin-checksums.json`
    Then it should write files atomically (`.tmp` suffix, then rename)
    And it should write `ws-server.js` first, then `manifest.json`, then `main.js` last
    And it should show "P2P Sync installed! Restart Obsidian to activate."

  Scenario: Checksum mismatch
    Given the harness has downloaded plugin assets
    When a SHA-256 checksum does not match
    Then it should show an error notice
    And it should not write any files
    And the harness should remain intact for retry on next reload

  Scenario: Download failure
    Given the harness attempts to download from GitHub
    When the network request fails
    Then it should show an error notice with the failure reason
    And the harness should remain intact for retry on next reload

  Scenario: GitHub rate limit
    Given the harness attempts to fetch the latest release
    When GitHub returns 403 or 429
    Then it should show "GitHub rate limit exceeded, try again later"

  # --- Auto-Update (Full Plugin) ---

  Scenario: Auto-update check on plugin load
    Given the full plugin is installed
    When Obsidian loads the plugin
    Then it should check for updates after initialization completes
    And the update check should not block plugin startup

  Scenario: Update available
    Given the plugin checks GitHub and finds a newer version
    When the download and checksum verification succeed
    Then it should write updated files atomically
    And it should show "Updated to vX.Y.Z! Restart Obsidian to apply."

  Scenario: Update rate limiting
    Given the plugin checked for updates less than 24 hours ago
    When Obsidian loads the plugin again
    Then it should skip the update check

  Scenario: Update check silent failure
    Given the plugin checks for updates
    When the network request fails
    Then it should log a warning to console
    And it should not show any notice to the user

  Scenario: Plugin disabled during update
    Given an update download is in progress
    When the user disables the plugin (triggering onunload)
    Then the download should be cancelled
    And no files should be written
