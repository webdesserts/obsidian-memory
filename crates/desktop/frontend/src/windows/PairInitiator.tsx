import { useEffect, useRef, useState } from "react";
import { Button, TextInput } from "@webdesserts/ui";
import { invoke } from "@tauri-apps/api/core";
import { listen } from "@tauri-apps/api/event";
import { getCurrentWindow } from "@tauri-apps/api/window";

// Mirror of the Rust serde structs (commands.rs). MeshDiscoveredPayload is the
// per-mesh discovery event payload; PairSuccessResult is what request_pairing
// and submit_pair_code resolve to.
interface MeshDiscoveredPayload {
  vault_id: string;
  mesh_name: string;
  online_count: number;
}

interface PairSuccessResult {
  device_name: string;
}

// The code field accepts exactly a 6-digit numeric string; Pair is gated on it.
function isCompleteCode(value: string): boolean {
  return /^\d{6}$/.test(value);
}

export function PairInitiator() {
  // Discovered meshes, deduped by vault_id. A ref (not state) because discovery
  // events fire from a Tauri listener and we only need the Map to derive the
  // <option> list + the "found N" counts — the option list itself is state.
  const meshesRef = useRef(new Map<string, MeshDiscoveredPayload>());
  const [meshes, setMeshes] = useState<MeshDiscoveredPayload[]>([]);
  const [selectedVaultId, setSelectedVaultId] = useState("");

  // Stage gating: stage 2 (code entry) stays hidden until request_pairing
  // resolves. requestSent disables stage 1 (mesh select + Request) while the
  // request is in flight or has succeeded.
  const [stage2Visible, setStage2Visible] = useState(false);
  const [requestSent, setRequestSent] = useState(false);
  const [code, setCode] = useState("");
  const [pairing, setPairing] = useState(false);

  const [scanStatus, setScanStatus] = useState("Scanning for meshes on this network…");
  const [codePrompt, setCodePrompt] = useState(
    "Enter the code shown on the other device:",
  );
  // status carries the success/error line under the buttons. `kind` selects the
  // token color (danger for errors, secondary for the neutral success notice).
  const [status, setStatus] = useState<{ text: string; kind: "error" | "ok" } | null>(
    null,
  );

  const codeInputRef = useRef<HTMLInputElement>(null);

  // Single mount effect: register BOTH discovery listeners and ONLY THEN start
  // discovery. Splitting the listens and the invoke into separate effects would
  // race — the daemon can emit pair://mesh-discovered before a listener is
  // registered, dropping the event. Awaiting both listens before the invoke
  // mirrors the original vanilla IIFE's ordering guarantee.
  useEffect(() => {
    let cancelled = false;
    const unlisteners: Array<() => void> = [];

    (async () => {
      const unlistenDiscovered = await listen<MeshDiscoveredPayload>(
        "pair://mesh-discovered",
        (event) => {
          const mesh = event.payload;
          if (meshesRef.current.has(mesh.vault_id)) return;
          meshesRef.current.set(mesh.vault_id, mesh);
          setMeshes(Array.from(meshesRef.current.values()));
          setScanStatus(`Scanning… found ${meshesRef.current.size} so far`);
        },
      );
      unlisteners.push(unlistenDiscovered);

      const unlistenFinished = await listen("pair://discovery-finished", () => {
        setScanStatus(
          meshesRef.current.size === 0
            ? "No meshes found."
            : `Found ${meshesRef.current.size} mesh(es).`,
        );
      });
      unlisteners.push(unlistenFinished);

      // Both listeners are live — now it's safe to start discovery.
      if (cancelled) return;
      try {
        await invoke("start_pair_discovery");
      } catch (e) {
        setScanStatus(`Discovery failed: ${e}`);
      }
    })();

    return () => {
      cancelled = true;
      for (const unlisten of unlisteners) unlisten();
    };
  }, []);

  // Step 1: connect + park (triggers the responder to show the code).
  async function handleRequest() {
    setRequestSent(true);
    setStatus(null);
    const meshName = meshesRef.current.get(selectedVaultId)?.mesh_name ?? "the other device";
    setScanStatus(`Waiting for code on ${meshName}…`);

    try {
      const result = await invoke<PairSuccessResult>("request_pairing", {
        vaultId: selectedVaultId,
      });
      // Connection established — the responder is now showing the code.
      setCodePrompt(`Enter the code shown on ${result.device_name}:`);
      setStage2Visible(true);
      codeInputRef.current?.focus();
    } catch (e) {
      setStatus({ text: `Connection failed: ${e}`, kind: "error" });
      // Re-enable stage 1 so the user can retry.
      setRequestSent(false);
      setScanStatus(
        meshesRef.current.size > 0
          ? `Found ${meshesRef.current.size} mesh(es).`
          : "No meshes found.",
      );
    }
  }

  // Step 2: submit the typed code to complete the HMAC exchange.
  async function handlePair() {
    setPairing(true);
    setStatus({ text: "Pairing…", kind: "ok" });
    try {
      const result = await invoke<PairSuccessResult>("submit_pair_code", {
        vaultId: selectedVaultId,
        code,
      });
      setStatus({ text: `Paired with ${result.device_name}. Closing…`, kind: "ok" });
      setTimeout(() => getCurrentWindow().close(), 1500);
    } catch (e) {
      setStatus({ text: `Pairing failed: ${e}`, kind: "error" });
      setPairing(false);
    }
  }

  // Cancel from either stage — cancellation is idempotent, so ignore errors.
  async function handleCancel() {
    try {
      await invoke("cancel_pair_discovery");
    } catch {
      // ignore — cancellation is idempotent
    }
    getCurrentWindow().close();
  }

  const requestDisabled = !selectedVaultId || requestSent;
  const pairDisabled = !isCompleteCode(code) || pairing;

  return (
    <main className="dot-grid flex h-screen flex-col gap-4 bg-surface-base px-6 pb-6 pt-8 text-text-primary">
      <h1 className="text-lg font-medium">Pair with nearby device</h1>
      <p className="text-sm text-text-secondary">{scanStatus}</p>

      {/* Stage 1: mesh selection + request */}
      <div className="flex flex-col gap-1.5">
        <label htmlFor="mesh" className="text-sm font-medium text-text-secondary">
          Mesh
        </label>
        <select
          id="mesh"
          className="rounded-sm border border-text-secondary/30 bg-surface-base px-2 py-1.5 text-sm text-text-primary"
          value={selectedVaultId}
          disabled={requestSent}
          onChange={(event) => setSelectedVaultId(event.target.value)}
        >
          <option disabled value="">
            —
          </option>
          {meshes.map((mesh) => (
            <option key={mesh.vault_id} value={mesh.vault_id}>
              {`${mesh.mesh_name} (${mesh.online_count} online)`}
            </option>
          ))}
        </select>
      </div>

      {/* Stage 2: code entry + pair (hidden until request_pairing succeeds) */}
      {stage2Visible && (
        <div className="flex flex-col gap-1.5">
          <label htmlFor="code" className="text-sm font-medium text-text-secondary">
            {codePrompt}
          </label>
          <TextInput
            id="code"
            ref={codeInputRef}
            wrapperClassName="w-32"
            className="text-center font-mono tracking-[0.3em]"
            value={code}
            maxLength={6}
            inputMode="numeric"
            pattern="[0-9]*"
            autoComplete="off"
            onChange={(event) => setCode(event.target.value)}
          />
        </div>
      )}

      {status && (
        <p
          className={
            status.kind === "error" ? "text-sm text-danger" : "text-sm text-text-secondary"
          }
        >
          {status.text}
        </p>
      )}

      <div className="mt-auto flex justify-end gap-2">
        <Button ghost onClick={handleCancel}>
          Cancel
        </Button>
        {stage2Visible ? (
          <Button onClick={handlePair} disabled={pairDisabled}>
            Pair
          </Button>
        ) : (
          <Button onClick={handleRequest} disabled={requestDisabled}>
            Request pairing
          </Button>
        )}
      </div>
    </main>
  );
}
