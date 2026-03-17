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
  let isAdding = false;
  let peers: PeerInfo[] = [];
  let nodeId: string | null = null;
  let bootstrapPeers: string[] = [];
  let errorMessage: string | null = null;
  let disabledReason: string | null = null;

  // Add bootstrap peer form
  let showAddForm = false;
  let addInput = "";

  // Event subscription
  let eventRef: EventRef | null = null;

  // Refresh interval for relative times
  let refreshInterval: number | null = null;

  // Check sync status
  async function checkStatus() {
    isChecking = true;
    errorMessage = null;

    try {
      disabledReason = plugin.disabledReason;
      if (disabledReason) {
        isChecking = false;
        return;
      }

      isInitialized = plugin.isVaultInitialized();
      nodeId = plugin.getNodeId();
      peers = plugin.getConnectedPeers();
      bootstrapPeers = plugin.settings.bootstrapPeers;
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

  // Add a bootstrap peer by NodeId
  async function addBootstrapPeer() {
    const input = addInput.trim();
    if (!input) {
      errorMessage = "Please enter a NodeId";
      return;
    }

    isAdding = true;
    errorMessage = null;

    try {
      await plugin.addBootstrapPeer(input);
      showAddForm = false;
      addInput = "";
      await checkStatus();
    } catch (e) {
      console.error("p2p-sync: Error adding bootstrap peer", e);
      errorMessage = `Failed to add peer: ${e}`;
    } finally {
      isAdding = false;
    }
  }

  async function removeBootstrapPeer(nodeId: string) {
    await plugin.removeBootstrapPeer(nodeId);
    await checkStatus();
  }

  // Format relative time for last activity
  function formatRelativeTime(date: Date): string {
    const seconds = Math.floor((Date.now() - date.getTime()) / 1000);
    if (seconds < 60) return `${seconds}s`;
    const minutes = Math.floor(seconds / 60);
    if (minutes < 60) return `${minutes}m`;
    const hours = Math.floor(minutes / 60);
    return `${hours}h`;
  }

  /** Shorten a 64-char NodeId to first6...last4 for display */
  function truncateNodeId(id: string): string {
    if (id.length <= 10) return id;
    return `${id.slice(0, 6)}...${id.slice(-4)}`;
  }

  function copyNodeId() {
    if (nodeId) {
      navigator.clipboard.writeText(nodeId);
    }
  }

  function toggleAddForm() {
    showAddForm = !showAddForm;
    if (!showAddForm) {
      addInput = "";
    }
  }

  onMount(() => {
    checkStatus();
    eventRef = plugin.events.on("state-changed", () => {
      checkStatus();
    });
    refreshInterval = window.setInterval(() => {
      peers = peers; // Trigger Svelte reactivity
    }, 10000);
  });

  onDestroy(() => {
    if (eventRef) {
      plugin.events.offref(eventRef);
    }
    if (refreshInterval) {
      clearInterval(refreshInterval);
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

      {#if nodeId}
        <div class="p2p-sync-details">
          <div class="p2p-sync-detail-row">
            <span class="p2p-sync-label">Node ID:</span>
            <button class="p2p-sync-url-value" on:click={copyNodeId} title="Click to copy full ID">
              {truncateNodeId(nodeId)}
            </button>
          </div>
        </div>
      {/if}

      <div class="p2p-sync-peers">
        <h4>Connected Peers ({peers.length})</h4>
        {#if peers.length === 0}
          <p class="p2p-sync-muted">No peers in swarm</p>
        {:else}
          <ul class="p2p-sync-peer-list">
            {#each peers as peer}
              <li class="p2p-sync-peer-item">
                <div class="p2p-sync-peer-info">
                  <span class="p2p-sync-peer-status p2p-sync-peer-online"></span>
                  <span class="p2p-sync-peer-address" title={peer.id}>
                    {truncateNodeId(peer.id)}
                  </span>
                </div>
                <div class="p2p-sync-peer-actions">
                  <span class="p2p-sync-peer-activity">
                    {formatRelativeTime(peer.lastActivityAt)}
                  </span>
                </div>
              </li>
            {/each}
          </ul>
        {/if}

        <h4 class="p2p-sync-section-header">Bootstrap Peers</h4>
        {#if bootstrapPeers.length === 0}
          <p class="p2p-sync-muted">No bootstrap peers configured</p>
        {:else}
          <ul class="p2p-sync-peer-list">
            {#each bootstrapPeers as peerId}
              <li class="p2p-sync-peer-item">
                <div class="p2p-sync-peer-info">
                  <span class="p2p-sync-peer-address" title={peerId}>
                    {truncateNodeId(peerId)}
                  </span>
                </div>
                <div class="p2p-sync-peer-actions">
                  <button
                    class="p2p-sync-remove-btn"
                    on:click={() => removeBootstrapPeer(peerId)}
                    title="Remove bootstrap peer"
                  >✕</button>
                </div>
              </li>
            {/each}
          </ul>
        {/if}

        {#if showAddForm}
          <div class="p2p-sync-connect-form">
            <input
              type="text"
              placeholder="Paste Node ID from other device"
              bind:value={addInput}
              class="p2p-sync-input"
            />
            <div class="p2p-sync-connect-buttons">
              <button
                class="p2p-sync-button-primary"
                on:click={addBootstrapPeer}
                disabled={isAdding}
              >
                {isAdding ? "Adding..." : "Add"}
              </button>
              <button
                class="p2p-sync-button-secondary"
                on:click={toggleAddForm}
              >
                Cancel
              </button>
            </div>
          </div>
        {:else}
          <button class="p2p-sync-button-secondary" on:click={toggleAddForm}>
            Add Bootstrap Peer
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

  .p2p-sync-peers {
    margin-top: 24px;
  }

  .p2p-sync-peers h4 {
    font-size: var(--font-ui-small);
    margin-bottom: 8px;
  }

  .p2p-sync-section-header {
    margin-top: 16px;
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

  .p2p-sync-peer-info {
    display: flex;
    align-items: center;
    gap: 6px;
    min-width: 0;
    flex: 1;
  }

  .p2p-sync-peer-status {
    width: 8px;
    height: 8px;
    border-radius: 50%;
    flex-shrink: 0;
  }

  .p2p-sync-peer-online {
    background: var(--text-success);
  }

  .p2p-sync-peer-address {
    font-family: var(--font-monospace);
    font-size: var(--font-ui-smaller);
    overflow: hidden;
    text-overflow: ellipsis;
    white-space: nowrap;
  }

  .p2p-sync-peer-actions {
    display: flex;
    align-items: center;
    gap: 8px;
    flex-shrink: 0;
  }

  .p2p-sync-peer-activity {
    color: var(--text-muted);
    font-size: var(--font-ui-smaller);
  }

  .p2p-sync-remove-btn {
    background: none;
    border: none;
    color: var(--text-muted);
    cursor: pointer;
    padding: 2px 4px;
    font-size: 12px;
    line-height: 1;
    border-radius: 2px;
  }

  .p2p-sync-remove-btn:hover {
    background: var(--background-modifier-hover);
    color: var(--text-error);
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
