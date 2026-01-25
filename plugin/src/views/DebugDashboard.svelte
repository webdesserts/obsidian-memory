<script lang="ts">
  import type P2PSyncPlugin from "../main";
  import type { PeerInfo } from "../network";
  import type { SyncEvent, RegistryStats, VersionVector } from "../wasm";
  import type { EventRef } from "obsidian";
  import { Platform } from "obsidian";
  import { onMount, onDestroy } from "svelte";

  export let plugin: P2PSyncPlugin;

  // State
  let isLoading = true;
  let error: string | null = null;

  // Self info
  let peerId: string | null = null;
  let serverStatus: string = "Not running";
  let fileCount: number = 0;

  // Registry stats
  let registryStats: RegistryStats | null = null;

  // Version vector
  let versionVector: VersionVector | null = null;

  // Connected peers (from TypeScript PeerManager)
  let connectedPeers: PeerInfo[] = [];

  // Events are read from plugin.debugEvents (persists across modal opens)
  $: recentEvents = plugin.debugEvents;

  // Plugin event subscription
  let eventRef: EventRef | null = null;

  /**
   * Truncate a 16-char peer ID to a readable format: first6...last4
   */
  function truncatePeerId(id: string): string {
    if (id.length <= 10) return id;
    return `${id.slice(0, 6)}...${id.slice(-4)}`;
  }

  /**
   * Format a timestamp for display.
   */
  function formatTime(timestamp: number): string {
    const date = new Date(timestamp);
    return date.toLocaleTimeString("en-US", {
      hour12: false,
      hour: "2-digit",
      minute: "2-digit",
      second: "2-digit",
    });
  }

  /**
   * Get a human-readable label for an event type.
   */
  function getEventLabel(event: SyncEvent): string {
    switch (event.type) {
      case "messageReceived":
        return `Received: ${event.messageType} (${formatBytes(event.size)})`;
      case "messageSent":
        return `Sent: ${event.messageType} (${formatBytes(event.size)})`;
      case "documentUpdated":
        return `Updated: ${event.path}`;
      case "fileOp":
        if (event.operation === "rename" && event.newPath) {
          return `Renamed: ${event.path} → ${event.newPath}`;
        }
        return `${capitalize(event.operation)}: ${event.path}`;
      case "peerConnected":
        return `Peer connected: ${truncatePeerId(event.peerId)}`;
      case "peerDisconnected":
        return `Peer disconnected: ${truncatePeerId(event.peerId)}`;
      default:
        return "Unknown event";
    }
  }

  function capitalize(s: string): string {
    return s.charAt(0).toUpperCase() + s.slice(1);
  }

  function formatBytes(bytes: number): string {
    if (bytes < 1024) return `${bytes}B`;
    if (bytes < 1024 * 1024) return `${(bytes / 1024).toFixed(1)}KB`;
    return `${(bytes / 1024 / 1024).toFixed(1)}MB`;
  }

  /**
   * Load all debug data from WASM and TypeScript APIs.
   */
  async function loadDebugData() {
    isLoading = true;
    error = null;

    // Check if plugin is disabled
    if (plugin.disabledReason) {
      error = plugin.disabledReason;
      isLoading = false;
      return;
    }

    const vault = plugin.vault;
    if (!vault) {
      error = "Vault not initialized";
      isLoading = false;
      return;
    }

    try {
      // WASM calls (synchronous debug APIs)
      peerId = vault.peerId();
      registryStats = vault.getRegistryStats() as RegistryStats;
      versionVector = vault.getRegistryVersion() as VersionVector;

      // Async WASM call
      const files = await vault.listFiles();
      fileCount = Array.isArray(files) ? files.length : 0;

      // TypeScript API: server status
      if (!Platform.isDesktop) {
        serverStatus = "N/A (mobile)";
      } else if (plugin.peerManager?.isServerRunning) {
        serverStatus = `Running on :${plugin.peerManager.port}`;
      } else {
        serverStatus = "Not running";
      }

      // TypeScript API: connected peers
      connectedPeers = plugin.peerManager?.getConnectedPeers() ?? [];
    } catch (e) {
      const message = e instanceof Error ? e.message : String(e);
      console.error("p2p-sync: Error loading debug data", e);
      error = `Error: ${message}`;
    } finally {
      isLoading = false;
    }
  }

  /**
   * Sort version vector entries by counter (descending).
   */
  function sortedVersionEntries(vv: VersionVector): Array<[string, number]> {
    return Object.entries(vv).sort((a, b) => b[1] - a[1]);
  }

  /**
   * Clear the event buffer.
   */
  function clearEvents() {
    plugin.clearDebugEvents();
    // Trigger reactivity
    recentEvents = plugin.debugEvents;
  }

  onMount(() => {
    loadDebugData();

    // Subscribe to plugin state changes (includes new events)
    eventRef = plugin.events.on("state-changed", () => {
      // Refresh events from plugin buffer
      recentEvents = plugin.debugEvents;
      // Also refresh peer data (in case peers changed)
      connectedPeers = plugin.peerManager?.getConnectedPeers() ?? [];
    });
  });

  onDestroy(() => {
    if (eventRef) {
      plugin.events.offref(eventRef);
    }
  });
</script>

<div class="debug-dashboard">
  {#if isLoading}
    <div class="debug-loading">Loading debug data...</div>
  {:else if error}
    <div class="debug-error">
      <div class="debug-error-icon">!</div>
      <div class="debug-error-message">{error}</div>
    </div>
    <button class="debug-refresh-btn" on:click={loadDebugData}>Refresh</button>
  {:else}
    <!-- Refresh button -->
    <div class="debug-actions">
      <button class="debug-refresh-btn" on:click={loadDebugData}>Refresh</button>
    </div>

    <!-- Self Info Section -->
    <section class="debug-section">
      <h3 class="debug-section-title">Self</h3>
      <div class="debug-table">
        <div class="debug-row">
          <span class="debug-label">Peer ID</span>
          <code class="debug-value" title={peerId ?? ""}>{peerId ?? "N/A"}</code>
        </div>
        <div class="debug-row">
          <span class="debug-label">Server</span>
          <span class="debug-value">{serverStatus}</span>
        </div>
        <div class="debug-row">
          <span class="debug-label">Files</span>
          <span class="debug-value">{fileCount}</span>
        </div>
      </div>
    </section>

    <!-- Registry Stats Section -->
    <section class="debug-section">
      <h3 class="debug-section-title">Registry</h3>
      <div class="debug-table">
        <div class="debug-row">
          <span class="debug-label">Changes</span>
          <span class="debug-value">{registryStats?.changeCount ?? 0}</span>
        </div>
        <div class="debug-row">
          <span class="debug-label">Operations</span>
          <span class="debug-value">{registryStats?.opCount ?? 0}</span>
        </div>
      </div>
    </section>

    <!-- Version Vector Section -->
    <section class="debug-section">
      <h3 class="debug-section-title">Version Vector</h3>
      {#if versionVector && Object.keys(versionVector).length > 0}
        <div class="debug-version-table">
          <div class="debug-version-header">
            <span>Peer ID</span>
            <span>Counter</span>
          </div>
          {#each sortedVersionEntries(versionVector) as [pId, counter]}
            <div class="debug-version-row">
              <code class="debug-peer-id" title={pId}>{truncatePeerId(pId)}</code>
              <span class="debug-counter">{counter}</span>
            </div>
          {/each}
        </div>
      {:else}
        <div class="debug-empty">No version data</div>
      {/if}
    </section>

    <!-- Connected Peers Section -->
    <section class="debug-section">
      <h3 class="debug-section-title">Connected Peers</h3>
      {#if connectedPeers.length > 0}
        <div class="debug-peers-list">
          {#each connectedPeers as peer}
            <div class="debug-peer-item">
              <div class="debug-peer-main">
                <span class="debug-direction-badge" class:incoming={peer.direction === "incoming"}>
                  {peer.direction}
                </span>
                <code class="debug-peer-id" title={peer.id}>{truncatePeerId(peer.id)}</code>
              </div>
              <div class="debug-peer-address">{peer.address}</div>
            </div>
          {/each}
        </div>
      {:else}
        <div class="debug-empty">No peers connected</div>
      {/if}
    </section>

    <!-- Recent Events Section -->
    <section class="debug-section">
      <div class="debug-section-header">
        <h3 class="debug-section-title">Recent Events</h3>
        {#if recentEvents.length > 0}
          <button class="debug-clear-btn" on:click={clearEvents}>Clear</button>
        {/if}
      </div>
      {#if recentEvents.length > 0}
        <div class="debug-events-list">
          {#each recentEvents as { event, id } (id)}
            <div class="debug-event-item">
              <span class="debug-event-time">{formatTime(event.timestamp)}</span>
              <span class="debug-event-label">{getEventLabel(event)}</span>
            </div>
          {/each}
        </div>
      {:else}
        <div class="debug-empty">No events yet</div>
      {/if}
    </section>
  {/if}
</div>

<style>
  .debug-dashboard {
    padding: 16px;
    max-height: 70vh;
    overflow-y: auto;
  }

  .debug-loading {
    text-align: center;
    color: var(--text-muted);
    padding: 24px;
  }

  .debug-error {
    display: flex;
    align-items: flex-start;
    gap: 12px;
    padding: 16px;
    background: var(--background-modifier-error);
    border-radius: 8px;
    margin-bottom: 16px;
  }

  .debug-error-icon {
    font-weight: bold;
    color: var(--text-on-accent);
    background: var(--text-error);
    width: 24px;
    height: 24px;
    border-radius: 50%;
    display: flex;
    align-items: center;
    justify-content: center;
    flex-shrink: 0;
  }

  .debug-error-message {
    color: var(--text-normal);
    line-height: 1.5;
  }

  .debug-actions {
    display: flex;
    justify-content: flex-end;
    margin-bottom: 16px;
  }

  .debug-refresh-btn {
    padding: 6px 12px;
    border-radius: 4px;
    background: var(--interactive-normal);
    color: var(--text-normal);
    border: 1px solid var(--background-modifier-border);
    cursor: pointer;
    font-size: var(--font-ui-small);
  }

  .debug-refresh-btn:hover {
    background: var(--interactive-hover);
  }

  .debug-section {
    margin-bottom: 20px;
  }

  .debug-section-header {
    display: flex;
    justify-content: space-between;
    align-items: center;
  }

  .debug-section-title {
    font-size: var(--font-ui-small);
    font-weight: 600;
    color: var(--text-muted);
    text-transform: uppercase;
    letter-spacing: 0.05em;
    margin-bottom: 8px;
    padding-bottom: 4px;
    border-bottom: 1px solid var(--background-modifier-border);
  }

  .debug-section-header .debug-section-title {
    border-bottom: none;
    margin-bottom: 0;
    padding-bottom: 0;
  }

  .debug-clear-btn {
    padding: 2px 8px;
    border-radius: 4px;
    background: transparent;
    color: var(--text-muted);
    border: 1px solid var(--background-modifier-border);
    cursor: pointer;
    font-size: var(--font-ui-smaller);
  }

  .debug-clear-btn:hover {
    background: var(--background-modifier-hover);
    color: var(--text-normal);
  }

  .debug-table {
    display: flex;
    flex-direction: column;
    gap: 4px;
  }

  .debug-row {
    display: flex;
    justify-content: space-between;
    align-items: center;
    padding: 4px 0;
  }

  .debug-label {
    color: var(--text-muted);
    font-size: var(--font-ui-small);
  }

  .debug-value {
    font-size: var(--font-ui-small);
  }

  code.debug-value {
    font-family: var(--font-monospace);
    background: var(--background-secondary);
    padding: 2px 6px;
    border-radius: 4px;
  }

  .debug-empty {
    color: var(--text-muted);
    font-size: var(--font-ui-small);
    font-style: italic;
    padding: 8px 0;
  }

  /* Version Vector Table */
  .debug-version-table {
    display: flex;
    flex-direction: column;
    gap: 2px;
    font-size: var(--font-ui-smaller);
  }

  .debug-version-header {
    display: flex;
    justify-content: space-between;
    color: var(--text-muted);
    padding: 4px 8px;
    font-weight: 500;
  }

  .debug-version-row {
    display: flex;
    justify-content: space-between;
    align-items: center;
    padding: 4px 8px;
    background: var(--background-secondary);
    border-radius: 4px;
  }

  .debug-peer-id {
    font-family: var(--font-monospace);
    font-size: var(--font-ui-smaller);
  }

  .debug-counter {
    font-family: var(--font-monospace);
    color: var(--text-accent);
  }

  /* Connected Peers List */
  .debug-peers-list {
    display: flex;
    flex-direction: column;
    gap: 8px;
  }

  .debug-peer-item {
    padding: 8px 12px;
    background: var(--background-secondary);
    border-radius: 6px;
  }

  .debug-peer-main {
    display: flex;
    align-items: center;
    gap: 8px;
    margin-bottom: 4px;
  }

  .debug-direction-badge {
    font-size: 10px;
    padding: 2px 6px;
    border-radius: 4px;
    background: var(--text-success);
    color: var(--text-on-accent);
    text-transform: uppercase;
    font-weight: 600;
  }

  .debug-direction-badge.incoming {
    background: var(--text-accent);
  }

  .debug-peer-address {
    font-family: var(--font-monospace);
    font-size: var(--font-ui-smaller);
    color: var(--text-muted);
    word-break: break-all;
  }

  /* Events List */
  .debug-events-list {
    display: flex;
    flex-direction: column;
    gap: 2px;
    max-height: 200px;
    overflow-y: auto;
    font-size: var(--font-ui-smaller);
    margin-top: 8px;
    border-top: 1px solid var(--background-modifier-border);
    padding-top: 8px;
  }

  .debug-event-item {
    display: flex;
    gap: 12px;
    padding: 4px 8px;
    background: var(--background-secondary);
    border-radius: 4px;
  }

  .debug-event-time {
    font-family: var(--font-monospace);
    color: var(--text-muted);
    flex-shrink: 0;
  }

  .debug-event-label {
    color: var(--text-normal);
    word-break: break-word;
  }
</style>
