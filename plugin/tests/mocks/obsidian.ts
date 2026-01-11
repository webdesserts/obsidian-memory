/**
 * Mock Obsidian module for testing.
 *
 * Provides minimal stubs for Obsidian types used by the plugin.
 * Tests should use MockObsidianVault for actual filesystem operations.
 */

export class Events {
  private handlers: Map<string, Function[]> = new Map();

  on(event: string, handler: Function): void {
    if (!this.handlers.has(event)) {
      this.handlers.set(event, []);
    }
    this.handlers.get(event)!.push(handler);
  }

  off(event: string, handler: Function): void {
    const handlers = this.handlers.get(event);
    if (handlers) {
      const index = handlers.indexOf(handler);
      if (index !== -1) {
        handlers.splice(index, 1);
      }
    }
  }

  trigger(event: string, ...args: unknown[]): void {
    const handlers = this.handlers.get(event);
    if (handlers) {
      for (const handler of handlers) {
        handler(...args);
      }
    }
  }
}

export class TFile {
  constructor(
    public path: string,
    public name: string = path.split("/").pop() || path,
    public extension: string = "md"
  ) {}
}

export class TFolder {
  constructor(
    public path: string,
    public name: string = path.split("/").pop() || path
  ) {}
}

export type TAbstractFile = TFile | TFolder;

export class Notice {
  constructor(public message: string) {}
}

export const Platform = {
  isMobile: false,
  isDesktop: true,
};

export class FileSystemAdapter {
  constructor(private basePath: string) {}

  getBasePath(): string {
    return this.basePath;
  }
}

export class Plugin {
  app: App = {} as App;
  manifest: PluginManifest = {} as PluginManifest;

  registerEvent(event: unknown): void {}
  registerView(type: string, viewCreator: (leaf: WorkspaceLeaf) => View): void {}
  addRibbonIcon(icon: string, title: string, callback: () => void): HTMLElement {
    return document.createElement("div");
  }
  addStatusBarItem(): HTMLElement {
    return document.createElement("div");
  }
  addCommand(command: Command): void {}

  async onload(): Promise<void> {}
  async onunload(): Promise<void> {}
}

export interface App {
  vault: Vault;
  workspace: Workspace;
}

export interface Vault {
  adapter: DataAdapter;
  getName(): string;
  getAbstractFileByPath(path: string): TAbstractFile | null;
  on(event: string, callback: Function): EventRef;
  create(path: string, content: string): Promise<TFile>;
  modify(file: TFile, content: string): Promise<void>;
  createFolder(path: string): Promise<void>;
}

export interface DataAdapter {
  exists(path: string): Promise<boolean>;
  read(path: string): Promise<string>;
  readBinary(path: string): Promise<ArrayBuffer>;
  write(path: string, content: string): Promise<void>;
  writeBinary(path: string, content: ArrayBuffer): Promise<void>;
  remove(path: string): Promise<void>;
  mkdir(path: string): Promise<void>;
  list(path: string): Promise<{ files: string[]; folders: string[] }>;
  stat(path: string): Promise<{ mtime: number; size: number; type: string } | null>;
}

export interface Workspace {
  onLayoutReady(callback: () => void): void;
  getLeavesOfType(type: string): WorkspaceLeaf[];
  getRightLeaf(split: boolean): WorkspaceLeaf | null;
  revealLeaf(leaf: WorkspaceLeaf): void;
}

export interface WorkspaceLeaf {
  setViewState(state: { type: string; active: boolean }): Promise<void>;
}

export interface View {}

export interface PluginManifest {}

export interface EventRef {}

export interface Command {
  id: string;
  name: string;
  callback: () => void;
}
