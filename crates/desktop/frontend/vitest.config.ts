import { defineConfig } from "vitest/config";
import { playwright } from "@vitest/browser-playwright";
import react from "@vitejs/plugin-react";
import tailwindcss from "@tailwindcss/vite";

// Browser-mode vitest mirroring webdesserts/ui's harness (Playwright/chromium).
// The settings panel is a browser component — it renders ui's CSS-driven states
// and calls Tauri APIs — so it's exercised in a real browser with the Tauri
// modules mocked per-test. @webdesserts/ui resolves from node_modules (the
// installed git dependency), the same path the production build uses.
export default defineConfig({
  plugins: [react(), tailwindcss()],
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
