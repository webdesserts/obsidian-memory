import { defineConfig } from "vite";
import react from "@vitejs/plugin-react";
import tailwindcss from "@tailwindcss/vite";
import path from "path";

export default defineConfig({
  plugins: [react(), tailwindcss()],
  build: {
    outDir: "dist",
    rollupOptions: {
      // Multi-page build: the tray index entry plus each bundled React window.
      // The pair windows are React entries (migrated from the old vanilla
      // public/windows/ HTML), so they get their own rollup inputs here.
      input: {
        index: path.resolve(__dirname, "index.html"),
        settings: path.resolve(__dirname, "settings.html"),
        pairInitiator: path.resolve(__dirname, "pair-initiator.html"),
      },
    },
  },
});
