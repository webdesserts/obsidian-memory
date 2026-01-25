import { App, Modal } from "obsidian";
import { mount, unmount } from "svelte";
import DebugDashboard from "./DebugDashboard.svelte";
import type P2PSyncPlugin from "../main";

/**
 * Modal for displaying P2P sync debug information.
 *
 * Shows internal sync state: peer ID, server status, registry stats,
 * version vectors, connected peers, and real-time sync events.
 */
export class DebugModal extends Modal {
  private plugin: P2PSyncPlugin;
  private component: ReturnType<typeof mount> | undefined;

  constructor(app: App, plugin: P2PSyncPlugin) {
    super(app);
    this.plugin = plugin;
  }

  onOpen(): void {
    this.contentEl.addClass("p2p-sync-debug-modal");
    this.setTitle("P2P Sync Debug");

    this.component = mount(DebugDashboard, {
      target: this.contentEl,
      props: { plugin: this.plugin },
    });
  }

  onClose(): void {
    if (this.component) {
      unmount(this.component);
      this.component = undefined;
    }
  }
}
