import { defineConfig } from "vite";
import react from "@vitejs/plugin-react";
import tailwindcss from "@tailwindcss/vite";
import path from "path";

// webdesserts/ui is consumed via a source alias rather than a package install:
// ui/dist isn't built (and is gitignored), so aliasing the source gives instant
// HMR during co-development without a separate ui build step. The path is four
// hops up — frontend → desktop → crates → obsidian-memory — to the sibling
// `ui` checkout under /Users/michael/code/webdesserts.
export default defineConfig({
  plugins: [react(), tailwindcss()],
  resolve: {
    alias: {
      "@webdesserts/ui": path.resolve(__dirname, "../../../../ui/src/index.ts"),
    },
  },
  build: {
    outDir: "dist",
    rollupOptions: {
      // Multi-page build: the existing index entry plus the bundled React
      // settings window. The vanilla pair windows live in public/ and are
      // copied verbatim, so they don't need an entry here.
      input: {
        index: path.resolve(__dirname, "index.html"),
        settings: path.resolve(__dirname, "settings.html"),
      },
    },
  },
});
