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
  let lanUrl: string | null = null;
  let disabledReason: string | null = null;

  // Connect form
  let showConnectForm = false;
  let connectInput = "";

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

      // Get LAN URL from plugin
      lanUrl = plugin.getLanUrl();
      
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

  // Check if input is a WebSocket URL
  function isUrl(input: string): boolean {
    const lower = input.toLowerCase().trim();
    return lower.startsWith('ws://') || lower.startsWith('wss://');
  }

  // Connect to peer (smart-detect URL vs IP:port)
  async function connect() {
    const input = connectInput.trim();
    if (!input) {
      errorMessage = "Please enter a URL or IP address";
      return;
    }

    isConnecting = true;
    errorMessage = null;

    try {
      if (isUrl(input)) {
        await plugin.connectToUrl(input);
      } else {
        // Parse as ip:port (default port 8765)
        const [address, portStr] = input.split(':');
        const port = parseInt(portStr, 10) || 8765;
        await plugin.connectToPeer(address, port);
      }
      showConnectForm = false;
      connectInput = "";
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

  function copyLanUrl() {
    if (lanUrl) {
      navigator.clipboard.writeText(lanUrl);
    }
  }

  function toggleConnectForm() {
    showConnectForm = !showConnectForm;
    if (!showConnectForm) {
      connectInput = "";
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
        {#if lanUrl}
          <div class="p2p-sync-detail-row">
            <span class="p2p-sync-label">LAN URL:</span>
            <button class="p2p-sync-url-value" on:click={copyLanUrl} title="Click to copy">
              {lanUrl}
            </button>
          </div>
        {/if}
        <div class="p2p-sync-detail-row">
          <span class="p2p-sync-label">Last sync:</span>
          <span class="p2p-sync-value">{formatTime(lastSyncTime)}</span>
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
                <span class="p2p-sync-peer-address">{peer.address}</span>
              </li>
            {/each}
          </ul>
        {/if}
        
        {#if showConnectForm}
          <div class="p2p-sync-connect-form">
            <input
              type="text"
              placeholder="wss://example.com/sync or 192.168.1.100:8765"
              bind:value={connectInput}
              class="p2p-sync-input"
            />
            <div class="p2p-sync-connect-buttons">
              <button
                class="p2p-sync-button-primary"
                on:click={connect}
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

  .p2p-sync-url-value {
    font-family: var(--font-monospace);
    font-size: var(--font-ui-smaller);
    background: var(--background-secondary);
    padding: 2px 6px;
    border-radius: 4px;
    border: none;
    cursor: pointer;
    color: var(--text-normal);
    max-width: 180px;
    overflow: hidden;
    text-overflow: ellipsis;
    white-space: nowrap;
  }

  .p2p-sync-url-value:hover {
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

  .p2p-sync-peer-address {
    font-family: var(--font-monospace);
    font-size: var(--font-ui-smaller);
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
