import { describe, it, expect, vi, beforeEach, afterEach } from "vitest";
import { render } from "vitest-browser-react";
import { page } from "vitest/browser";
import { TestWrapper } from "./test-wrapper";

// Mock the Tauri bridge. `invoke` is the seam to reject_inbound_pair; `listen`
// captures the completed/failed callbacks so tests can drive those events;
// getCurrentWindow is spied so close() doesn't touch a real window.
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

const { PairResponder } = await import("../src/windows/PairResponder");

type EventCallback = (event: { payload: unknown }) => void;
const listeners = new Map<string, EventCallback>();

/**
 * Inject the responder's init data the way the Rust side does — via the URL
 * query string — BEFORE render(). The component reads window.location.search
 * synchronously on first render, so the URL must be set before it mounts. This
 * mirrors the production path (no test-only prop): only the URL is the seam.
 */
function setQuery(search: string) {
  window.history.replaceState(null, "", search);
}

beforeEach(() => {
  invoke.mockReset();
  close.mockReset();
  listen.mockReset();
  listeners.clear();

  listen.mockImplementation((event: string, cb: EventCallback) => {
    listeners.set(event, cb);
    return Promise.resolve(() => {});
  });
  invoke.mockResolvedValue(undefined);
  // Default: no query params, so each test starts from the fallback state and
  // opts into specific params with setQuery() before its own render().
  setQuery("?");
});

afterEach(() => {
  // Leave the URL clean so a stray query string can't leak into the next test.
  setQuery("?");
});

describe("PairResponder", () => {
  describe("init data", () => {
    it("renders the device name and code from the query string", async () => {
      setQuery("?device=Studio&code=123456&expires=0");
      const screen = await render(
        <TestWrapper>
          <PairResponder />
        </TestWrapper>,
      );

      await expect.element(screen.getByText("Studio")).toBeVisible();
      await expect.element(screen.getByText("123456")).toBeVisible();
    });

    it("falls back when params are absent", async () => {
      // beforeEach already set "?", so no device/code are present.
      const screen = await render(
        <TestWrapper>
          <PairResponder />
        </TestWrapper>,
      );

      await expect.element(screen.getByText("Unknown device")).toBeVisible();
      await expect.element(screen.getByText("——————")).toBeVisible();
    });
  });

  describe("countdown", () => {
    it("renders M:SS and closes the window at expiry", async () => {
      vi.useFakeTimers();
      try {
        const now = Date.now();
        // Expire 1 second out so a single tick window crosses zero deterministically.
        setQuery(`?device=Studio&code=123456&expires=${now + 1000}`);
        const screen = await render(
          <TestWrapper>
            <PairResponder />
          </TestWrapper>,
        );

        await vi.waitFor(() => expect(screen.getByText("0:01")).toBeTruthy());

        // Advance past expiry — the tick crosses zero, shows 0:00, and closes.
        await vi.advanceTimersByTimeAsync(1100);
        await vi.waitFor(() => expect(screen.getByText("0:00")).toBeTruthy());
        expect(close).toHaveBeenCalled();
      } finally {
        vi.useRealTimers();
      }
    });
  });

  describe("completion", () => {
    it("shows success, disables Reject, and closes after 800ms", async () => {
      vi.useFakeTimers();
      try {
        setQuery("?device=Studio&code=123456&expires=0");
        const screen = await render(
          <TestWrapper>
            <PairResponder />
          </TestWrapper>,
        );
        await vi.waitFor(() => expect(listeners.has("pair://responder-completed")).toBe(true));

        listeners.get("pair://responder-completed")?.({ payload: undefined });

        await vi.waitFor(() => expect(screen.getByText("Pairing complete.")).toBeTruthy());
        await expect
          .element(screen.getByRole("button", { name: "Reject" }))
          .toBeDisabled();

        await vi.advanceTimersByTimeAsync(800);
        expect(close).toHaveBeenCalledOnce();
      } finally {
        vi.useRealTimers();
      }
    });

    it("shows the failure reason and closes after 1500ms", async () => {
      vi.useFakeTimers();
      try {
        setQuery("?device=Studio&code=123456&expires=0");
        const screen = await render(
          <TestWrapper>
            <PairResponder />
          </TestWrapper>,
        );
        await vi.waitFor(() => expect(listeners.has("pair://responder-failed")).toBe(true));

        listeners.get("pair://responder-failed")?.({ payload: { reason: "code mismatch" } });

        await vi.waitFor(() =>
          expect(screen.getByText("Pairing failed: code mismatch")).toBeTruthy(),
        );

        await vi.advanceTimersByTimeAsync(1500);
        expect(close).toHaveBeenCalledOnce();
      } finally {
        vi.useRealTimers();
      }
    });
  });

  describe("reject", () => {
    it("rejects the inbound pairing and closes the window", async () => {
      setQuery("?device=Studio&code=123456&expires=0");
      const screen = await render(
        <TestWrapper>
          <PairResponder />
        </TestWrapper>,
      );

      await screen.getByRole("button", { name: "Reject" }).click();

      expect(invoke).toHaveBeenCalledWith("reject_inbound_pair");
      await vi.waitFor(() => expect(close).toHaveBeenCalledOnce());
    });
  });

  describe("appearance", () => {
    it("renders the resting responder panel", async () => {
      // No `expires` param → the countdown shows the static "5:00" fallback and
      // never ticks, keeping the snapshot deterministic.
      setQuery("?device=Studio&code=123456");
      const screen = await render(
        <TestWrapper>
          <PairResponder />
        </TestWrapper>,
      );
      await expect.element(screen.getByText("123456")).toBeVisible();

      await page.elementLocator(screen.container).hover({ position: { x: 0, y: 0 } });
      await expect
        .element(page.elementLocator(screen.container))
        .toMatchScreenshot();
    });
  });
});
