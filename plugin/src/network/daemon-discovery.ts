import { Events } from "obsidian";
import TOML from "smol-toml";

/**
 * Watches .sync/daemon.toml for daemon relay URL changes.
 *
 * The daemon writes its embedded relay URL to daemon.toml when it starts.
 * The plugin reads this to decide whether to use the daemon's relay or
 * start its own. Polling is used instead of fs.watch because fs.watch
 * is unreliable across platforms (especially on macOS and in Electron).
 *
 * Emits 'relay-changed' when the relay URL appears, disappears, or changes.
 */
export class DaemonDiscovery extends Events {
  private relayUrl: string | null = null;
  private watchInterval: ReturnType<typeof setInterval> | null = null;
  private readonly configPath = ".sync/daemon.toml";

  /** The current relay URL from daemon.toml, or null if none is available. */
  get currentRelayUrl(): string | null {
    return this.relayUrl;
  }

  /**
   * Start watching daemon.toml for relay URL changes.
   *
   * Reads the initial state immediately, then polls every 5 seconds.
   *
   * @param vaultAdapter - Obsidian DataAdapter for reading vault files
   */
  async start(vaultAdapter: VaultAdapter): Promise<void> {
    await this.checkConfig(vaultAdapter);
    this.watchInterval = setInterval(() => this.checkConfig(vaultAdapter), 5000);
  }

  /** Stop polling for changes. */
  stop(): void {
    if (this.watchInterval) {
      clearInterval(this.watchInterval);
      this.watchInterval = null;
    }
  }

  private async checkConfig(vaultAdapter: VaultAdapter): Promise<void> {
    try {
      const exists = await vaultAdapter.exists(this.configPath);
      if (!exists) {
        this.updateRelayUrl(null);
        return;
      }

      const content = await vaultAdapter.read(this.configPath);
      const config = TOML.parse(content);
      const url = typeof config.relay_url === "string" ? config.relay_url : null;
      this.updateRelayUrl(url);
    } catch {
      // File doesn't exist or isn't valid TOML — no relay available
      this.updateRelayUrl(null);
    }
  }

  private updateRelayUrl(url: string | null): void {
    if (url !== this.relayUrl) {
      this.relayUrl = url;
      this.trigger("relay-changed", url);
    }
  }
}

/** Minimal interface for reading vault files — matches Obsidian's DataAdapter. */
interface VaultAdapter {
  exists(path: string): Promise<boolean>;
  read(path: string): Promise<string>;
}
