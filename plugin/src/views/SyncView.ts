import { ItemView, WorkspaceLeaf } from "obsidian";
import { mount, unmount } from "svelte";
import SyncPanel from "./SyncPanel.svelte";
import type P2PSyncPlugin from "../main";

export const VIEW_TYPE_SYNC = "p2p-sync-view";

export class SyncView extends ItemView {
  plugin: P2PSyncPlugin;
  private component: ReturnType<typeof SyncPanel> | undefined;

  // Don't show navigation arrows
  navigation = false;

  constructor(leaf: WorkspaceLeaf, plugin: P2PSyncPlugin) {
    super(leaf);
    this.plugin = plugin;
  }

  getViewType(): string {
    return VIEW_TYPE_SYNC;
  }

  getDisplayText(): string {
    return "P2P Sync";
  }

  getIcon(): string {
    return "refresh-cw";
  }

  async onOpen(): Promise<void> {
    this.contentEl.addClass("p2p-sync-view");

    this.component = mount(SyncPanel, {
      target: this.contentEl,
      props: {
        plugin: this.plugin,
      },
    });
  }

  async onClose(): Promise<void> {
    if (this.component) {
      unmount(this.component);
    }
  }

  refresh(): void {
    if (this.component) {
      unmount(this.component);
    }
    this.component = mount(SyncPanel, {
      target: this.contentEl,
      props: {
        plugin: this.plugin,
      },
    });
  }
}
