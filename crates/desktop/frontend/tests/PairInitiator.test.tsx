import { describe, it, expect, vi, beforeEach } from "vitest";
import { render } from "vitest-browser-react";
import { page, userEvent } from "vitest/browser";
import { TestWrapper } from "./test-wrapper";

// Mock the Tauri bridge. There's no Tauri runtime in the test browser, so the
// three modules the initiator imports are stubbed. `invoke` is the seam to the
// Rust pairing commands; `listen` registers discovery-event subscriptions and
// hands back the captured callbacks so tests can drive events; `getCurrentWindow`
// is spied so close() doesn't touch a real window.
const invoke = vi.fn();
const close = vi.fn();
const listen = vi.fn();

vi.mock("@tauri-apps/api/core", () => ({
  invoke: (...args: unknown[]) => invoke(...args),
}));
vi.mock("@tauri-apps/api/event", () => ({
  listen: (...args: unknown[]) => listen(...args),
}));
vi.mock("@tauri-apps/api/window", () => ({
  getCurrentWindow: () => ({ close }),
}));

// Imported after the mocks are registered so the component binds to the stubs.
const { PairInitiator } = await import("../src/windows/PairInitiator");

type EventCallback = (event: { payload: unknown }) => void;

// Captured listener callbacks, keyed by event name, so a test can simulate the
// daemon emitting a discovery event by invoking the registered callback.
const listeners = new Map<string, EventCallback>();
// Ordered log of bridge calls used to assert the race-guard: both listens must
// be registered before start_pair_discovery is invoked.
let callOrder: string[] = [];

beforeEach(() => {
  invoke.mockReset();
  close.mockReset();
  listen.mockReset();
  listeners.clear();
  callOrder = [];

  listen.mockImplementation((event: string, cb: EventCallback) => {
    listeners.set(event, cb);
    callOrder.push(`listen:${event}`);
    return Promise.resolve(() => {});
  });
  invoke.mockImplementation((cmd: string) => {
    callOrder.push(`invoke:${cmd}`);
    return Promise.resolve();
  });
});

/** Drive a discovered-mesh event through the captured listener callback. */
function emitMeshDiscovered(payload: {
  vault_id: string;
  mesh_name: string;
  online_count: number;
}) {
  listeners.get("pair://mesh-discovered")?.({ payload });
}

describe("PairInitiator", () => {
  describe("on mount", () => {
    it("registers both discovery listeners BEFORE starting discovery", async () => {
      const screen = await render(
        <TestWrapper>
          <PairInitiator />
        </TestWrapper>,
      );

      // Wait until discovery has actually been kicked off.
      await vi.waitFor(() => expect(invoke).toHaveBeenCalledWith("start_pair_discovery"));

      // Race guard (reviewer-flagged): the daemon can emit pair://mesh-discovered
      // the instant discovery starts, so both listeners MUST be live first. Assert
      // start_pair_discovery is the LAST bridge call, preceded by both listens.
      expect(callOrder).toEqual([
        "listen:pair://mesh-discovered",
        "listen:pair://discovery-finished",
        "invoke:start_pair_discovery",
      ]);
      // The screen renders before the assertions resolve.
      void screen;
    });
  });

  describe("discovery", () => {
    it("adds a discovered mesh as a selectable option", async () => {
      const screen = await render(
        <TestWrapper>
          <PairInitiator />
        </TestWrapper>,
      );
      await vi.waitFor(() => expect(listeners.has("pair://mesh-discovered")).toBe(true));

      emitMeshDiscovered({ vault_id: "v1", mesh_name: "Home", online_count: 2 });

      await expect.element(screen.getByText("Home (2 online)")).toBeInTheDocument();
      await expect.element(screen.getByText("Scanning… found 1 so far")).toBeVisible();
    });

    it("ignores a duplicate vault_id", async () => {
      const screen = await render(
        <TestWrapper>
          <PairInitiator />
        </TestWrapper>,
      );
      await vi.waitFor(() => expect(listeners.has("pair://mesh-discovered")).toBe(true));

      emitMeshDiscovered({ vault_id: "v1", mesh_name: "Home", online_count: 2 });
      // A second event for the same vault_id (e.g. an updated online_count) is a
      // no-op — the mesh is deduped by vault_id, matching the original Map.
      emitMeshDiscovered({ vault_id: "v1", mesh_name: "Home", online_count: 3 });

      await expect.element(screen.getByText("Scanning… found 1 so far")).toBeVisible();
      const options = screen.container.querySelectorAll('option[value="v1"]');
      expect(options.length).toBe(1);
    });

    it("updates the scan status when discovery finishes", async () => {
      const screen = await render(
        <TestWrapper>
          <PairInitiator />
        </TestWrapper>,
      );
      await vi.waitFor(() => expect(listeners.has("pair://discovery-finished")).toBe(true));

      emitMeshDiscovered({ vault_id: "v1", mesh_name: "Home", online_count: 2 });
      listeners.get("pair://discovery-finished")?.({ payload: undefined });

      await expect.element(screen.getByText("Found 1 mesh(es).")).toBeVisible();
    });

    it("reports no meshes found when discovery finishes empty", async () => {
      const screen = await render(
        <TestWrapper>
          <PairInitiator />
        </TestWrapper>,
      );
      await vi.waitFor(() => expect(listeners.has("pair://discovery-finished")).toBe(true));

      listeners.get("pair://discovery-finished")?.({ payload: undefined });

      await expect.element(screen.getByText("No meshes found.")).toBeVisible();
    });
  });

  describe("request pairing", () => {
    it("sends request_pairing with the camelCase vaultId arg", async () => {
      invoke.mockImplementation((cmd: string) => {
        callOrder.push(`invoke:${cmd}`);
        if (cmd === "request_pairing") {
          return Promise.resolve({ device_name: "Studio" });
        }
        return Promise.resolve();
      });

      const screen = await render(
        <TestWrapper>
          <PairInitiator />
        </TestWrapper>,
      );
      await vi.waitFor(() => expect(listeners.has("pair://mesh-discovered")).toBe(true));
      emitMeshDiscovered({ vault_id: "v1", mesh_name: "Home", online_count: 2 });

      // Selecting a mesh enables Request; clicking it pairs against that vault.
      await userEvent.selectOptions(screen.getByLabelText("Mesh"), "v1");
      await screen.getByRole("button", { name: "Request pairing" }).click();

      // Contract: the arg object uses the camelCase `vaultId` key (Tauri maps it
      // to the Rust snake_case vault_id param).
      expect(invoke).toHaveBeenCalledWith("request_pairing", { vaultId: "v1" });
      // On success, stage 2 reveals the code prompt naming the responder device.
      await expect
        .element(screen.getByText("Enter the code shown on Studio:"))
        .toBeVisible();
    });

    it("surfaces a connection failure and re-enables stage 1", async () => {
      invoke.mockImplementation((cmd: string) => {
        callOrder.push(`invoke:${cmd}`);
        if (cmd === "request_pairing") {
          return Promise.reject("network down");
        }
        return Promise.resolve();
      });

      const screen = await render(
        <TestWrapper>
          <PairInitiator />
        </TestWrapper>,
      );
      await vi.waitFor(() => expect(listeners.has("pair://mesh-discovered")).toBe(true));
      emitMeshDiscovered({ vault_id: "v1", mesh_name: "Home", online_count: 2 });

      await userEvent.selectOptions(screen.getByLabelText("Mesh"), "v1");
      await screen.getByRole("button", { name: "Request pairing" }).click();

      await expect.element(screen.getByText("Connection failed: network down")).toBeVisible();
      // Stage 1 comes back so the user can retry.
      await expect
        .element(screen.getByRole("button", { name: "Request pairing" }))
        .toBeEnabled();
    });
  });

  describe("submit code", () => {
    it("sends submit_pair_code with vaultId + code and closes on success", async () => {
      vi.useFakeTimers();
      try {
        invoke.mockImplementation((cmd: string) => {
          callOrder.push(`invoke:${cmd}`);
          if (cmd === "request_pairing") return Promise.resolve({ device_name: "Studio" });
          if (cmd === "submit_pair_code") return Promise.resolve({ device_name: "Studio" });
          return Promise.resolve();
        });

        const screen = await render(
          <TestWrapper>
            <PairInitiator />
          </TestWrapper>,
        );
        await vi.waitFor(() => expect(listeners.has("pair://mesh-discovered")).toBe(true));
        emitMeshDiscovered({ vault_id: "v1", mesh_name: "Home", online_count: 2 });

        await userEvent.selectOptions(screen.getByLabelText("Mesh"), "v1");
        await screen.getByRole("button", { name: "Request pairing" }).click();
        await vi.waitFor(() =>
          expect(invoke).toHaveBeenCalledWith("request_pairing", { vaultId: "v1" }),
        );

        await userEvent.fill(screen.getByLabelText("Enter the code shown on Studio:"), "123456");
        await screen.getByRole("button", { name: "Pair" }).click();

        // Contract: both the camelCase `vaultId` and `code` keys are sent.
        expect(invoke).toHaveBeenCalledWith("submit_pair_code", {
          vaultId: "v1",
          code: "123456",
        });
        await vi.waitFor(() =>
          expect(screen.getByText("Paired with Studio. Closing…")).toBeTruthy(),
        );

        // Window auto-closes 1500ms after a successful pair.
        await vi.advanceTimersByTimeAsync(1500);
        expect(close).toHaveBeenCalledOnce();
      } finally {
        vi.useRealTimers();
      }
    });
  });

  describe("cancel", () => {
    it("cancels discovery and closes the window", async () => {
      const screen = await render(
        <TestWrapper>
          <PairInitiator />
        </TestWrapper>,
      );
      await vi.waitFor(() => expect(invoke).toHaveBeenCalledWith("start_pair_discovery"));

      await screen.getByRole("button", { name: "Cancel" }).click();

      expect(invoke).toHaveBeenCalledWith("cancel_pair_discovery");
      await vi.waitFor(() => expect(close).toHaveBeenCalledOnce());
    });
  });

  describe("appearance", () => {
    it("renders the resting initiator panel", async () => {
      const screen = await render(
        <TestWrapper>
          <PairInitiator />
        </TestWrapper>,
      );
      await vi.waitFor(() => expect(listeners.has("pair://mesh-discovered")).toBe(true));

      // Park the pointer off interactive elements so the snapshot is the true
      // resting state (hover inverts ui's TextInput / Button).
      await page.elementLocator(screen.container).hover({ position: { x: 0, y: 0 } });
      await expect
        .element(page.elementLocator(screen.container))
        .toMatchScreenshot();
    });
  });
});
