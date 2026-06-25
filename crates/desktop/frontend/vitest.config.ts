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
      provider: playwright(),
      headless: true,
      screenshotFailures: false,
      // Panel snapshots match exactly except at the corners, where the
      // decorative window-backdrop gradient (the vignette + diagonal warm/cool
      // tints, steepest at the corners) rasterizes slightly differently across
      // GPUs. That fuzz measured ~584px at deviceScaleFactor 2 and is smaller at
      // native scale; 400 covers it with margin while staying far below any real
      // content change (the panel body itself is exact; content diffs land in
      // the center and run to thousands of px).
      expect: {
        toMatchScreenshot: {
          comparatorName: "pixelmatch",
          comparatorOptions: { allowedMismatchedPixels: 400 },
        },
      },
      instances: [
        {
          browser: "chromium",
          viewport: { width: 480, height: 360 },
        },
      ],
    },
  },
});
