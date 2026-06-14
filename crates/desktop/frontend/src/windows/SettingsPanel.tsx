import { useEffect, useState } from "react";
import { Button, TextInput } from "@webdesserts/ui";
import { invoke } from "@tauri-apps/api/core";
import { getCurrentWindow } from "@tauri-apps/api/window";
import { open as openDialog } from "@tauri-apps/plugin-dialog";

// Mirror of the Rust SettingsPayload (commands.rs). Absent stored values come
// back as empty strings, so these bind directly to the text inputs.
interface SettingsPayload {
  relay_url: string;
  vault_path: string;
}

// Client-side validation mirrors the Rust D3/D4 rules so the panel rejects bad
// input before a round-trip and the inline message matches what save_settings
// would have returned. The Rust side still runs these (plus an is_dir check on
// the vault path) and is the source of truth on save.
function relayUrlError(value: string): string | null {
  if (value === "" || value.startsWith("http://") || value.startsWith("https://")) {
    return null;
  }
  return "Relay URL must start with http:// or https://";
}

function vaultPathError(value: string): string | null {
  return value.trim() === "" ? "Vault folder is required" : null;
}

export function SettingsPanel() {
  const [relayUrl, setRelayUrl] = useState("");
  const [vaultPath, setVaultPath] = useState("");
  // Inline field errors from client validation, surfaced only after the user
  // attempts a save so the form doesn't scold them mid-typing.
  const [relayError, setRelayError] = useState<string | null>(null);
  const [vaultError, setVaultError] = useState<string | null>(null);
  // The unconditional post-save restart notice (D2) and the error string a
  // rejected save returns from Rust (e.g. the is_dir check). Mutually exclusive.
  const [saved, setSaved] = useState(false);
  const [saveError, setSaveError] = useState<string | null>(null);

  useEffect(() => {
    invoke<SettingsPayload>("get_settings")
      .then((settings) => {
        setRelayUrl(settings.relay_url);
        setVaultPath(settings.vault_path);
      })
      .catch((err) => {
        setSaveError(String(err));
      });
  }, []);

  async function handleChoose() {
    const selected = await openDialog({ directory: true });
    // The dialog returns null when cancelled and (with directory: true) a single
    // path string on selection.
    if (typeof selected === "string") {
      setVaultPath(selected);
      setVaultError(null);
    }
  }

  async function handleSave() {
    const nextRelayError = relayUrlError(relayUrl);
    const nextVaultError = vaultPathError(vaultPath);
    setRelayError(nextRelayError);
    setVaultError(nextVaultError);
    if (nextRelayError || nextVaultError) {
      setSaved(false);
      setSaveError(null);
      return;
    }

    try {
      await invoke("save_settings", { relayUrl, vaultPath });
      setSaved(true);
      setSaveError(null);
    } catch (err) {
      setSaved(false);
      setSaveError(String(err));
    }
  }

  function handleCancel() {
    getCurrentWindow().close();
  }

  return (
    <main className="flex h-screen flex-col gap-5 bg-surface-base p-6 text-text-primary">
      <h1 className="text-lg font-medium">Memory Settings</h1>

      <div className="flex flex-1 flex-col gap-5">
        <div className="flex flex-col gap-1.5">
          <label htmlFor="relay-url" className="text-sm font-medium text-text-secondary">
            Relay URL
          </label>
          <TextInput
            id="relay-url"
            value={relayUrl}
            invalid={relayError !== null}
            placeholder="https://umbra.computer/"
            onChange={(event) => {
              setRelayUrl(event.target.value);
              setRelayError(null);
            }}
          />
          {relayError && <p className="text-xs text-danger">{relayError}</p>}
        </div>

        <div className="flex flex-col gap-1.5">
          <label htmlFor="vault-path" className="text-sm font-medium text-text-secondary">
            Vault Folder
          </label>
          <div className="flex items-stretch gap-2">
            <TextInput
              id="vault-path"
              wrapperClassName="flex-1"
              value={vaultPath}
              invalid={vaultError !== null}
              placeholder="/path/to/your/vault"
              onChange={(event) => {
                setVaultPath(event.target.value);
                setVaultError(null);
              }}
            />
            <Button onClick={handleChoose}>Choose…</Button>
          </div>
          {vaultError && <p className="text-xs text-danger">{vaultError}</p>}
        </div>
      </div>

      {/* Save outcome — the restart notice on success, the Rust error otherwise. */}
      {saved && (
        <p className="text-sm text-text-secondary">
          Changes take effect when the app is restarted.
        </p>
      )}
      {saveError && <p className="text-sm text-danger">{saveError}</p>}

      <div className="flex justify-end gap-2">
        <Button ghost onClick={handleCancel}>
          Cancel
        </Button>
        <Button onClick={handleSave}>Save</Button>
      </div>
    </main>
  );
}
