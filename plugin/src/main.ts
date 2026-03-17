import { Notice, Plugin, FileSystemAdapter, Events, TFile } from "obsidian";
import { SyncView, VIEW_TYPE_SYNC } from "./views/SyncView";
import { DebugModal } from "./views/DebugModal";
import {
  initWasm,
  isWasmReady,
  WasmVault,
  WasmSubscription,
  SyncEvent,
  LogEvent,
} from "./wasm";
import { createFsBridge } from "./fs/ObsidianFs";
import { NetworkManager, type PeerInfo } from "./network";
import { VaultOperationQueue } from "./VaultOperationQueue";
import { log } from "./logger";
import { checkForUpdates } from "./updater";

/** Key for storing secret key (hex) in local storage */
const SECRET_KEY_KEY = "p2p-sync-secret-key";

/** Maximum file size to sync (10MB). Files larger than this are skipped. */
const MAX_FILE_SIZE = 10 * 1024 * 1024;

/** Maximum number of sync events to keep in the debug buffer */
const MAX_DEBUG_EVENTS = 50;

/** Minimum time between broadcasts for the same file (ms). Prevents flooding. */
const BROADCAST_THROTTLE_MS = 1000;

/** Path to settings file within the vault's .sync directory */
const SETTINGS_PATH = ".sync/settings.json";

/**
 * Plugin settings persisted per-vault in .sync/settings.json.
 *
 * `bootstrapPeers` are iroh EndpointId hex strings used on startup to join
 * the vault gossip swarm. Peers exchange their node IDs out-of-band (e.g.,
 * copy-paste) and add each other as bootstrap peers.
 */
interface P2PSyncSettings {
  bootstrapPeers: string[];
}

const DEFAULT_SETTINGS: P2PSyncSettings = {
  bootstrapPeers: [],
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

  /** Network manager (iroh-based P2P) */
  networkManager: NetworkManager | null = null;

  /** Status bar element */
  private statusBarEl: HTMLElement | null = null;

  /**
   * Whether the plugin is disabled due to Obsidian Sync being active.
   * When true, the plugin will not initialize vault or network manager.
   */
  disabledReason: string | null = null;

  /**
   * Rolling buffer of recent sync events for the debug panel.
   * Events are captured at the plugin level so they persist across modal opens.
   */
  debugEvents: Array<{ event: SyncEvent; id: number }> = [];

  /** Counter for unique event IDs */
  private debugEventIdCounter = 0;

  /** Subscription to WASM sync events for debug buffer */
  private debugEventSubscription: WasmSubscription | null = null;

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

  private cleanupMaps(): void {
    const cleanupTimestamps = <K>(map: Map<K, number>, maxSize: number): void => {
      if (map.size <= maxSize) return;
      const toRemove = map.size - maxSize;
      const entries = Array.from(map.entries()).sort((a, b) => a[1] - b[1]);
      for (let i = 0; i < toRemove && i < entries.length; i++) {
        map.delete(entries[i][0]);
      }
    };

    cleanupTimestamps(this.lastBroadcastTime, this.MAX_MAP_ENTRIES);
    cleanupTimestamps(this.pendingBroadcasts, this.MAX_MAP_ENTRIES);
  }

  async onload() {
    log.info("Loading plugin...");

    // Check if Obsidian Sync is enabled - block P2P sync to prevent conflicts
    if (this.isObsidianSyncEnabled()) {
      log.info("Obsidian Sync is enabled, disabling P2P Sync");
      this.disabledReason =
        "Obsidian Sync is enabled. P2P Sync cannot run at the same time to prevent vault corruption. Please disable Obsidian Sync in Settings → Core plugins if you want to use P2P Sync.";

      this.registerView(VIEW_TYPE_SYNC, (leaf) => new SyncView(leaf, this));
      this.addRibbonIcon("refresh-cw", "Open P2P Sync", () => { this.activateView(); });
      this.statusBarEl = this.addStatusBarItem();
      this.updateStatusBar("Disabled");

      log.info("Plugin loaded (disabled due to Obsidian Sync)");
      return;
    }

    // Initialize WASM module with file logging
    try {
      const debugLogPath = ".sync/debug.log";

      if (!(await this.app.vault.adapter.exists(".sync"))) {
        await this.app.vault.adapter.mkdir(".sync");
      }

      await this.app.vault.adapter.write(
        debugLogPath,
        `--- Log started ${new Date().toISOString()} ---\n`
      );

      await initWasm({
        logger: (event: LogEvent) => {
          const line = `${new Date(event.timestamp).toISOString()} [${event.level}] ${event.target}: ${event.message}\n`;
          this.app.vault.adapter.append(debugLogPath, line).catch((err) => {
            console.error("Failed to write debug log:", err);
          });
        },
      });
      log.info("WASM initialized");
    } catch (err) {
      log.error("Failed to initialize WASM:", err);
      new Notice("P2P Sync: Failed to initialize. Check console for details.");
      return;
    }

    await this.loadSettings();

    this.registerView(VIEW_TYPE_SYNC, (leaf) => new SyncView(leaf, this));
    this.addRibbonIcon("refresh-cw", "Open P2P Sync", () => { this.activateView(); });
    this.statusBarEl = this.addStatusBarItem();
    this.updateStatusBar("ready");

    this.addCommand({
      id: "p2p-sync-open",
      name: "Open Sync Panel",
      callback: () => { this.activateView(); },
    });

    this.addCommand({
      id: "p2p-sync-debug",
      name: "Open Debug Panel",
      callback: () => { new DebugModal(this.app, this).open(); },
    });

    this.app.workspace.onLayoutReady(async () => {
      await this.tryLoadVault();
      await this.startNetworkManager();
      this.registerFileEvents();
    });

    log.info("Plugin loaded");
    checkForUpdates(this);
  }

  private isObsidianSyncEnabled(): boolean {
    try {
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
    if (this.debugEventSubscription) {
      this.debugEventSubscription.dispose();
      this.debugEventSubscription = null;
    }

    if (this.networkManager) {
      await this.networkManager.stop();
      this.networkManager = null;
    }

    if (this.vault) {
      this.vault.free();
      this.vault = null;
    }
    log.info("Plugin unloaded");
  }

  private subscribeToDebugEvents(): void {
    if (!this.vault || this.debugEventSubscription) return;

    this.debugEventSubscription = this.vault.subscribeSyncEvents((event: SyncEvent) => {
      this.debugEvents.unshift({ event, id: this.debugEventIdCounter++ });
      if (this.debugEvents.length > MAX_DEBUG_EVENTS) {
        this.debugEvents.length = MAX_DEBUG_EVENTS;
      }
      this.events.trigger("state-changed");
    });

    log.debug("Subscribed to sync events for debug buffer");
  }

  clearDebugEvents(): void {
    this.debugEvents = [];
    this.debugEventIdCounter = 0;
  }

  /**
   * Start the network manager (iroh sync node + gossip).
   */
  private async startNetworkManager(): Promise<void> {
    const vaultId = this.vault?.peerId() ?? null;

    if (!vaultId) {
      log.info("Vault not initialized — skipping network manager startup");
      return;
    }

    const secretKey = this.loadOrGenerateSecretKey();

    this.networkManager = new NetworkManager();

    if (this.vault) {
      this.networkManager.setVault(this.vault);
    }

    this.networkManager.on("peer-connected", (nodeId: string) => {
      log.info(`Peer joined swarm: ${nodeId}`);
      this.updateStatusBar(`${this.networkManager?.peerCount ?? 0} peers`);
      this.events.trigger("state-changed");
    });

    this.networkManager.on("peer-disconnected", (nodeId: string) => {
      log.info(`Peer left swarm: ${nodeId}`);
      this.updateStatusBar(`${this.networkManager?.peerCount ?? 0} peers`);
      this.events.trigger("state-changed");
    });

    this.networkManager.on("change-received", (from: string, path: string) => {
      log.debug(`Change notification from ${from}: ${path}`);
      this.events.trigger("state-changed");
    });

    this.networkManager.on("files-modified", async (paths: string[]) => {
      log.info(`${paths.length} file(s) updated from sync`);
      for (const path of paths) {
        await this.reloadFileFromDisk(path);
      }
      this.events.trigger("state-changed");
    });

    this.networkManager.on("sync-completed", () => {
      this.events.trigger("state-changed");
    });

    try {
      await this.networkManager.start(secretKey, vaultId, this.settings.bootstrapPeers);
      log.info("Network manager started");
      this.events.trigger("state-changed");
    } catch (err) {
      log.error("Failed to start network manager:", err);
    }
  }

  /**
   * Load or generate the 32-byte ed25519 secret key for this node.
   *
   * Stored as a 64-char hex string in localStorage, keyed by vault name.
   */
  private loadOrGenerateSecretKey(): Uint8Array {
    const vaultKey = `${SECRET_KEY_KEY}:${this.app.vault.getName()}`;
    const stored = localStorage.getItem(vaultKey);

    if (stored && stored.length === 64) {
      const bytes = new Uint8Array(32);
      for (let i = 0; i < 32; i++) {
        bytes[i] = parseInt(stored.slice(i * 2, i * 2 + 2), 16);
      }
      return bytes;
    }

    const key = new Uint8Array(32);
    crypto.getRandomValues(key);

    const hex = Array.from(key)
      .map((b) => b.toString(16).padStart(2, "0"))
      .join("");
    localStorage.setItem(vaultKey, hex);

    return key;
  }

  /**
   * Get the list of peers visible in the gossip swarm.
   */
  getConnectedPeers(): PeerInfo[] {
    return this.networkManager?.getConnectedPeers() ?? [];
  }

  /**
   * Get this node's iroh EndpointId (hex string).
   */
  getNodeId(): string | null {
    return this.networkManager?.nodeId ?? null;
  }

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

    if (!Array.isArray(this.settings.bootstrapPeers)) {
      this.settings.bootstrapPeers = [];
    }
  }

  private async saveSettings(): Promise<void> {
    try {
      if (!(await this.app.vault.adapter.exists(".sync"))) {
        await this.app.vault.adapter.mkdir(".sync");
      }
      const json = JSON.stringify(this.settings, null, 2);
      await this.app.vault.adapter.write(SETTINGS_PATH, json);
    } catch (e) {
      log.error("Failed to save settings:", e);
    }
  }

  /**
   * Add an iroh EndpointId as a bootstrap peer for future startups.
   */
  async addBootstrapPeer(nodeId: string): Promise<void> {
    if (!this.settings.bootstrapPeers.includes(nodeId)) {
      this.settings.bootstrapPeers.push(nodeId);
      await this.saveSettings();
    }
  }

  /**
   * Remove a bootstrap peer.
   */
  async removeBootstrapPeer(nodeId: string): Promise<void> {
    this.settings.bootstrapPeers = this.settings.bootstrapPeers.filter((p) => p !== nodeId);
    await this.saveSettings();
  }

  private async tryLoadVault(): Promise<void> {
    try {
      const fsBridge = createFsBridge(this.app.vault);
      const syncDirExists = await this.app.vault.adapter.exists(".sync");

      if (syncDirExists) {
        this.vault = await WasmVault.load(fsBridge);
        log.info("Vault loaded");
        this.updateStatusBar("loaded");
        this.subscribeToDebugEvents();
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
   */
  async initializeVault(): Promise<void> {
    if (this.vault) {
      log.info("Vault already initialized");
      return;
    }

    const fsBridge = createFsBridge(this.app.vault);
    this.vault = await WasmVault.init(fsBridge);
    log.info("Vault initialized");
    this.updateStatusBar("initialized");
    this.subscribeToDebugEvents();
    this.events.trigger("state-changed");

    // Start network manager now that vault is available
    await this.startNetworkManager();
  }

  isVaultInitialized(): boolean {
    return this.vault !== null;
  }

  /**
   * Broadcast a file change notification to all connected peers.
   * Throttled per-file to prevent flooding.
   */
  private async broadcastChange(path: string): Promise<void> {
    if (!this.networkManager) return;
    if (this.networkManager.peerCount === 0) return;

    const now = Date.now();
    const pendingTime = this.pendingBroadcasts.get(path);

    if (pendingTime) {
      if (now - pendingTime >= BROADCAST_THROTTLE_MS) {
        this.pendingBroadcasts.delete(path);
        this.lastBroadcastTime.set(path, now);
      } else {
        log.debug(`Skipping broadcast (still throttling): ${path}`);
        return;
      }
    } else {
      const lastBroadcast = this.lastBroadcastTime.get(path) ?? 0;
      if (now - lastBroadcast < BROADCAST_THROTTLE_MS) {
        log.debug(`Queuing broadcast (throttle window): ${path}`);
        this.pendingBroadcasts.set(path, now);
        return;
      }
      this.lastBroadcastTime.set(path, now);
    }

    this.cleanupMaps();

    try {
      await this.networkManager.broadcastChange(path);
      log.debug(`Broadcast change for ${path} to ${this.networkManager.peerCount} peer(s)`);
    } catch (err) {
      log.error(`Failed to broadcast change for ${path}:`, err);
    }
  }

  private updateStatusBar(status: string): void {
    if (this.statusBarEl) {
      this.statusBarEl.setText(`P2P: ${status}`);
    }
  }

  getVaultBasePath(): string | null {
    const adapter = this.app.vault.adapter;
    if (adapter instanceof FileSystemAdapter) {
      return adapter.getBasePath();
    }
    return null;
  }

  private isValidVaultPath(path: string): boolean {
    if (path.includes("..") || path.startsWith("/") || path.startsWith("\\")) {
      return false;
    }
    const normalized = path.replace(/\\/g, "/");
    if (normalized !== normalized.trim()) return false;
    return /^[a-zA-Z0-9_\-\./ '(),&#@+\[\]]+$/.test(normalized);
  }

  private async reloadFileFromDisk(path: string): Promise<void> {
    if (!this.isValidVaultPath(path)) {
      log.error(`Invalid path rejected: ${path}`);
      return;
    }

    const abstractFile = this.app.vault.getAbstractFileByPath(path);

    try {
      if (abstractFile instanceof TFile) {
        const content = await this.app.vault.adapter.read(path);
        await this.app.vault.modify(abstractFile, content);
        log.debug(`Reloaded ${path} from disk`);
      } else if (!abstractFile) {
        const exists = await this.app.vault.adapter.exists(path);
        if (exists) {
          const content = await this.app.vault.adapter.read(path);
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
    } catch (err) {
      log.error(`Failed to reload/create ${path}:`, err);
    }
  }

  isWasmReady(): boolean {
    return isWasmReady();
  }

  async activateView(): Promise<void> {
    const { workspace } = this.app;
    let leaf = workspace.getLeavesOfType(VIEW_TYPE_SYNC)[0];

    if (!leaf) {
      const rightLeaf = workspace.getRightLeaf(false);
      if (rightLeaf) {
        await rightLeaf.setViewState({ type: VIEW_TYPE_SYNC, active: true });
        leaf = rightLeaf;
      }
    }

    if (leaf) {
      workspace.revealLeaf(leaf);
    }
  }

  private registerFileEvents(): void {
    this.registerEvent(
      this.app.vault.on("modify", async (file) => {
        if (!this.vault) return;
        if (!(file instanceof TFile)) return;
        if (!file.path.endsWith(".md")) return;

        if (file.stat.size > MAX_FILE_SIZE) {
          const sizeMB = Math.round(file.stat.size / 1024 / 1024);
          log.warn(`Skipping large file (${sizeMB}MB): ${file.path}`);
          new Notice(`P2P Sync: "${file.path}" is ${sizeMB}MB (max: 10MB) - not syncing`);
          return;
        }

        log.debug("File modified:", file.path);
        try {
          const wasSynced = await this.vaultQueue.run(async () =>
            this.vault!.consumeSyncFlag(file.path)
          );
          if (wasSynced) {
            log.debug("Skipping broadcast for synced file:", file.path);
            return;
          }

          await this.vaultQueue.run(() => this.vault!.onFileChanged(file.path));
          await this.broadcastChange(file.path);
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

        if (file.stat.size > MAX_FILE_SIZE) {
          const sizeMB = Math.round(file.stat.size / 1024 / 1024);
          log.warn(`Skipping large file (${sizeMB}MB): ${file.path}`);
          new Notice(`P2P Sync: "${file.path}" is ${sizeMB}MB (max: 10MB) - not syncing`);
          return;
        }

        log.debug("File created:", file.path);
        try {
          const wasSynced = await this.vaultQueue.run(async () =>
            this.vault!.consumeSyncFlag(file.path)
          );
          if (wasSynced) {
            log.debug("Skipping broadcast for synced new file:", file.path);
            return;
          }

          await this.vaultQueue.run(() => this.vault!.onFileChanged(file.path));
          await this.broadcastChange(file.path);
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
          const wasSynced = await this.vaultQueue.run(async () =>
            this.vault!.consumeSyncFlag(file.path)
          );
          if (wasSynced) {
            log.debug("Skipping broadcast for synced deletion:", file.path);
            return;
          }

          await this.vaultQueue.run(() => this.vault!.deleteFile(file.path));
          log.info(`Deleted ${file.path} from registry tree`);

          // Notify peers so they can reconcile the deletion
          await this.broadcastChange(file.path);
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
          const wasSynced = await this.vaultQueue.run(async () =>
            this.vault!.consumeSyncFlag(file.path)
          );
          if (wasSynced) {
            log.debug("Skipping broadcast for synced rename:", oldPath, "->", file.path);
            return;
          }

          await this.vaultQueue.run(() => this.vault!.renameFile(oldPath, file.path));
          log.info(`Renamed ${oldPath} -> ${file.path} in registry tree`);

          // Notify peers of the new path so they can reconcile
          await this.broadcastChange(file.path);
        } catch (err) {
          log.error("Failed to handle file rename:", err);
        }
      })
    );
  }
}
