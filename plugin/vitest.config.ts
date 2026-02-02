import { defineConfig } from "vitest/config";
import path from "path";

export default defineConfig({
  test: {
    include: ["tests/**/*.test.ts"],
    environment: "node",
    globals: true,
  },
  resolve: {
    alias: {
      // Mock the obsidian module - tests use MockObsidianVault instead
      obsidian: path.resolve(__dirname, "tests/mocks/obsidian.ts"),
    },
  },
});
