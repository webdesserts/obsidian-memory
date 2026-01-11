<script lang="ts">
  import type P2PSyncPlugin from "../main";
  import type { PeerInfo } from "../network";
  import type { EventRef } from "obsidian";
  import { onMount, onDestroy } from "svelte";

  export let plugin: P2PSyncPlugin;

  // Reactive state
  let isInitialized = false;
  let isChecking = true;
  let isSyncing = false;
  let isConnecting = false;
  let connectedPeers: PeerInfo[] = [];
  let lastSyncTime: Date | null = null;
  let errorMessage: string | null = null;
  let peerId: string | null = null;
  let serverPort: number = 8765;
  let disabledReason: string | null = null;

  // Connect form
  let showConnectForm = false;
  let connectAddress = "";
  let connectPort = "8765";

  // Event subscription
  let eventRef: EventRef | null = null;

  // Check sync status
  async function checkStatus() {
    isChecking = true;
    errorMessage = null;

    try {
      // Check if plugin is disabled
      disabledReason = plugin.disabledReason;
      if (disabledReason) {
        isChecking = false;
        return;
      }

      // Get peer ID from plugin
      peerId = plugin.peerId;
      serverPort = plugin.getServerPort();
      
      // Check if vault is initialized
      isInitialized = plugin.isVaultInitialized();
      
      // Get connected peers
      connectedPeers = plugin.getConnectedPeers();
    } catch (e) {
      console.error("p2p-sync: Error checking status", e);
      errorMessage = `Error: ${e}`;
    } finally {
      isChecking = false;
    }
  }

  // Initialize sync
  async function initializeSync() {
    isSyncing = true;
    errorMessage = null;

    try {
      await plugin.initializeVault();
      isInitialized = true;
    } catch (e) {
      console.error("p2p-sync: Error initializing", e);
      errorMessage = `Failed to initialize: ${e}`;
    } finally {
      isSyncing = false;
    }
  }

  // Connect to peer
  async function connectToPeer() {
    if (!connectAddress.trim()) {
      errorMessage = "Please enter an IP address";
      return;
    }

    isConnecting = true;
    errorMessage = null;

    try {
      const port = parseInt(connectPort, 10) || 8765;
      await plugin.connectToPeer(connectAddress.trim(), port);
      showConnectForm = false;
      connectAddress = "";
      await checkStatus();
    } catch (e) {
      console.error("p2p-sync: Error connecting to peer", e);
      errorMessage = `Failed to connect: ${e}`;
    } finally {
      isConnecting = false;
    }
  }

  // Manual sync trigger
  async function syncNow() {
    isSyncing = true;
    errorMessage = null;

    try {
      // TODO: Implement actual sync
      await new Promise((resolve) => setTimeout(resolve, 1000));
      lastSyncTime = new Date();
    } catch (e) {
      console.error("p2p-sync: Error syncing", e);
      errorMessage = `Sync failed: ${e}`;
    } finally {
      isSyncing = false;
    }
  }

  function formatTime(date: Date | null): string {
    if (!date) return "Never";
    return date.toLocaleTimeString();
  }

  function formatPeerId(id: string | null): string {
    if (!id) return "Unknown";
    // Show first 8 characters
    return id.substring(0, 8) + "...";
  }

  function copyPeerId() {
    if (peerId) {
      navigator.clipboard.writeText(peerId);
    }
  }

  function toggleConnectForm() {
    showConnectForm = !showConnectForm;
    if (!showConnectForm) {
      connectAddress = "";
      connectPort = "8765";
    }
  }

  onMount(() => {
    checkStatus();
    // Subscribe to state changes from plugin
    eventRef = plugin.events.on("state-changed", () => {
      checkStatus();
    });
  });

  onDestroy(() => {
    if (eventRef) {
      plugin.events.offref(eventRef);
    }
  });
</script>

<div class="p2p-sync-container">
  <div class="nav-header">
    <div class="nav-header-title">P2P Sync</div>
    <div class="nav-header-actions">
      <button
        class="clickable-icon nav-action-button"
        aria-label="Refresh"
        on:click={checkStatus}
        disabled={isChecking}
      >
        <svg
          xmlns="http://www.w3.org/2000/svg"
          width="18"
          height="18"
          viewBox="0 0 24 24"
          fill="none"
          stroke="currentColor"
          stroke-width="2"
          stroke-linecap="round"
          stroke-linejoin="round"
          class="svg-icon"
          class:spinning={isChecking || isSyncing}
        >
          <path d="M21 12a9 9 0 1 1-9-9c2.52 0 4.93 1 6.74 2.74L21 8" />
          <path d="M21 3v5h-5" />
        </svg>
      </button>
    </div>
  </div>

  <div class="p2p-sync-content">
    {#if isChecking}
      <div class="p2p-sync-status">
        <span class="p2p-sync-icon">...</span>
        <span>Checking status...</span>
      </div>
    {:else if disabledReason}
      <div class="p2p-sync-status p2p-sync-disabled">
        <span class="p2p-sync-icon">X</span>
        <span>Sync Disabled</span>
      </div>
      <div class="p2p-sync-disabled-message">
        <p>{disabledReason}</p>
      </div>
    {:else if errorMessage}
      <div class="p2p-sync-status p2p-sync-error">
        <span class="p2p-sync-icon">!</span>
        <span>{errorMessage}</span>
      </div>
    {:else if !isInitialized}
      <div class="p2p-sync-status p2p-sync-warning">
        <span class="p2p-sync-icon">?</span>
        <span>Sync not initialized</span>
      </div>
      
      <div class="p2p-sync-peer-id">
        <span class="p2p-sync-label">Your Peer ID:</span>
        <button class="p2p-sync-peer-id-value" on:click={copyPeerId} title="Click to copy">
          {formatPeerId(peerId)}
        </button>
      </div>
      
      <p class="p2p-sync-description">
        Initialize P2P sync to start syncing this vault with other devices.
      </p>
      <button
        class="mod-cta p2p-sync-button"
        on:click={initializeSync}
        disabled={isSyncing}
      >
        {#if isSyncing}
          Initializing...
        {:else}
          Initialize Sync
        {/if}
      </button>
    {:else}
      <div class="p2p-sync-status p2p-sync-ok">
        <span class="p2p-sync-icon">+</span>
        <span>Sync enabled</span>
      </div>
      
      <div class="p2p-sync-details">
        <div class="p2p-sync-detail-row">
          <span class="p2p-sync-label">Peer ID:</span>
          <button class="p2p-sync-peer-id-value" on:click={copyPeerId} title="Click to copy">
            {formatPeerId(peerId)}
          </button>
        </div>
        <div class="p2p-sync-detail-row">
          <span class="p2p-sync-label">Server port:</span>
          <span class="p2p-sync-value">{serverPort}</span>
        </div>
        <div class="p2p-sync-detail-row">
          <span class="p2p-sync-label">Last sync:</span>
          <span class="p2p-sync-value">{formatTime(lastSyncTime)}</span>
        </div>
        <div class="p2p-sync-detail-row">
          <span class="p2p-sync-label">Connected peers:</span>
          <span class="p2p-sync-value">{connectedPeers.length}</span>
        </div>
      </div>

      <button
        class="p2p-sync-button"
        on:click={syncNow}
        disabled={isSyncing}
      >
        {#if isSyncing}
          Syncing...
        {:else}
          Sync Now
        {/if}
      </button>

      <div class="p2p-sync-peers">
        <h4>Peers</h4>
        {#if connectedPeers.length === 0}
          <p class="p2p-sync-muted">No peers connected</p>
        {:else}
          <ul class="p2p-sync-peer-list">
            {#each connectedPeers as peer}
              <li class="p2p-sync-peer-item">
                <span class="p2p-sync-peer-name">{formatPeerId(peer.id)}</span>
                <span class="p2p-sync-peer-direction">({peer.direction})</span>
              </li>
            {/each}
          </ul>
        {/if}
        
        {#if showConnectForm}
          <div class="p2p-sync-connect-form">
            <input
              type="text"
              placeholder="IP Address (e.g., 192.168.1.100)"
              bind:value={connectAddress}
              class="p2p-sync-input"
            />
            <input
              type="text"
              placeholder="Port"
              bind:value={connectPort}
              class="p2p-sync-input p2p-sync-input-port"
            />
            <div class="p2p-sync-connect-buttons">
              <button
                class="p2p-sync-button-primary"
                on:click={connectToPeer}
                disabled={isConnecting}
              >
                {isConnecting ? "Connecting..." : "Connect"}
              </button>
              <button
                class="p2p-sync-button-secondary"
                on:click={toggleConnectForm}
              >
                Cancel
              </button>
            </div>
          </div>
        {:else}
          <button class="p2p-sync-button-secondary" on:click={toggleConnectForm}>
            Connect to Peer
          </button>
        {/if}
      </div>
    {/if}
  </div>
</div>

<style>
  .p2p-sync-container {
    display: flex;
    flex-direction: column;
    height: 100%;
  }

  .nav-header {
    display: flex;
    align-items: center;
    justify-content: space-between;
    padding: 8px 12px;
    border-bottom: 1px solid var(--background-modifier-border);
  }

  .nav-header-title {
    font-weight: 600;
    font-size: var(--font-ui-small);
  }

  .nav-header-actions {
    display: flex;
    gap: 4px;
  }

  .p2p-sync-content {
    padding: 12px;
    flex: 1;
    overflow-y: auto;
  }

  .p2p-sync-status {
    display: flex;
    align-items: center;
    gap: 8px;
    padding: 8px 12px;
    border-radius: 4px;
    background: var(--background-secondary);
    margin-bottom: 12px;
  }

  .p2p-sync-icon {
    font-size: 16px;
    font-weight: bold;
  }

  .p2p-sync-ok {
    color: var(--text-success);
  }

  .p2p-sync-warning {
    color: var(--text-muted);
  }

  .p2p-sync-error {
    color: var(--text-error);
  }

  .p2p-sync-disabled {
    color: white;
    background: var(--background-modifier-error);
  }

  .p2p-sync-disabled-message {
    padding: 12px;
    background: var(--background-secondary);
    border-radius: 4px;
    margin-bottom: 12px;
  }

  .p2p-sync-disabled-message p {
    color: var(--text-muted);
    font-size: var(--font-ui-small);
    line-height: 1.5;
    margin: 0;
  }

  .p2p-sync-peer-id {
    display: flex;
    align-items: center;
    gap: 8px;
    margin-bottom: 12px;
    font-size: var(--font-ui-small);
  }

  .p2p-sync-peer-id-value {
    font-family: var(--font-monospace);
    background: var(--background-secondary);
    padding: 2px 6px;
    border-radius: 4px;
    border: none;
    cursor: pointer;
    color: var(--text-normal);
  }

  .p2p-sync-peer-id-value:hover {
    background: var(--background-modifier-hover);
  }

  .p2p-sync-description {
    color: var(--text-muted);
    font-size: var(--font-ui-small);
    margin-bottom: 16px;
    line-height: 1.5;
  }

  .p2p-sync-button {
    width: 100%;
    margin-bottom: 12px;
  }

  .p2p-sync-button-primary {
    flex: 1;
  }

  .p2p-sync-button-secondary {
    width: 100%;
    background: var(--background-secondary);
  }

  .p2p-sync-details {
    display: flex;
    flex-direction: column;
    gap: 8px;
    margin-bottom: 16px;
  }

  .p2p-sync-detail-row {
    display: flex;
    justify-content: space-between;
    align-items: center;
    font-size: var(--font-ui-small);
  }

  .p2p-sync-label {
    color: var(--text-muted);
  }

  .p2p-sync-value {
    color: var(--text-normal);
  }

  .p2p-sync-peers {
    margin-top: 24px;
  }

  .p2p-sync-peers h4 {
    font-size: var(--font-ui-small);
    margin-bottom: 8px;
  }

  .p2p-sync-muted {
    color: var(--text-muted);
    font-size: var(--font-ui-small);
    margin-bottom: 8px;
  }

  .p2p-sync-peer-list {
    list-style: none;
    padding: 0;
    margin: 0 0 12px 0;
  }

  .p2p-sync-peer-item {
    display: flex;
    justify-content: space-between;
    align-items: center;
    padding: 6px 8px;
    background: var(--background-secondary);
    border-radius: 4px;
    margin-bottom: 4px;
    font-size: var(--font-ui-small);
  }

  .p2p-sync-peer-name {
    font-family: var(--font-monospace);
  }

  .p2p-sync-peer-direction {
    color: var(--text-muted);
  }

  .p2p-sync-connect-form {
    display: flex;
    flex-direction: column;
    gap: 8px;
    margin-top: 8px;
  }

  .p2p-sync-input {
    width: 100%;
    padding: 8px;
    border: 1px solid var(--background-modifier-border);
    border-radius: 4px;
    background: var(--background-primary);
    color: var(--text-normal);
  }

  .p2p-sync-input-port {
    width: 80px;
  }

  .p2p-sync-connect-buttons {
    display: flex;
    gap: 8px;
  }

  .p2p-sync-connect-buttons .p2p-sync-button-secondary {
    flex: 0 0 auto;
    width: auto;
    padding: 6px 12px;
  }

  .spinning {
    animation: spin 1s linear infinite;
  }

  @keyframes spin {
    from {
      transform: rotate(0deg);
    }
    to {
      transform: rotate(360deg);
    }
  }
</style>
