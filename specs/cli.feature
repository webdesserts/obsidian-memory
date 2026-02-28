Feature: CLI Commands
  The `memory` binary provides a unified CLI for managing MCP and sync
  services. There are two primary modes of operation, plus a future third:

  - **Client mode**: The MCP server runs on-demand via stdio, launched by
    the agent's MCP config. The Obsidian plugin handles sync independently.
    The user never interacts with the CLI directly — the agent does.

  - **Daemon mode**: The sync daemon and MCP HTTP server run together as
    long-lived services on a home server or VPS. The user starts them once
    and they stay running.

  - **Daemon-client mode** (future): The sync daemon runs continuously,
    but the MCP server is launched on-demand via stdio by the agent. The
    agent config either connects to a localhost HTTP server or invokes the
    CLI which manages a shared server behind the scenes.

  # --- Client Mode ---
  # The agent's MCP config invokes `memory mcp io --vault ~/notes` to start
  # an on-demand stdio session. No sync daemon needed — the Obsidian plugin
  # handles it.

  Scenario: Agent launches MCP server via stdio
    Given an agent MCP config that runs `memory mcp io --vault ~/notes`
    When the agent starts a session
    Then the MCP server should start with stdio transport
    And it should accept MCP protocol messages on stdin/stdout
    And it should exit when the agent disconnects

  Scenario: User runs `memory mcp` interactively
    When I run `memory mcp` in a terminal
    Then it should display help for MCP subcommands (io, up)
    And it should not start any server

  Scenario: Missing --vault shows error
    When I run `memory mcp io` without --vault
    Then it should display an error about the missing --vault flag
    And it should not start any server

  # --- Daemon Mode ---
  # On a home server or VPS, the user starts all services with one command.
  # Both sync and MCP HTTP run together, sharing the vault via filesystem.

  Scenario: Start all services on a home server
    Given a vault exists at "/home/user/notes"
    When I run `memory up --vault /home/user/notes`
    Then the sync daemon should start and listen for peer connections
    And the MCP HTTP server should start and listen for requests
    And both should run concurrently in the foreground

  Scenario: Start only the sync daemon
    Given a vault exists at "/home/user/notes"
    When I run `memory sync up --vault /home/user/notes`
    Then the sync daemon should start and listen for peer connections
    And the MCP server should not start

  Scenario: Start only the MCP HTTP server
    When I run `memory mcp up --vault ~/notes`
    Then the MCP server should start with HTTP transport
    And it should listen on 127.0.0.1:3000 by default
    And the sync daemon should not start

  Scenario: Graceful shutdown on Ctrl+C
    Given all services are running via `memory up`
    When I send SIGINT (Ctrl+C)
    Then both the sync daemon and MCP server should shut down gracefully

  # --- Configuration ---

  Scenario: Custom MCP HTTP listen address
    When I run `memory mcp up --vault ~/notes --listen 0.0.0.0:8080`
    Then the MCP server should listen on 0.0.0.0:8080

  Scenario: Sync daemon with bootstrap peers
    Given a vault exists at "/home/user/notes"
    When I run `memory sync up --vault /home/user/notes --bootstrap ws://peer:8080`
    Then the sync daemon should connect to ws://peer:8080 on startup

  Scenario: Sync daemon with advertised address
    Given a vault exists at "/home/user/notes"
    When I run `memory sync up --vault /home/user/notes --advertise ws://my.server.com:8080`
    Then peers should see the daemon at ws://my.server.com:8080

  # --- Help & Discoverability ---

  Scenario: No subcommand shows help
    When I run `memory` with no arguments
    Then it should display help text listing available subcommands
    And it should not start any server

  Scenario: Namespace shows subcommand help
    When I run `memory sync` with no further arguments
    Then it should display help for sync subcommands (up)

  # --- Plugin Installation ---

  Scenario: Install plugin to auto-discovered vault
    Given Obsidian is installed with one vault at "~/notes"
    When I run `memory obsidian install`
    Then it should create `.obsidian/plugins/obsidian-p2p-sync/` in the vault
    And it should write the bootstrap harness as `main.js`
    And it should write a `manifest.json` with the CLI version
    And it should print success with instructions to enable the plugin

  Scenario: Install plugin with explicit vault path
    When I run `memory obsidian install --vault ~/my-vault`
    Then it should install the plugin to `~/my-vault/.obsidian/plugins/obsidian-p2p-sync/`

  Scenario: Install refuses to overwrite existing plugin without --force
    Given the vault already has a `main.js` that is not the bootstrap harness
    When I run `memory obsidian install --vault ~/notes`
    Then it should warn that a plugin already exists
    And it should exit without modifying any files

  Scenario: Install with --force overwrites existing plugin
    Given the vault already has a `main.js` that is not the bootstrap harness
    When I run `memory obsidian install --vault ~/notes --force`
    Then it should overwrite the existing plugin files
