import fs from "fs/promises";
import path from "path";
import os from "os";
import { DateTime } from "luxon";
import { describe, it, expect, beforeEach, afterEach } from "vitest";
import { addLog } from "./Log.js";

describe("addLog", () => {
  const testVaultPath = path.join(os.tmpdir(), "obsidian-memory-test-vault-log");
  const logPath = path.join(testVaultPath, "Log.md");

  beforeEach(async () => {
    // Create test vault directory
    await fs.mkdir(testVaultPath, { recursive: true });
  });

  afterEach(async () => {
    // Clean up test vault
    await fs.rm(testVaultPath, { recursive: true, force: true });
  });

  it("should create a new log file with day header", async () => {
    const time = DateTime.fromISO("2025-11-24T14:30:00");
    await addLog(logPath, time, "First entry");

    const content = await fs.readFile(logPath, "utf-8");
    expect(content).toContain("## 2025-W48-1 (Mon)");
    expect(content).toContain("- 2:30 PM – First entry");
  });

  it("should add entries to existing day section in chronological order", async () => {
    // Add first entry at 2:30 PM
    const time1 = DateTime.fromISO("2025-11-24T14:30:00");
    await addLog(logPath, time1, "Second entry");

    // Add earlier entry at 1:00 AM
    const time2 = DateTime.fromISO("2025-11-24T01:00:00");
    await addLog(logPath, time2, "Earlier entry");

    // Add third entry at 3:00 PM
    const time3 = DateTime.fromISO("2025-11-24T15:00:00");
    await addLog(logPath, time3, "Third entry");

    const content = await fs.readFile(logPath, "utf-8");
    const entryLines = content.split("\n").filter((line) => line.startsWith("- "));

    expect(entryLines).toHaveLength(3);
    expect(entryLines[0]).toContain("1:00 AM – Earlier entry");
    expect(entryLines[1]).toContain("2:30 PM – Second entry");
    expect(entryLines[2]).toContain("3:00 PM – Third entry");
  });

  it("should create new day sections when day changes", async () => {
    const time1 = DateTime.fromISO("2025-11-24T09:00:00");
    await addLog(logPath, time1, "Entry day 1");

    const time2 = DateTime.fromISO("2025-11-25T09:00:00");
    await addLog(logPath, time2, "Entry day 2");

    const content = await fs.readFile(logPath, "utf-8");
    const headers = content.split("\n").filter((line) => line.startsWith("##"));

    expect(headers).toHaveLength(2);
    expect(headers[0]).toContain("2025-W48-1 (Mon)");
    expect(headers[1]).toContain("2025-W48-2 (Tue)");
  });

  it("should strip leading dash from content", async () => {
    const time = DateTime.fromISO("2025-11-24T09:00:00");
    await addLog(logPath, time, "- Entry with dash");

    const content = await fs.readFile(logPath, "utf-8");
    expect(content).toContain("9:00 AM – Entry with dash");
    expect(content).not.toContain("- - Entry");
  });

  it("should use 12-hour time format", async () => {
    const time1 = DateTime.fromISO("2025-11-24T09:30:00");
    await addLog(logPath, time1, "Morning entry");

    const time2 = DateTime.fromISO("2025-11-24T14:45:00");
    await addLog(logPath, time2, "Afternoon entry");

    const content = await fs.readFile(logPath, "utf-8");
    expect(content).toContain("9:30 AM – Morning entry");
    expect(content).toContain("2:45 PM – Afternoon entry");
  });

  it("should handle midnight and noon correctly", async () => {
    const midnight = DateTime.fromISO("2025-11-24T00:00:00");
    await addLog(logPath, midnight, "Midnight entry");

    const noon = DateTime.fromISO("2025-11-24T12:00:00");
    await addLog(logPath, noon, "Noon entry");

    const content = await fs.readFile(logPath, "utf-8");
    expect(content).toContain("12:00 AM – Midnight entry");
    expect(content).toContain("12:00 PM – Noon entry");
  });

  it("should preserve existing entries when adding to a day section", async () => {
    // Create initial log with existing entry
    const initialContent = `## 2025-W48-1 (Mon)

- 9:00 AM – Existing entry
`;
    await fs.writeFile(logPath, initialContent);

    // Add new entry after existing one
    const time = DateTime.fromISO("2025-11-24T10:00:00");
    await addLog(logPath, time, "New entry");

    const content = await fs.readFile(logPath, "utf-8");
    expect(content).toContain("9:00 AM – Existing entry");
    expect(content).toContain("10:00 AM – New entry");
  });
});
