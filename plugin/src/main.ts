import { Notice, Plugin, Platform, FileSystemAdapter, Events, TFile } from "obsidian";
import { SyncView, VIEW_TYPE_SYNC } from "./views/SyncView";
import { initWasm, isWasmReady, generatePeerId, WasmVault, versionIncludes } from "./wasm";
import { createFsBridge } from "./fs/ObsidianFs";
import { PeerManager, PeerInfo } from "./network";
import { VaultOperationQueue } from "./VaultOperationQueue";
import { log } from "./logger";

/** Result from processSyncMessage */
interface SyncMessageResult {
  response: Uint8Array | null;
  modifiedPaths: string[];
}

/** Key for storing peer ID in local storage */
const PEER_ID_KEY = "p2p-sync-peer-id";

/** Default WebSocket server port */
const DEFAULT_PORT = 8765;

/** Maximum file size to sync (10MB). Files larger than this are skipped. */
const MAX_FILE_SIZE = 10 * 1024 * 1024;

/** Maximum sync message size (50MB). Messages larger than this are rejected. */
const MAX_SYNC_MESSAGE_SIZE = 50 * 1024 * 1024;

/** Minimum time between broadcasts for the same file (ms). Prevents flooding. */
const BROADCAST_THROTTLE_MS = 1000;

/** A saved peer address for auto-reconnect */
export interface KnownPeer {
  /** Full WebSocket URL (ws:// or wss://) */
  url: string;
  /** Display label (hostname:port or full URL for proxied connections) */
  label: string;
}

/** Path to settings file within the vault's .sync directory */
const SETTINGS_PATH = ".sync/settings.json";

/** Plugin settings persisted per-vault in .sync/settings.json */
interface P2PSyncSettings {
  /** Peers to auto-reconnect to on plugin load */
  knownPeers: KnownPeer[];
}

const DEFAULT_SETTINGS: P2PSyncSettings = {
  knownPeers: [],
};

/**
 * Plugin events:
 * - 'state-changed': Emitted when peers connect/disconnect or vault initializes
 */
export default class P2PSyncPlugin extends Plugin {
  /** Event emitter for UI updates */
  readonly events = new Events();

  /** Plugin settings */
  settings: P2PSyncSettings = DEFAULT_SETTINGS;

  /** The vault manager from WASM */
  vault: WasmVault | null = null;

  /** Our unique peer identifier */
  peerId: string | null = null;

  /** Peer connection manager */
  peerManager: PeerManager | null = null;

  /** Status bar element (desktop only) */
  private statusBarEl: HTMLElement | null = null;

  /** 
   * Version vectors of files after they were synced.
   * Used to detect if a file modification is purely from sync (skip re-broadcast)
   * or includes local edits (needs broadcast).
   * 
   * Key: file path
   * Value: encoded version vector bytes from the last sync operation
   */
  private lastSyncedVersions: Map<string, Uint8Array> = new Map();

  /**
   * Whether the plugin is disabled due to Obsidian Sync being active.
   * When true, the plugin will not initialize vault or peer manager.
   */
  disabledReason: string | null = null;

  /**
   * Operation queue to serialize all WASM vault calls.
   * Prevents concurrent &mut self borrows which cause panics.
   */
  private vaultQueue = new VaultOperationQueue();

  /**
   * Timestamp of last broadcast per file path.
   * Used to throttle broadcasts and prevent flooding peers.
   */
  private lastBroadcastTime: Map<string, number> = new Map();

  /**
   * Pending broadcasts that were throttled.
   * Key: file path, Value: timestamp when pending
   */
  private pendingBroadcasts: Map<string, number> = new Map();

  /**
   * Maximum number of entries in Maps to prevent memory leaks.
   */
  private readonly MAX_MAP_ENTRIES = 10000;

  /**
   * Clean up old entries from Maps when they grow too large.
   * Uses approximate LRU by removing oldest entries (for timestamp maps).
   * Only called when Maps exceed MAX_MAP_ENTRIES to prevent unbounded growth.
   */
  private cleanupMaps(): void {
    // Clean timestamp-based maps (can sort by value)
    const cleanupTimestamps = <K>(map: Map<K, number>, maxSize: number): void => {
      if (map.size <= maxSize) return;
      const toRemove = map.size - maxSize;
      const entries = Array.from(map.entries())
        .sort((a, b) => a[1] - b[1]);
      for (let i = 0; i < toRemove && i < entries.length; i++) {
        map.delete(entries[i][0]);
      }
    };

    // Clean non-timestamp maps (FIFO - remove oldest inserted)
    const cleanupFifo = <K, V>(map: Map<K, V>, maxSize: number): void => {
      if (map.size <= maxSize) return;
      const toRemove = map.size - maxSize;
      const keys = Array.from(map.keys());
      for (let i = 0; i < toRemove && i < keys.length; i++) {
        map.delete(keys[i]);
      }
    };

    cleanupTimestamps(this.lastBroadcastTime, this.MAX_MAP_ENTRIES);
    cleanupTimestamps(this.pendingBroadcasts, this.MAX_MAP_ENTRIES);
    cleanupFifo(this.lastSyncedVersions, this.MAX_MAP_ENTRIES);
  }

  async onload() {
    log.info("Loading plugin...");

    // Check if Obsidian Sync is enabled - block P2P sync to prevent conflicts
    if (this.isObsidianSyncEnabled()) {
      log.info("Obsidian Sync is enabled, disabling P2P Sync");
      this.disabledReason = "Obsidian Sync is enabled. P2P Sync cannot run at the same time to prevent vault corruption. Please disable Obsidian Sync in Settings → Core plugins if you want to use P2P Sync.";
      
      // Still register the view so users can see why it's disabled
      this.registerView(VIEW_TYPE_SYNC, (leaf) => new SyncView(leaf, this));
      
      // Add ribbon icon
      this.addRibbonIcon("refresh-cw", "Open P2P Sync", () => {
        this.activateView();
      });

      // Add status bar showing disabled state (desktop only)
      if (!Platform.isMobile) {
        this.statusBarEl = this.addStatusBarItem();
        this.updateStatusBar("Disabled");
      }

      log.info("Plugin loaded (disabled due to Obsidian Sync)");
      return;
    }

    // Initialize WASM module
    try {
      await initWasm();
      log.info("WASM initialized");
    } catch (err) {
      log.error("Failed to initialize WASM:", err);
      new Notice("P2P Sync: Failed to initialize. Check console for details.");
      return;
    }

    // Get or generate peer ID
    this.peerId = this.loadPeerId();
    log.info("Peer ID:", this.peerId);

    // Load settings (known peers, etc.)
    await this.loadSettings();

    // Register the sidebar view
    this.registerView(VIEW_TYPE_SYNC, (leaf) => new SyncView(leaf, this));

    // Add ribbon icon (left sidebar button)
    this.addRibbonIcon("refresh-cw", "Open P2P Sync", () => {
      this.activateView();
    });

    // Add status bar item (desktop only)
    if (!Platform.isMobile) {
      this.statusBarEl = this.addStatusBarItem();
      this.updateStatusBar("ready");
    }

    // Add commands
    this.addCommand({
      id: "p2p-sync-open",
      name: "Open Sync Panel",
      callback: () => {
        this.activateView();
      },
    });

    this.addCommand({
      id: "p2p-sync-now",
      name: "Sync Now",
      callback: () => {
        this.triggerSync();
      },
    });

    // Try to load existing vault and start peer manager on startup
    this.app.workspace.onLayoutReady(async () => {
      await this.tryLoadVault();
      await this.startPeerManager();
      this.registerFileEvents();
    });

    log.info("Plugin loaded");
  }

  /**
   * Check if Obsidian Sync (core plugin) is enabled.
   */
  private isObsidianSyncEnabled(): boolean {
    try {
      // Access internal plugins API (not officially documented but stable)
      const internalPlugins = (this.app as any).internalPlugins;
      if (!internalPlugins) return false;
      
      const syncPlugin = internalPlugins.getPluginById?.("sync");
      return syncPlugin?.enabled ?? false;
    } catch (err) {
      log.warn("Could not check Obsidian Sync status:", err);
      return false;
    }
  }

  async onunload() {
    // Stop peer manager
    if (this.peerManager) {
      await this.peerManager.stop();
      this.peerManager = null;
    }

    // Clean up vault
    if (this.vault) {
      this.vault.free();
      this.vault = null;
    }
    log.info("Plugin unloaded");
  }

  /**
   * Start the peer manager (WebSocket server + connection handling).
   */
  private async startPeerManager(): Promise<void> {
    if (!this.peerId) return;

    // Get the plugin directory for loading ws-server.js on desktop
    const pluginDir = this.getPluginDir();
    this.peerManager = new PeerManager(this.peerId, pluginDir);

    this.peerManager.on("peer-connected", async (peer: PeerInfo) => {
      log.info(`Peer connected: ${peer.id} (${peer.direction})`);
      this.updateStatusBar(`${this.peerManager?.peerCount ?? 0} peers`);
      this.events.trigger("state-changed");

      // Send sync request to the new peer
      await this.sendSyncRequest(peer.id);
    });

    this.peerManager.on("peer-disconnected", (peerId: string) => {
      log.info(`Peer disconnected: ${peerId}`);
      this.updateStatusBar(`${this.peerManager?.peerCount ?? 0} peers`);
      this.events.trigger("state-changed");
    });

    this.peerManager.on("message", async (peerId: string, data: Uint8Array) => {
      log.debug(`Message from ${peerId}:`, data.length, "bytes");
      this.peerManager?.updatePeerActivity(peerId);
      await this.handleSyncMessage(peerId, data);
    });

    this.peerManager.on("peer-activity", () => {
      this.events.trigger("state-changed");
    });

    this.peerManager.on("error", (err: Error) => {
      log.error("Peer manager error:", err);
    });

    try {
      const actualPort = await this.peerManager.start(DEFAULT_PORT);
      log.info(`Peer manager started on port ${actualPort}`);
      this.events.trigger("state-changed");
    } catch (err) {
      log.error("Failed to start peer manager:", err);
      // Non-fatal - can still work as client
    }

    // Auto-reconnect to known peers (fire-and-forget, don't block startup)
    this.reconnectToKnownPeers();
  }

  /**
   * Attempt to reconnect to all known peers in parallel.
   * Runs in background - failures are logged but don't block startup.
   */
  private async reconnectToKnownPeers(): Promise<void> {
    if (this.settings.knownPeers.length === 0) {
      return;
    }

    log.info(`Auto-reconnecting to ${this.settings.knownPeers.length} known peer(s)...`);

    // Connect to all peers in parallel
    const results = await Promise.allSettled(
      this.settings.knownPeers.map(async (peer) => {
        await this.peerManager?.connectToUrl(peer.url);
        return peer.label;
      })
    );

    // Log results
    for (const result of results) {
      if (result.status === 'fulfilled') {
        log.info(`Reconnected to ${result.value}`);
      } else {
        log.warn(`Failed to reconnect:`, result.reason);
      }
    }
  }

  /**
   * Connect to a peer at the given address.
   */
  async connectToPeer(address: string, port: number = DEFAULT_PORT): Promise<void> {
    if (!this.peerManager) {
      throw new Error("Peer manager not started");
    }

    await this.peerManager.connectToPeer(address, port);

    // Save to known peers for auto-reconnect
    const url = `ws://${address}:${port}`;
    const label = port === DEFAULT_PORT ? address : `${address}:${port}`;
    await this.addKnownPeer(url, label);
  }

  /**
   * Connect to a peer using a full WebSocket URL.
   * Use this for connecting through reverse proxies or with custom paths.
   */
  async connectToUrl(url: string): Promise<void> {
    if (!this.peerManager) {
      throw new Error("Peer manager not started");
    }

    await this.peerManager.connectToUrl(url);

    // Save to known peers for auto-reconnect
    // Build a readable label from the URL
    const parsed = new URL(url);
    let label = parsed.hostname;
    // Include port if non-default
    if (parsed.port && parsed.port !== '443' && parsed.port !== '80') {
      label += `:${parsed.port}`;
    }
    // Include path if not root
    if (parsed.pathname && parsed.pathname !== '/') {
      label += parsed.pathname;
    }
    await this.addKnownPeer(url, label);
  }

  /**
   * Get the list of connected peers.
   */
  getConnectedPeers(): PeerInfo[] {
    return this.peerManager?.getConnectedPeers() ?? [];
  }

  /**
   * Disconnect from a specific peer.
   */
  disconnectPeer(peerId: string): void {
    if (!this.peerManager) return;
    try {
      this.peerManager.disconnectPeer(peerId);
    } catch (e) {
      log.error("Failed to disconnect peer:", e);
    }
  }

  /**
   * Get the server port.
   */
  getServerPort(): number {
    return this.peerManager?.port ?? DEFAULT_PORT;
  }

  /**
   * Get the LAN URL for other devices to connect to this one.
   * Returns null on mobile or if no server is running.
   */
  getLanUrl(): string | null {
    if (!this.peerManager?.isServerRunning) return null;

    const addresses = this.peerManager.getLanAddresses();
    if (addresses.length === 0) return null;

    const port = this.peerManager.port;
    // Return first LAN address (usually the primary network interface)
    return `ws://${addresses[0]}:${port}`;
  }

  /**
   * Load peer ID from local storage, or generate a new one.
   * Uses vault-specific key so each vault has its own peer ID.
   */
  private loadPeerId(): string {
    const vaultKey = `${PEER_ID_KEY}:${this.app.vault.getName()}`;
    
    // Try to load from Obsidian's localStorage
    const stored = localStorage.getItem(vaultKey);
    if (stored) {
      return stored;
    }

    // Generate new peer ID and store it
    const newId = generatePeerId();
    localStorage.setItem(vaultKey, newId);
    return newId;
  }

  /**
   * Load plugin settings from .sync/settings.json.
   */
  private async loadSettings(): Promise<void> {
    try {
      if (await this.app.vault.adapter.exists(SETTINGS_PATH)) {
        const raw = await this.app.vault.adapter.read(SETTINGS_PATH);
        const data = JSON.parse(raw);
        this.settings = Object.assign({}, DEFAULT_SETTINGS, data);
      } else {
        this.settings = { ...DEFAULT_SETTINGS };
      }
    } catch (e) {
      log.warn("Failed to load settings, using defaults:", e);
      this.settings = { ...DEFAULT_SETTINGS };
    }

    // Validate knownPeers array
    if (!Array.isArray(this.settings.knownPeers)) {
      log.warn("Invalid knownPeers in settings, resetting to empty array");
      this.settings.knownPeers = [];
    } else {
      // Filter out invalid entries and deduplicate by normalized URL
      const seen = new Set<string>();
      this.settings.knownPeers = this.settings.knownPeers.filter(p => {
        if (!p || typeof p.url !== 'string' || typeof p.label !== 'string') {
          return false;
        }
        const normalized = this.normalizeWsUrl(p.url);
        if (seen.has(normalized)) {
          return false;
        }
        seen.add(normalized);
        return true;
      });
    }
  }

  /**
   * Save plugin settings to .sync/settings.json.
   */
  private async saveSettings(): Promise<void> {
    try {
      // Ensure .sync directory exists
      if (!await this.app.vault.adapter.exists(".sync")) {
        await this.app.vault.adapter.mkdir(".sync");
      }

      const json = JSON.stringify(this.settings, null, 2);
      await this.app.vault.adapter.write(SETTINGS_PATH, json);
    } catch (e) {
      log.error("Failed to save settings:", e);
    }
  }

  /**
   * Normalize a WebSocket URL for consistent storage and comparison.
   * Must match PeerManager.normalizeUrl() exactly for peer matching to work.
   * Removes default ports, query strings, fragments, and trailing slashes.
   */
  private normalizeWsUrl(url: string): string {
    try {
      const parsed = new URL(url);
      // Remove default ports
      if ((parsed.protocol === 'wss:' && parsed.port === '443') ||
          (parsed.protocol === 'ws:' && parsed.port === '80')) {
        parsed.port = '';
      }
      // Remove query and hash (PeerManager strips these)
      parsed.search = '';
      parsed.hash = '';
      // Remove trailing slash (URL.href always adds one for root paths)
      return parsed.href.replace(/\/$/, '');
    } catch {
      return url;
    }
  }

  /**
   * Add a peer to known peers (for auto-reconnect).
   */
  private async addKnownPeer(url: string, label: string): Promise<void> {
    // Normalize URL for consistent comparison
    const normalizedUrl = this.normalizeWsUrl(url);

    // Don't add duplicates
    if (this.settings.knownPeers.some(p => p.url === normalizedUrl)) {
      return;
    }
    this.settings.knownPeers.push({ url: normalizedUrl, label });
    await this.saveSettings();
  }

  /**
   * Remove a peer from known peers.
   */
  async removeKnownPeer(url: string): Promise<void> {
    const normalizedUrl = this.normalizeWsUrl(url);
    this.settings.knownPeers = this.settings.knownPeers.filter(p => p.url !== normalizedUrl);
    await this.saveSettings();
  }

  /**
   * Get the list of known peers (for UI display).
   */
  getKnownPeers(): KnownPeer[] {
    return this.settings.knownPeers;
  }

  /**
   * Try to load an existing vault (if .sync directory exists).
   */
  private async tryLoadVault(): Promise<void> {
    if (!this.peerId) return;

    try {
      const fsBridge = createFsBridge(this.app.vault);
      const syncDirExists = await this.app.vault.adapter.exists(".sync");

      if (syncDirExists) {
        this.vault = await WasmVault.load(fsBridge, this.peerId);
        log.info("Vault loaded");
        this.updateStatusBar("loaded");
        this.events.trigger("state-changed");
      } else {
        log.info("No existing vault found (.sync directory missing)");
      }
    } catch (err) {
      log.error("Failed to load vault:", err);
    }
  }

  /**
   * Initialize a new vault.
   *
   * Creates the .sync directory and initializes Loro documents.
   */
  async initializeVault(): Promise<void> {
    if (!this.peerId) {
      throw new Error("Peer ID not set");
    }

    if (this.vault) {
      log.info("Vault already initialized");
      return;
    }

    const fsBridge = createFsBridge(this.app.vault);
    this.vault = await WasmVault.init(fsBridge, this.peerId);
    log.info("Vault initialized");
    this.updateStatusBar("initialized");
    this.events.trigger("state-changed");
  }

  /**
   * Check if the vault is initialized.
   */
  isVaultInitialized(): boolean {
    return this.vault !== null;
  }

  /**
   * Trigger a manual sync with all connected peers.
   */
  private async triggerSync(): Promise<void> {
    if (!this.vault) {
      new Notice("P2P Sync: Vault not initialized");
      return;
    }

    const peers = this.getConnectedPeers();
    if (peers.length === 0) {
      new Notice("P2P Sync: No peers connected");
      return;
    }

    // Send sync request to all peers
    for (const peer of peers) {
      await this.sendSyncRequest(peer.id);
    }

    new Notice(`P2P Sync: Syncing with ${peers.length} peer(s)...`);
  }

  /**
   * Send a sync request to a specific peer.
   */
  private async sendSyncRequest(peerId: string): Promise<void> {
    if (!this.vault || !this.peerManager) {
      log.debug(`Cannot send sync request - vault=${!!this.vault}, peerManager=${!!this.peerManager}`);
      return;
    }

    try {
      // Queue the WASM call to prevent concurrent &mut self borrows
      const request = await this.vaultQueue.run(() => 
        this.vault!.prepareSyncRequest()
      );
      this.peerManager.send(peerId, request);
      log.debug(`Sent sync request to ${peerId}`);
    } catch (err) {
      log.error(`Failed to send sync request to ${peerId}:`, err);
    }
  }

  /**
   * Handle an incoming sync message from a peer.
   */
  private async handleSyncMessage(peerId: string, data: Uint8Array): Promise<void> {
    if (!this.vault || !this.peerManager) {
      log.debug(`Cannot handle sync message - vault=${!!this.vault}, peerManager=${!!this.peerManager}`);
      return;
    }

    // Reject excessively large messages to prevent memory issues
    if (data.length > MAX_SYNC_MESSAGE_SIZE) {
      log.warn(`Rejecting oversized sync message from ${peerId}: ${Math.round(data.length / 1024 / 1024)}MB`);
      return;
    }

    try {
      // Queue the WASM call to prevent concurrent &mut self borrows
      const result = await this.vaultQueue.run(() =>
        this.vault!.processSyncMessage(data)
      ) as SyncMessageResult;
      
      log.debug(`Sync result - response=${result.response ? result.response.length + ' bytes' : 'null'}, modifiedPaths=${JSON.stringify(result.modifiedPaths)}`);

      // If there's a response, send it back
      if (result.response) {
        this.peerManager.send(peerId, result.response);
        log.debug(`Sent sync response to ${peerId}`);
      }

      // If files were modified, reload them in Obsidian
      if (result.modifiedPaths.length > 0) {
        log.info(`${result.modifiedPaths.length} file(s) synced from ${peerId}:`, result.modifiedPaths);
        
        for (const path of result.modifiedPaths) {
          await this.reloadFileFromDisk(path);
        }
      }
    } catch (err) {
      log.error(`Failed to process sync message from ${peerId}:`, err);
    }
  }

  /**
   * Broadcast a document update to all connected peers.
   * Also flushes pending broadcasts if throttle window has passed.
   */
  private async broadcastDocumentUpdate(path: string): Promise<void> {
    if (!this.vault || !this.peerManager) return;
    if (this.peerManager.peerCount === 0) return;

    const now = Date.now();

    // Check if this file has pending (throttled) updates that need flushing
    const pendingTime = this.pendingBroadcasts.get(path);

    if (pendingTime) {
      if (now - pendingTime >= BROADCAST_THROTTLE_MS) {
        // Throttle window passed, clear pending and proceed to broadcast
        this.pendingBroadcasts.delete(path);
        this.lastBroadcastTime.set(path, now);
      } else {
        // Throttle window hasn't passed yet, skip this broadcast
        log.debug(`Skipping broadcast (still throttling): ${path}`);
        return;
      }
    } else {
      // No pending entry - check if we need to throttle based on last broadcast time
      const lastBroadcast = this.lastBroadcastTime.get(path) ?? 0;
      if (now - lastBroadcast < BROADCAST_THROTTLE_MS) {
        // Still in throttle window, queue for later
        log.debug(`Queuing broadcast (throttle window): ${path}`);
        this.pendingBroadcasts.set(path, now);
        return;
      }
      // Throttle window passed, proceed to broadcast
      this.lastBroadcastTime.set(path, now);
    }

    // Clean up old entries before performing the broadcast
    this.cleanupMaps();

    try {
      // Queue the WASM call to prevent concurrent &mut self borrows
      const update = await this.vaultQueue.run(() =>
        this.vault!.prepareDocumentUpdate(path)
      );
      if (update) {
        this.peerManager.broadcast(update);
        log.debug(`Broadcast update for ${path} to ${this.peerManager.peerCount} peer(s)`);
      }
    } catch (err) {
      log.error(`Failed to broadcast document update for ${path}:`, err);
    }
  }

  /**
   * Update the status bar text.
   */
  private updateStatusBar(status: string): void {
    if (this.statusBarEl) {
      this.statusBarEl.setText(`P2P: ${status}`);
    }
  }

  /**
   * Get the vault's base path on the filesystem.
   * Returns null on mobile where direct filesystem access isn't available.
   */
  getVaultBasePath(): string | null {
    const adapter = this.app.vault.adapter;
    if (adapter instanceof FileSystemAdapter) {
      return adapter.getBasePath();
    }
    return null;
  }

  /**
   * Get the plugin's installation directory.
   * Returns null on mobile where direct filesystem access isn't available.
   */
  private getPluginDir(): string | null {
    const basePath = this.getVaultBasePath();
    if (!basePath) return null;
    return `${basePath}/.obsidian/plugins/obsidian-p2p-sync`;
  }

  /**
   * Validate that a path is safe and within the vault.
   * Prevents path traversal attacks.
   */
  private isValidVaultPath(path: string): boolean {
    // Check for path traversal sequences
    if (path.includes("..") || path.startsWith("/") || path.startsWith("\\")) {
      return false;
    }
    // Normalize and verify it doesn't escape vault
    const normalized = path.replace(/\\/g, "/");
    // Reject leading/trailing whitespace (filesystem inconsistencies)
    if (normalized !== normalized.trim()) return false;
    // Allow alphanumerics, common filename characters, and path separators
    // Excludes: .. (path traversal), leading slashes, control chars, ?% (URL-like)
    // TODO: Add Unicode support (\p{L}\p{N}\p{M} with u flag) for international filenames
    return /^[a-zA-Z0-9_\-\./ '(),&#@+\[\]]+$/.test(normalized);
  }

  /**
   * Reload a file from disk into Obsidian's cache.
   *
   * Called after sync writes a file to ensure Obsidian picks up the changes.
   * For new files, this creates the file in Obsidian's index.
    * 
    * Stores the document's version vector after sync to detect if subsequent
    * file modifications are purely from this sync operation.
    */
  private async reloadFileFromDisk(path: string): Promise<void> {
    // Validate path to prevent path traversal attacks
    if (!this.isValidVaultPath(path)) {
      log.error(`Invalid path rejected: ${path}`);
      return;
    }
    
    const abstractFile = this.app.vault.getAbstractFileByPath(path);
    
    try {
      if (abstractFile instanceof TFile) {
        // File exists in Obsidian - read fresh content from disk and update
        const content = await this.app.vault.adapter.read(path);
        // Use modify to update Obsidian's internal cache
        await this.app.vault.modify(abstractFile, content);
        log.debug(`Reloaded ${path} from disk`);
      } else if (!abstractFile) {
        // File doesn't exist in Obsidian - check if it exists on disk
        const exists = await this.app.vault.adapter.exists(path);
        if (exists) {
          const content = await this.app.vault.adapter.read(path);
          // Ensure parent directories exist in Obsidian
          const dir = path.substring(0, path.lastIndexOf("/"));
          if (dir) {
            const dirExists = this.app.vault.getAbstractFileByPath(dir);
            if (!dirExists) {
              await this.app.vault.createFolder(dir);
            }
          }
          await this.app.vault.create(path, content);
          log.debug(`Created ${path} in Obsidian`);
        }
      }

      // Store the version vector after sync completes.
      // This allows onFileModified to detect if subsequent modifications
      // are purely from this sync (version unchanged) or include local edits.
      if (this.vault) {
        const version = await this.vaultQueue.run(() =>
          this.vault!.getDocumentVersion(path)
        ) as Uint8Array | null;
        if (version) {
          this.lastSyncedVersions.set(path, version);
          log.debug(`Stored synced version for ${path}`);
        }
      }
    } catch (err) {
      log.error(`Failed to reload/create ${path}:`, err);
    }
  }

  /**
   * Check if WASM module is ready for use.
   */
  isWasmReady(): boolean {
    return isWasmReady();
  }

  /**
   * Activate (open/reveal) the sync sidebar view.
   */
  async activateView(): Promise<void> {
    const { workspace } = this.app;

    // Check if view already exists
    let leaf = workspace.getLeavesOfType(VIEW_TYPE_SYNC)[0];

    if (!leaf) {
      // Create in right sidebar
      const rightLeaf = workspace.getRightLeaf(false);
      if (rightLeaf) {
        await rightLeaf.setViewState({ type: VIEW_TYPE_SYNC, active: true });
        leaf = rightLeaf;
      }
    }

    // Reveal and focus
    if (leaf) {
      workspace.revealLeaf(leaf);
    }
  }

  private registerFileEvents(): void {
    // File change events - will trigger sync later
    this.registerEvent(
      this.app.vault.on("modify", async (file) => {
        if (!this.vault) return;
        if (!(file instanceof TFile)) return;
        if (!file.path.endsWith(".md")) return;
        
        // Skip files that are too large to prevent memory issues
        if (file.stat.size > MAX_FILE_SIZE) {
          const sizeMB = Math.round(file.stat.size / 1024 / 1024);
          log.warn(`Skipping large file (${sizeMB}MB): ${file.path}`);
          new Notice(`P2P Sync: "${file.path}" is ${sizeMB}MB (max: 10MB) - not syncing`);
          return;
        }

        log.debug("File modified:", file.path);
        try {
          // Update the Loro document with the new content
          await this.vaultQueue.run(() => this.vault!.onFileChanged(file.path));

          // Check if this modification is purely from a sync operation.
          // If the current version includes all operations from the last synced version,
          // then no local edits were made - skip broadcasting to prevent sync loops.
          const lastSynced = this.lastSyncedVersions.get(file.path);
          if (lastSynced) {
            const currentVersion = await this.vaultQueue.run(() =>
              this.vault!.getDocumentVersion(file.path)
            ) as Uint8Array | null;

            if (currentVersion && versionIncludes(currentVersion, lastSynced)) {
              // Version unchanged or only contains synced operations - skip broadcast
              log.debug("Skipping broadcast for synced file:", file.path);
              // Clear the synced version now that we've processed this event
              this.lastSyncedVersions.delete(file.path);
              return;
            }
            // Version has new local operations - clear synced version and proceed to broadcast
            this.lastSyncedVersions.delete(file.path);
          }

          // Broadcast the update to all connected peers (handles throttling internally)
          await this.broadcastDocumentUpdate(file.path);
        } catch (err) {
          log.error("Failed to handle file change:", err);
        }
      })
    );

    this.registerEvent(
      this.app.vault.on("create", async (file) => {
        if (!this.vault) return;
        if (!(file instanceof TFile)) return;
        if (!file.path.endsWith(".md")) return;
        
        // Skip files that are too large
        if (file.stat.size > MAX_FILE_SIZE) {
          const sizeMB = Math.round(file.stat.size / 1024 / 1024);
          log.warn(`Skipping large file (${sizeMB}MB): ${file.path}`);
          new Notice(`P2P Sync: "${file.path}" is ${sizeMB}MB (max: 10MB) - not syncing`);
          return;
        }

        log.debug("File created:", file.path);
        try {
          // Check if this is a file being created from sync
          const lastSynced = this.lastSyncedVersions.get(file.path);
          if (lastSynced) {
            log.debug("Skipping broadcast for synced new file:", file.path);
            this.lastSyncedVersions.delete(file.path);
            return;
          }

          // Queue the WASM call to prevent concurrent &mut self borrows
          await this.vaultQueue.run(() => this.vault!.onFileChanged(file.path));
          // Broadcast the new file to all connected peers
          await this.broadcastDocumentUpdate(file.path);
        } catch (err) {
          log.error("Failed to handle file create:", err);
        }
      })
    );

    this.registerEvent(
      this.app.vault.on("delete", async (file) => {
        if (!this.vault) return;
        if (!(file instanceof TFile)) return;
        if (!file.path.endsWith(".md")) return;

        log.debug("File deleted:", file.path);
        try {
          // Delete file from tree (CRDT operation)
          await this.vaultQueue.run(() => this.vault!.deleteFile(file.path));
          log.info(`Deleted ${file.path} from registry tree`);

          // Broadcast deletion to peers
          if (this.peerManager && this.peerManager.peerCount > 0) {
            const msg = this.vault!.prepareFileDeleted(file.path);
            this.peerManager.broadcast(msg);
            log.debug(`Broadcast deletion of ${file.path} to ${this.peerManager.peerCount} peer(s)`);
          }
        } catch (err) {
          log.error("Failed to handle file delete:", err);
        }
      })
    );

    this.registerEvent(
      this.app.vault.on("rename", async (file, oldPath) => {
        if (!this.vault) return;
        if (!(file instanceof TFile)) return;
        if (!file.path.endsWith(".md")) return;

        log.debug("File renamed:", oldPath, "->", file.path);
        try {
          // Rename file in tree (CRDT operation)
          await this.vaultQueue.run(() => this.vault!.renameFile(oldPath, file.path));
          log.info(`Renamed ${oldPath} -> ${file.path} in registry tree`);

          // Broadcast rename to peers
          if (this.peerManager && this.peerManager.peerCount > 0) {
            const msg = this.vault!.prepareFileRenamed(oldPath, file.path);
            this.peerManager.broadcast(msg);
            log.debug(`Broadcast rename ${oldPath} -> ${file.path} to ${this.peerManager.peerCount} peer(s)`);
          }
        } catch (err) {
          log.error("Failed to handle file rename:", err);
        }
      })
    );
  }
}
