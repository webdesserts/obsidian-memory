import fs from "fs/promises";
import path from "path";
import os from "os";
import { describe, it, expect, beforeEach, afterEach } from "vitest";
import { writeLogs } from "./WriteLogs.js";

describe("writeLogs", () => {
  const testVaultPath = path.join(os.tmpdir(), "obsidian-memory-test-vault-writelogs");
  const logPath = path.join(testVaultPath, "Log.md");

  beforeEach(async () => {
    // Create test vault directory
    await fs.mkdir(testVaultPath, { recursive: true });
  });

  afterEach(async () => {
    // Clean up test vault
    await fs.rm(testVaultPath, { recursive: true, force: true });
  });

  it("should create a new log file with day section", async () => {
    const result = await writeLogs(logPath, "2025-W50-1", {
      "9:00 AM": "First entry",
      "2:30 PM": "Second entry",
    });

    expect(result.errors).toEqual([]);
    expect(result.count).toBe(2);

    const content = await fs.readFile(logPath, "utf-8");
    expect(content).toContain("## 2025-W50-1 (Mon)");
    expect(content).toContain("- 9:00 AM – First entry");
    expect(content).toContain("- 2:30 PM – Second entry");
  });

  it("should sort entries chronologically", async () => {
    const result = await writeLogs(logPath, "2025-W50-1", {
      "2:30 PM": "Afternoon entry",
      "9:00 AM": "Morning entry",
      "12:00 PM": "Noon entry",
    });

    expect(result.errors).toEqual([]);
    expect(result.count).toBe(3);

    const content = await fs.readFile(logPath, "utf-8");
    const entryLines = content.split("\n").filter((line) => line.startsWith("- "));

    expect(entryLines).toHaveLength(3);
    expect(entryLines[0]).toContain("9:00 AM – Morning entry");
    expect(entryLines[1]).toContain("12:00 PM – Noon entry");
    expect(entryLines[2]).toContain("2:30 PM – Afternoon entry");
  });

  it("should replace existing day section", async () => {
    // Create initial log with existing entries
    const initialContent = `## 2025-W50-1 (Mon)

- 9:00 AM – Original entry 1
- 10:00 AM – Original entry 2
- 11:00 AM – Original entry 3

## 2025-W50-2 (Tue)

- 9:00 AM – Tuesday entry
`;
    await fs.writeFile(logPath, initialContent);

    // Replace Monday's entries
    const result = await writeLogs(logPath, "2025-W50-1", {
      "9:00 AM": "Consolidated entry",
      "2:30 PM": "Summary of afternoon work",
    });

    expect(result.errors).toEqual([]);
    expect(result.count).toBe(2);

    const content = await fs.readFile(logPath, "utf-8");

    // Monday section should be replaced
    expect(content).toContain("- 9:00 AM – Consolidated entry");
    expect(content).toContain("- 2:30 PM – Summary of afternoon work");
    expect(content).not.toContain("Original entry");

    // Tuesday section should be untouched
    expect(content).toContain("## 2025-W50-2 (Tue)");
    expect(content).toContain("- 9:00 AM – Tuesday entry");
  });

  it("should append new day section if it doesn't exist", async () => {
    // Create initial log with Monday
    const initialContent = `## 2025-W50-1 (Mon)

- 9:00 AM – Monday entry
`;
    await fs.writeFile(logPath, initialContent);

    // Add Tuesday entries
    const result = await writeLogs(logPath, "2025-W50-2", {
      "10:00 AM": "Tuesday entry",
    });

    expect(result.errors).toEqual([]);
    expect(result.count).toBe(1);

    const content = await fs.readFile(logPath, "utf-8");

    // Both sections should exist
    expect(content).toContain("## 2025-W50-1 (Mon)");
    expect(content).toContain("- 9:00 AM – Monday entry");
    expect(content).toContain("## 2025-W50-2 (Tue)");
    expect(content).toContain("- 10:00 AM – Tuesday entry");
  });

  it("should validate ISO week date format", async () => {
    const result = await writeLogs(logPath, "invalid-date", {
      "9:00 AM": "Entry",
    });

    expect(result.count).toBe(0);
    expect(result.errors).toHaveLength(1);
    expect(result.errors[0]).toContain("Invalid ISO week date format");
    expect(result.errors[0]).toContain("invalid-date");
  });

  it("should validate time format", async () => {
    const result = await writeLogs(logPath, "2025-W50-1", {
      "9:00 AM": "Valid entry",
      "25:00": "Invalid time",
      "not a time": "Another invalid entry",
    });

    expect(result.count).toBe(0);
    expect(result.errors).toHaveLength(2);
    expect(result.errors[0]).toContain("Invalid time format: '25:00'");
    expect(result.errors[1]).toContain("Invalid time format: 'not a time'");
  });

  it("should handle midnight and noon correctly", async () => {
    const result = await writeLogs(logPath, "2025-W50-1", {
      "12:00 AM": "Midnight entry",
      "12:00 PM": "Noon entry",
    });

    expect(result.errors).toEqual([]);
    expect(result.count).toBe(2);

    const content = await fs.readFile(logPath, "utf-8");
    expect(content).toContain("- 12:00 AM – Midnight entry");
    expect(content).toContain("- 12:00 PM – Noon entry");
  });

  it("should preserve other day sections when replacing one", async () => {
    // Create log with multiple days
    const initialContent = `## 2025-W50-1 (Mon)

- 9:00 AM – Monday original

## 2025-W50-2 (Tue)

- 10:00 AM – Tuesday original

## 2025-W50-3 (Wed)

- 11:00 AM – Wednesday original
`;
    await fs.writeFile(logPath, initialContent);

    // Replace Tuesday only
    const result = await writeLogs(logPath, "2025-W50-2", {
      "2:00 PM": "Tuesday replaced",
    });

    expect(result.errors).toEqual([]);
    expect(result.count).toBe(1);

    const content = await fs.readFile(logPath, "utf-8");

    // Monday should be untouched
    expect(content).toContain("## 2025-W50-1 (Mon)");
    expect(content).toContain("- 9:00 AM – Monday original");

    // Tuesday should be replaced
    expect(content).toContain("## 2025-W50-2 (Tue)");
    expect(content).toContain("- 2:00 PM – Tuesday replaced");
    expect(content).not.toContain("Tuesday original");

    // Wednesday should be untouched
    expect(content).toContain("## 2025-W50-3 (Wed)");
    expect(content).toContain("- 11:00 AM – Wednesday original");
  });

  it("should format entries with en-dash separator", async () => {
    const result = await writeLogs(logPath, "2025-W50-1", {
      "9:00 AM": "Entry with special – characters",
    });

    expect(result.errors).toEqual([]);

    const content = await fs.readFile(logPath, "utf-8");
    // Should use en-dash (–) between time and content
    expect(content).toContain("- 9:00 AM – Entry with special – characters");
  });

  it("should delete entire day section when passed empty object", async () => {
    // Create log with multiple days
    const initialContent = `## 2025-W50-1 (Mon)

- 9:00 AM – Monday entry 1
- 10:00 AM – Monday entry 2

## 2025-W50-2 (Tue)

- 11:00 AM – Tuesday entry

## 2025-W50-3 (Wed)

- 12:00 PM – Wednesday entry
`;
    await fs.writeFile(logPath, initialContent);

    // Delete Monday section
    const result = await writeLogs(logPath, "2025-W50-1", {});

    expect(result.errors).toEqual([]);
    expect(result.deleted).toBe(true);
    expect(result.count).toBe(0);

    const content = await fs.readFile(logPath, "utf-8");

    // Monday section should be gone
    expect(content).not.toContain("## 2025-W50-1 (Mon)");
    expect(content).not.toContain("Monday entry");

    // Other sections should remain
    expect(content).toContain("## 2025-W50-2 (Tue)");
    expect(content).toContain("- 11:00 AM – Tuesday entry");
    expect(content).toContain("## 2025-W50-3 (Wed)");
    expect(content).toContain("- 12:00 PM – Wednesday entry");
  });

  it("should return deleted:false when trying to delete non-existent day", async () => {
    // Create log with Monday only
    const initialContent = `## 2025-W50-1 (Mon)

- 9:00 AM – Monday entry
`;
    await fs.writeFile(logPath, initialContent);

    // Try to delete Tuesday (doesn't exist)
    const result = await writeLogs(logPath, "2025-W50-2", {});

    expect(result.errors).toEqual([]);
    expect(result.deleted).toBe(false);
    expect(result.count).toBe(0);

    // Monday should still be there
    const content = await fs.readFile(logPath, "utf-8");
    expect(content).toContain("## 2025-W50-1 (Mon)");
    expect(content).toContain("- 9:00 AM – Monday entry");
  });

  it("should clean up extra blank lines after deletion", async () => {
    // Create log with spacing between sections
    const initialContent = `## 2025-W50-1 (Mon)

- 9:00 AM – Monday entry


## 2025-W50-2 (Tue)

- 10:00 AM – Tuesday entry
`;
    await fs.writeFile(logPath, initialContent);

    // Delete Monday
    const result = await writeLogs(logPath, "2025-W50-1", {});

    expect(result.deleted).toBe(true);

    const content = await fs.readFile(logPath, "utf-8");

    // Should not have triple+ newlines
    expect(content).not.toMatch(/\n{3,}/);
  });

  it("should delete last remaining day section", async () => {
    // Create log with only Monday
    const initialContent = `## 2025-W50-1 (Mon)

- 9:00 AM – Monday entry
`;
    await fs.writeFile(logPath, initialContent);

    // Delete Monday
    const result = await writeLogs(logPath, "2025-W50-1", {});

    expect(result.deleted).toBe(true);

    const content = await fs.readFile(logPath, "utf-8");

    // File should be essentially empty (maybe just whitespace)
    expect(content.trim()).toBe("");
  });

  it("should handle extra non-day headers within a day section", async () => {
    // Create log with invalid headers mixed in
    const initialContent = `## 2025-W50-1 (Mon)

- 9:00 AM – Started work

## Notes from standup

Bob mentioned priority changes

- 2:30 PM – Finished work

## 2025-W50-2 (Tue)

- 10:00 AM – Tuesday entry
`;
    await fs.writeFile(logPath, initialContent);

    // Replace Monday - should include everything up to the next valid day header
    const result = await writeLogs(logPath, "2025-W50-1", {
      "11:00 AM": "Consolidated Monday work",
    });

    expect(result.errors).toEqual([]);
    expect(result.count).toBe(1);

    const content = await fs.readFile(logPath, "utf-8");

    // Monday section should be replaced, including the extra headers
    expect(content).toContain("## 2025-W50-1 (Mon)");
    expect(content).toContain("- 11:00 AM – Consolidated Monday work");
    expect(content).not.toContain("Notes from standup");
    expect(content).not.toContain("9:00 AM");
    expect(content).not.toContain("2:30 PM");

    // Tuesday should be unchanged
    expect(content).toContain("## 2025-W50-2 (Tue)");
    expect(content).toContain("- 10:00 AM – Tuesday entry");
  });
});
