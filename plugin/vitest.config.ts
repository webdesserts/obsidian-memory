import { defineConfig } from "vitest/config";

export default defineConfig({
  test: {
    include: ["tests/**/*.test.ts"],
    environment: "node",
    globals: true,
  },
  resolve: {
    alias: {
      // Mock the obsidian module - tests use MockObsidianVault instead
      obsidian: "./tests/mocks/obsidian.ts",
    },
  },
});
