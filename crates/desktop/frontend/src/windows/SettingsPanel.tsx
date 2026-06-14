import { Button, TextInput } from "@webdesserts/ui";

// Consumption-proof stub for the settings window. Its only job is to prove the
// dormant React/Vite/Tailwind pipeline can render webdesserts/ui components
// styled — the alias resolves, Tailwind generates ui's classes, the semantic
// tokens apply, and motion (a transitive dep of ui's barrel) resolves at build.
// The real panel (relay URL + vault folder, Rust-backed) replaces this in a
// follow-up dispatch.
export function SettingsPanel() {
  return (
    <main className="flex flex-col gap-4 bg-surface-base p-6 text-text-primary">
      <h1 className="text-lg font-medium">Memory Settings</h1>

      <label className="flex flex-col gap-1.5">
        <span className="text-sm font-medium text-text-secondary">
          Relay URL
        </span>
        <TextInput
          size="md"
          placeholder="https://umbra.computer/"
        />
      </label>

      {/* Exercise the invalid state so the danger rule is part of the proof. */}
      <label className="flex flex-col gap-1.5">
        <span className="text-sm font-medium text-text-secondary">
          Vault Folder
        </span>
        <TextInput size="md" invalid defaultValue="/not/a/real/path" />
      </label>

      <div className="flex gap-2">
        <Button>Save</Button>
      </div>
    </main>
  );
}
