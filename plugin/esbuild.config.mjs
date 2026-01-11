import esbuild from "esbuild";
import esbuildSvelte from "esbuild-svelte";
import sveltePreprocess from "svelte-preprocess";
import process from "process";
import builtins from "builtin-modules";
import { readFileSync } from "fs";
import { dirname, resolve } from "path";

const prod = process.argv[2] === "production";

// Build the desktop-only WebSocket server module separately (Node.js platform)
await esbuild.build({
  entryPoints: ["src/network/WebSocketServer.ts"],
  bundle: true,
  platform: "node",
  format: "cjs",
  external: [...builtins],
  outfile: "ws-server.js",
  minify: prod,
  sourcemap: prod ? false : "inline",
});

// Custom plugin to handle WASM binary imports.
// Loads .wasm files as base64-encoded ArrayBuffers that can be passed to initSync().
const wasmPlugin = {
  name: "wasm-loader",
  setup(build) {
    build.onResolve({ filter: /\.wasm$/ }, (args) => {
      // Resolve the path relative to the importer
      const resolvedPath = resolve(dirname(args.importer), args.path);
      return {
        path: resolvedPath,
        namespace: "wasm-binary",
      };
    });

    build.onLoad({ filter: /.*/, namespace: "wasm-binary" }, async (args) => {
      const wasmBuffer = readFileSync(args.path);
      const base64 = wasmBuffer.toString("base64");
      return {
        contents: `
          const base64 = "${base64}";
          const binary = Uint8Array.from(atob(base64), c => c.charCodeAt(0));
          export default binary.buffer;
        `,
        loader: "js",
      };
    });
  },
};

const context = await esbuild.context({
  entryPoints: ["src/main.ts"],
  bundle: true,
  external: [
    "obsidian",
    "electron",
    "@codemirror/autocomplete",
    "@codemirror/collab",
    "@codemirror/commands",
    "@codemirror/language",
    "@codemirror/lint",
    "@codemirror/search",
    "@codemirror/state",
    "@codemirror/view",
    "@lezer/common",
    "@lezer/highlight",
    "@lezer/lr",
    ...builtins,
    // Exclude ws-server - it's built separately and loaded at runtime on desktop
    "./ws-server.js",
  ],
  format: "cjs",
  target: "es2020",
  logLevel: "info",
  sourcemap: prod ? false : "inline",
  treeShaking: true,
  outfile: "main.js",
  minify: prod,
  plugins: [
    esbuildSvelte({
      preprocess: sveltePreprocess(),
      compilerOptions: { css: "injected" },
    }),
    wasmPlugin,
  ],
});

if (prod) {
  await context.rebuild();
  process.exit(0);
} else {
  await context.watch();
}
