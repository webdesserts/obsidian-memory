/**
 * Shared test wrapper that loads the settings window's styles (Tailwind + ui
 * tokens + neapolitan preset) so the panel renders the same way it does in the
 * real window. Mirrors webdesserts/ui's tests/test-wrapper.tsx.
 */

import "../src/windows/windows.css";

export function TestWrapper({ children }: { children: React.ReactNode }) {
  return (
    <div
      className="bg-surface-base text-text-primary antialiased"
      style={{ width: "480px", height: "360px", overflow: "hidden" }}
    >
      {children}
    </div>
  );
}
