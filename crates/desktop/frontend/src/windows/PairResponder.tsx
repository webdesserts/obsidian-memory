import { useEffect, useState } from "react";
import { Button } from "@webdesserts/ui";
import { invoke } from "@tauri-apps/api/core";
import { listen } from "@tauri-apps/api/event";
import { getCurrentWindow } from "@tauri-apps/api/window";

// Init data delivered via the URL query string set by the Rust WebviewWindowBuilder.
// Reading it synchronously on first render (not from a post-mount effect) avoids
// a listen-vs-emit race: the values are present in window.location the moment the
// page parses, before any async work runs.
interface ResponderInit {
  device: string;
  code: string;
  expiresAt: number | null;
}

function readInit(): ResponderInit {
  const params = new URLSearchParams(window.location.search);
  return {
    device: params.get("device") ?? "Unknown device",
    code: params.get("code") ?? "——————",
    expiresAt: parseInt(params.get("expires") ?? "0", 10) || null,
  };
}

function formatRemaining(ms: number): string {
  const minutes = Math.floor(ms / 60000);
  const seconds = Math.floor((ms % 60000) / 1000);
  return `${minutes}:${String(seconds).padStart(2, "0")}`;
}

export function PairResponder() {
  // useState initializer runs once, synchronously, during the first render — the
  // documented no-race read of the query string.
  const [init] = useState<ResponderInit>(readInit);
  const [countdown, setCountdown] = useState(() =>
    init.expiresAt === null
      ? "5:00"
      : formatRemaining(Math.max(0, init.expiresAt - Date.now())),
  );
  const [status, setStatus] = useState<{ text: string; kind: "error" | "ok" } | null>(
    null,
  );
  const [rejectDisabled, setRejectDisabled] = useState(false);

  // Countdown: tick every 250ms, formatting remaining time as M:SS. At expiry,
  // show 0:00 and close the window — the code is no longer valid. (Security/UX
  // expiry: the displayed code stops working when the daemon's timer lapses.)
  useEffect(() => {
    if (init.expiresAt === null) return;
    let timeoutId: ReturnType<typeof setTimeout>;
    const tick = () => {
      const remainingMs = (init.expiresAt as number) - Date.now();
      if (remainingMs <= 0) {
        setCountdown("0:00");
        getCurrentWindow().close();
        return;
      }
      setCountdown(formatRemaining(remainingMs));
      timeoutId = setTimeout(tick, 250);
    };
    tick();
    return () => clearTimeout(timeoutId);
  }, [init.expiresAt]);

  // Auto-close paths: the daemon confirms the exchange completed (success or
  // failure) and the listener closes the window after a brief status message.
  useEffect(() => {
    let cancelled = false;
    const unlisteners: Array<() => void> = [];

    (async () => {
      const unlistenCompleted = await listen("pair://responder-completed", () => {
        setStatus({ text: "Pairing complete.", kind: "ok" });
        setRejectDisabled(true);
        setTimeout(() => getCurrentWindow().close(), 800);
      });
      if (cancelled) {
        unlistenCompleted();
        return;
      }
      unlisteners.push(unlistenCompleted);

      const unlistenFailed = await listen<{ reason?: string }>(
        "pair://responder-failed",
        (event) => {
          const reason = event.payload?.reason;
          setStatus({
            text: reason ? `Pairing failed: ${reason}` : "Pairing failed.",
            kind: "error",
          });
          setRejectDisabled(true);
          setTimeout(() => getCurrentWindow().close(), 1500);
        },
      );
      if (cancelled) {
        unlistenFailed();
        return;
      }
      unlisteners.push(unlistenFailed);
    })();

    return () => {
      cancelled = true;
      for (const unlisten of unlisteners) unlisten();
    };
  }, []);

  // Reject is idempotent; ignore failure and close anyway.
  async function handleReject() {
    setRejectDisabled(true);
    try {
      await invoke("reject_inbound_pair");
    } catch {
      // ignore — reject is idempotent
    }
    getCurrentWindow().close();
  }

  return (
    <main data-tauri-drag-region className="window-backdrop flex h-screen flex-col items-center gap-2 bg-surface-base px-6 pb-6 pt-8 text-center text-text-primary">
      <p className="text-sm text-text-secondary">
        Pair request from <span className="font-semibold text-text-primary">{init.device}</span>
      </p>
      <p className="text-sm text-text-secondary">Enter this code on the other device:</p>
      <div className="my-2 pr-[0.4em] font-mono text-3xl font-semibold tracking-[0.4em]">{init.code}</div>
      <p className="text-sm text-text-secondary">Expires in {countdown}</p>

      {status && (
        <p
          className={
            status.kind === "error" ? "text-sm text-danger" : "text-sm text-text-secondary"
          }
        >
          {status.text}
        </p>
      )}

      <div className="mt-auto">
        <Button ghost onClick={handleReject} disabled={rejectDisabled}>
          Reject
        </Button>
      </div>
    </main>
  );
}
