import { defineConfig } from "vitest/config";
import { playwright } from "@vitest/browser-playwright";
import react from "@vitejs/plugin-react";
import tailwindcss from "@tailwindcss/vite";
import path from "path";

// Browser-mode vitest mirroring webdesserts/ui's harness (Playwright/chromium).
// The settings panel is a browser component — it renders ui's CSS-driven states
// and calls Tauri APIs — so it's exercised in a real browser with the Tauri
// modules mocked per-test. The @webdesserts/ui alias matches vite.config.ts so
// the panel resolves ui from source.
export default defineConfig({
  plugins: [react(), tailwindcss()],
  resolve: {
    alias: {
      "@webdesserts/ui": path.resolve(__dirname, "../../../../ui/src/index.ts"),
    },
  },
  test: {
    browser: {
      enabled: true,
      provider: playwright({ contextOptions: { deviceScaleFactor: 2 } }),
      headless: true,
      screenshotFailures: false,
      instances: [
        {
          browser: "chromium",
          viewport: { width: 480, height: 360 },
        },
      ],
    },
  },
});
