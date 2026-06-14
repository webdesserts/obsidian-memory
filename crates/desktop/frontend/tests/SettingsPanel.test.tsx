import { describe, it, expect, vi, beforeEach } from "vitest";
import { render } from "vitest-browser-react";
import { page, userEvent } from "vitest/browser";
import { TestWrapper } from "./test-wrapper";

// Mock the Tauri bridge: in the test browser there's no Tauri runtime, so the
// three modules the panel imports are stubbed. `invoke` is the seam to the Rust
// commands — get_settings returns canned settings, save_settings is recorded so
// we can assert the panel sends the right (camelCased) args. The window/dialog
// modules are spied so close + the folder picker don't touch a real window.
const invoke = vi.fn();
const close = vi.fn();
const openDialog = vi.fn();

vi.mock("@tauri-apps/api/core", () => ({
  invoke: (...args: unknown[]) => invoke(...args),
}));
vi.mock("@tauri-apps/api/window", () => ({
  getCurrentWindow: () => ({ close }),
}));
vi.mock("@tauri-apps/plugin-dialog", () => ({
  open: (...args: unknown[]) => openDialog(...args),
}));

// Imported after the mocks are registered so the panel binds to the stubs.
const { SettingsPanel } = await import("../src/windows/SettingsPanel");

const CANNED_SETTINGS = {
  relay_url: "https://umbra.computer/",
  vault_path: "/Users/michael/notes",
};

/** Route get_settings to canned data; default everything else to a resolved call. */
function stubInvoke(
  overrides: Record<string, (args: Record<string, unknown>) => unknown> = {},
) {
  invoke.mockImplementation((cmd: string, args: Record<string, unknown> = {}) => {
    if (cmd === "get_settings") return Promise.resolve(CANNED_SETTINGS);
    const handler = overrides[cmd];
    if (handler) return Promise.resolve(handler(args));
    return Promise.resolve();
  });
}

beforeEach(() => {
  invoke.mockReset();
  close.mockReset();
  openDialog.mockReset();
});

describe("SettingsPanel", () => {
  describe("on mount", () => {
    it("loads persisted settings into the fields", async () => {
      stubInvoke();
      const screen = await render(
        <TestWrapper>
          <SettingsPanel />
        </TestWrapper>,
      );

      // The panel reads get_settings and binds both fields to the result.
      const relay = screen.getByLabelText("Relay URL");
      const vault = screen.getByLabelText("Vault Folder");
      await expect.element(relay).toHaveValue(CANNED_SETTINGS.relay_url);
      await expect.element(vault).toHaveValue(CANNED_SETTINGS.vault_path);
      expect(invoke).toHaveBeenCalledWith("get_settings");
    });
  });

  describe("saving", () => {
    it("rejects an invalid relay URL before calling the backend", async () => {
      stubInvoke();
      const screen = await render(
        <TestWrapper>
          <SettingsPanel />
        </TestWrapper>,
      );

      // Replace the loaded URL with a bare host — fails the D3 http(s):// rule.
      const relay = screen.getByLabelText("Relay URL");
      await userEvent.fill(relay, "umbra.computer");
      await screen.getByRole("button", { name: "Save" }).click();

      await expect
        .element(screen.getByText("Relay URL must start with http:// or https://"))
        .toBeVisible();
      // A rejected client-side validation never reaches the backend.
      expect(invoke).not.toHaveBeenCalledWith("save_settings", expect.anything());
    });

    it("rejects an empty vault folder before calling the backend", async () => {
      stubInvoke();
      const screen = await render(
        <TestWrapper>
          <SettingsPanel />
        </TestWrapper>,
      );

      const vault = screen.getByLabelText("Vault Folder");
      await userEvent.clear(vault);
      await screen.getByRole("button", { name: "Save" }).click();

      await expect
        .element(screen.getByText("Vault folder is required"))
        .toBeVisible();
      expect(invoke).not.toHaveBeenCalledWith("save_settings", expect.anything());
    });

    it("saves valid settings and shows the restart notice", async () => {
      stubInvoke();
      const screen = await render(
        <TestWrapper>
          <SettingsPanel />
        </TestWrapper>,
      );

      // Wait for the load so the saved value is the canned one, not a stale blank.
      await expect
        .element(screen.getByLabelText("Relay URL"))
        .toHaveValue(CANNED_SETTINGS.relay_url);
      await screen.getByRole("button", { name: "Save" }).click();

      // save_settings receives camelCased keys (Tauri maps them to the Rust
      // snake_case params), and a successful save shows the unconditional notice.
      expect(invoke).toHaveBeenCalledWith("save_settings", {
        relayUrl: CANNED_SETTINGS.relay_url,
        vaultPath: CANNED_SETTINGS.vault_path,
      });
      await expect
        .element(
          screen.getByText("Changes take effect when the app is restarted."),
        )
        .toBeVisible();
    });

    it("surfaces the backend error when a save is rejected", async () => {
      stubInvoke({
        save_settings: () => {
          throw "Vault folder does not exist: /Users/michael/notes";
        },
      });
      const screen = await render(
        <TestWrapper>
          <SettingsPanel />
        </TestWrapper>,
      );

      await expect
        .element(screen.getByLabelText("Relay URL"))
        .toHaveValue(CANNED_SETTINGS.relay_url);
      await screen.getByRole("button", { name: "Save" }).click();

      // The Rust is_dir check failure (passes client validation) is shown verbatim.
      await expect
        .element(
          screen.getByText("Vault folder does not exist: /Users/michael/notes"),
        )
        .toBeVisible();
    });
  });

  describe("vault folder picker", () => {
    it("sets the path from the native folder dialog", async () => {
      stubInvoke();
      openDialog.mockResolvedValue("/Users/michael/picked-vault");
      const screen = await render(
        <TestWrapper>
          <SettingsPanel />
        </TestWrapper>,
      );

      await screen.getByRole("button", { name: "Choose…" }).click();

      expect(openDialog).toHaveBeenCalledWith({ directory: true });
      await expect
        .element(screen.getByLabelText("Vault Folder"))
        .toHaveValue("/Users/michael/picked-vault");
    });

    it("leaves the path unchanged when the dialog is cancelled", async () => {
      stubInvoke();
      openDialog.mockResolvedValue(null);
      const screen = await render(
        <TestWrapper>
          <SettingsPanel />
        </TestWrapper>,
      );

      await expect
        .element(screen.getByLabelText("Vault Folder"))
        .toHaveValue(CANNED_SETTINGS.vault_path);
      await screen.getByRole("button", { name: "Choose…" }).click();

      // A cancelled dialog (null) is a no-op — the loaded path stays put.
      await expect
        .element(screen.getByLabelText("Vault Folder"))
        .toHaveValue(CANNED_SETTINGS.vault_path);
    });
  });

  describe("cancel", () => {
    it("closes the window", async () => {
      stubInvoke();
      const screen = await render(
        <TestWrapper>
          <SettingsPanel />
        </TestWrapper>,
      );

      await screen.getByRole("button", { name: "Cancel" }).click();
      expect(close).toHaveBeenCalledOnce();
    });
  });

  describe("appearance", () => {
    it("renders the loaded panel", async () => {
      stubInvoke();
      const screen = await render(
        <TestWrapper>
          <SettingsPanel />
        </TestWrapper>,
      );
      await expect
        .element(screen.getByLabelText("Relay URL"))
        .toHaveValue(CANNED_SETTINGS.relay_url);

      // Park the pointer off the fields so the snapshot is the true resting state
      // (hover mono-inverts ui's TextInput).
      await page.elementLocator(screen.container).hover({ position: { x: 0, y: 0 } });
      await expect
        .element(page.elementLocator(screen.container))
        .toMatchScreenshot();
    });
  });
});
