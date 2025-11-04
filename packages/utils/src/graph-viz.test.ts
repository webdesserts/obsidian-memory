import { describe, it, expect } from "vitest";
import { renderGraphTree, type NeighborhoodNode } from "./graph-viz.js";

describe("renderGraphTree()", () => {
  it("should render basic tree with forward and backward links", () => {
    const neighborhood = new Map<string, NeighborhoodNode>([
      [
        "West Coast Swing",
        {
          distance: 1,
          linkType: "forward",
          directLinks: ["Compression", "Connection"],
          backlinks: [],
        },
      ],
      [
        "Compression",
        {
          distance: 2,
          linkType: "forward",
          directLinks: [],
          backlinks: [],
        },
      ],
      [
        "Connection",
        {
          distance: 2,
          linkType: "backward",
          directLinks: [],
          backlinks: [],
        },
      ],
    ]);

    const result = renderGraphTree(
      "Anchor Step",
      neighborhood,
      new Set(),
      () => undefined
    );

    expect(result).toContain("Exploring from: Anchor Step");
    expect(result).toContain("└─→ West Coast Swing");
    expect(result).toContain("    ├─→ Compression");
    expect(result).toContain("    └─← Connection");
  });

  it("should sort forward links before backlinks", () => {
    const neighborhood = new Map<string, NeighborhoodNode>([
      [
        "Parent",
        {
          distance: 1,
          linkType: "forward",
          directLinks: ["Alpha", "Beta", "Gamma"],
          backlinks: [],
        },
      ],
      [
        "Alpha",
        {
          distance: 2,
          linkType: "backward",
          directLinks: [],
          backlinks: ["Parent"],
        },
      ],
      [
        "Beta",
        {
          distance: 2,
          linkType: "forward",
          directLinks: ["Parent"],
          backlinks: [],
        },
      ],
      [
        "Gamma",
        {
          distance: 2,
          linkType: "forward",
          directLinks: ["Parent"],
          backlinks: [],
        },
      ],
    ]);

    const result = renderGraphTree(
      "Start",
      neighborhood,
      new Set(),
      () => undefined
    );

    const lines = result.split("\n");
    const childrenStart = lines.findIndex((line) => line.includes("Parent"));

    // Forward links (Beta, Gamma) should come before backward link (Alpha)
    const betaIndex = lines.findIndex((line) => line.includes("Beta"));
    const gammaIndex = lines.findIndex((line) => line.includes("Gamma"));
    const alphaIndex = lines.findIndex((line) => line.includes("Alpha"));

    expect(betaIndex).toBeGreaterThan(childrenStart);
    expect(gammaIndex).toBeGreaterThan(childrenStart);
    expect(alphaIndex).toBeGreaterThan(betaIndex);
    expect(alphaIndex).toBeGreaterThan(gammaIndex);
  });

  it("should group multi-parent nodes by connection strength", () => {
    const neighborhood = new Map<string, NeighborhoodNode>([
      [
        "West Coast Swing",
        {
          distance: 1,
          linkType: "forward",
          directLinks: ["Compression", "Stretch", "Connection"],
          backlinks: [],
        },
      ],
      [
        "Pattern",
        {
          distance: 1,
          linkType: "forward",
          directLinks: ["Compression"],
          backlinks: [],
        },
      ],
      [
        "Compression",
        {
          distance: 2,
          linkType: "forward",
          directLinks: ["Connection", "West Coast Swing", "Pattern"],
          backlinks: [],
        },
      ],
      [
        "Stretch",
        {
          distance: 2,
          linkType: "forward",
          directLinks: ["Connection", "West Coast Swing"],
          backlinks: [],
        },
      ],
      [
        "Connection",
        {
          distance: 2,
          linkType: "forward",
          directLinks: ["West Coast Swing", "Compression", "Stretch"],
          backlinks: [],
        },
      ],
    ]);

    const result = renderGraphTree(
      "Anchor Step",
      neighborhood,
      new Set(),
      () => undefined
    );

    // Compression should be grouped under West Coast Swing (stronger connection cluster)
    // because Compression connects to WCS + Stretch + Connection (3 links in WCS cluster)
    // vs Pattern (1 link)
    const lines = result.split("\n");
    const wcsIndex = lines.findIndex((line) =>
      line.includes("West Coast Swing")
    );
    const compressionIndex = lines.findIndex((line) =>
      line.includes("Compression")
    );
    const patternIndex = lines.findIndex((line) => line.includes("Pattern"));

    // Compression should appear as child of WCS (after WCS line)
    expect(compressionIndex).toBeGreaterThan(wcsIndex);

    // Compression should be indented under WCS (4 spaces)
    expect(lines[compressionIndex]).toMatch(/^\s{4}/);

    // Compression should NOT be under Pattern
    // Check that there's no Compression between Pattern and the next top-level node
    // Pattern and WCS are both distance-1, so one comes before the other
    if (patternIndex < wcsIndex) {
      const linesAfterPattern = lines.slice(patternIndex + 1, wcsIndex);
      const hasCompressionAfterPattern = linesAfterPattern.some((line) =>
        line.includes("Compression")
      );
      expect(hasCompressionAfterPattern).toBe(false);
    }
  });

  it("should show path for duplicate note names", () => {
    const neighborhood = new Map<string, NeighborhoodNode>([
      [
        "Settings",
        {
          distance: 1,
          linkType: "forward",
          directLinks: [],
          backlinks: [],
        },
      ],
    ]);

    const duplicates = new Set(["Settings"]);
    const getPath = (name: string) =>
      name === "Settings" ? "knowledge/Settings" : undefined;

    const result = renderGraphTree("Start", neighborhood, duplicates, getPath);

    expect(result).toContain("Settings (knowledge/Settings)");
  });

  it("should handle orphan nodes gracefully", () => {
    // Orphan node has no connections to distance-1 parents
    const neighborhood = new Map<string, NeighborhoodNode>([
      [
        "Parent",
        {
          distance: 1,
          linkType: "forward",
          directLinks: ["Child"],
          backlinks: [],
        },
      ],
      [
        "Child",
        {
          distance: 2,
          linkType: "forward",
          directLinks: [],
          backlinks: [],
        },
      ],
      [
        "Orphan",
        {
          distance: 2,
          linkType: "forward",
          directLinks: [],
          backlinks: [],
        },
      ],
    ]);

    const result = renderGraphTree(
      "Start",
      neighborhood,
      new Set(),
      () => undefined
    );

    // Should not crash, orphan should be skipped
    expect(result).toContain("Parent");
    expect(result).toContain("Child");
    expect(result).not.toContain("Orphan");
  });

  it("should calculate unexplored counts correctly", () => {
    const neighborhood = new Map<string, NeighborhoodNode>([
      [
        "Parent",
        {
          distance: 1,
          linkType: "forward",
          directLinks: ["Child"],
          backlinks: [],
        },
      ],
      [
        "Child",
        {
          distance: 2,
          linkType: "forward",
          directLinks: ["Note1", "Note2", "Note3"],
          backlinks: ["Note4"],
        },
      ],
    ]);

    const result = renderGraphTree(
      "Start",
      neighborhood,
      new Set(),
      () => undefined
    );

    // Child has 3 forward links + 1 backlink = 4 unexplored
    expect(result).toContain("Child [4 unexplored]");
  });

  it("should render proper tree connectors", () => {
    const neighborhood = new Map<string, NeighborhoodNode>([
      [
        "Parent1",
        {
          distance: 1,
          linkType: "forward",
          directLinks: ["Child1", "Child2"],
          backlinks: [],
        },
      ],
      [
        "Parent2",
        {
          distance: 1,
          linkType: "forward",
          directLinks: ["Child3"],
          backlinks: [],
        },
      ],
      [
        "Child1",
        {
          distance: 2,
          linkType: "forward",
          directLinks: [],
          backlinks: [],
        },
      ],
      [
        "Child2",
        {
          distance: 2,
          linkType: "forward",
          directLinks: [],
          backlinks: [],
        },
      ],
      [
        "Child3",
        {
          distance: 2,
          linkType: "forward",
          directLinks: [],
          backlinks: [],
        },
      ],
    ]);

    const result = renderGraphTree(
      "Start",
      neighborhood,
      new Set(),
      () => undefined
    );

    // First top-level node uses ├─
    expect(result).toContain("├─→ Parent1");
    // Last child of first parent uses └─
    expect(result).toMatch(/└─→ Child2/);
    // Vertical line between top-level nodes
    expect(result).toContain("│\n");
    // Last top-level node uses └─
    expect(result).toContain("└─→ Parent2");
  });

  it("should handle bidirectional links with forward precedence", () => {
    const neighborhood = new Map<string, NeighborhoodNode>([
      [
        "Parent",
        {
          distance: 1,
          linkType: "both", // Bidirectional
          directLinks: ["Child"],
          backlinks: ["Child"],
        },
      ],
      [
        "Child",
        {
          distance: 2,
          linkType: "both", // Bidirectional
          directLinks: ["Parent"],
          backlinks: ["Parent"],
        },
      ],
    ]);

    const result = renderGraphTree(
      "Start",
      neighborhood,
      new Set(),
      () => undefined
    );

    // Both should use forward arrow → (forward takes precedence)
    expect(result).toContain("└─→ Parent");
    expect(result).toContain("└─→ Child");
  });

  it("should handle empty neighborhood", () => {
    const neighborhood = new Map<string, NeighborhoodNode>();

    const result = renderGraphTree(
      "Start",
      neighborhood,
      new Set(),
      () => undefined
    );

    expect(result).toContain("Exploring from: Start");
    // Should just show the header, no tree (just header line after trimming)
    expect(result.trim().split("\n")).toHaveLength(1);
  });

  it("should handle node with no unexplored links", () => {
    const neighborhood = new Map<string, NeighborhoodNode>([
      [
        "Parent",
        {
          distance: 1,
          linkType: "forward",
          directLinks: ["Child"],
          backlinks: [],
        },
      ],
      [
        "Child",
        {
          distance: 2,
          linkType: "forward",
          directLinks: [], // No unexplored links
          backlinks: [], // No unexplored links
        },
      ],
    ]);

    const result = renderGraphTree(
      "Start",
      neighborhood,
      new Set(),
      () => undefined
    );

    // Should not show unexplored count when it's 0 (keeps output clean)
    expect(result).toContain("Child");
    expect(result).not.toContain("[0 unexplored]");
  });

  it("should alphabetically sort within link type groups", () => {
    const neighborhood = new Map<string, NeighborhoodNode>([
      [
        "Parent",
        {
          distance: 1,
          linkType: "forward",
          directLinks: ["Zebra", "Alpha", "Gamma", "Beta"],
          backlinks: [],
        },
      ],
      [
        "Zebra",
        {
          distance: 2,
          linkType: "forward",
          directLinks: [],
          backlinks: [],
        },
      ],
      [
        "Alpha",
        {
          distance: 2,
          linkType: "forward",
          directLinks: [],
          backlinks: [],
        },
      ],
      [
        "Gamma",
        {
          distance: 2,
          linkType: "forward",
          directLinks: [],
          backlinks: [],
        },
      ],
      [
        "Beta",
        {
          distance: 2,
          linkType: "forward",
          directLinks: [],
          backlinks: [],
        },
      ],
    ]);

    const result = renderGraphTree(
      "Start",
      neighborhood,
      new Set(),
      () => undefined
    );

    const lines = result.split("\n");
    const alphaIndex = lines.findIndex((line) => line.includes("Alpha"));
    const betaIndex = lines.findIndex((line) => line.includes("Beta"));
    const gammaIndex = lines.findIndex((line) => line.includes("Gamma"));
    const zebraIndex = lines.findIndex((line) => line.includes("Zebra"));

    // Should be alphabetically sorted: Alpha, Beta, Gamma, Zebra
    expect(alphaIndex).toBeLessThan(betaIndex);
    expect(betaIndex).toBeLessThan(gammaIndex);
    expect(gammaIndex).toBeLessThan(zebraIndex);
  });

  it("should handle complex multi-level tree", () => {
    const neighborhood = new Map<string, NeighborhoodNode>([
      [
        "West Coast Swing",
        {
          distance: 1,
          linkType: "forward",
          directLinks: ["Compression", "Stretch"],
          backlinks: ["Connection"],
        },
      ],
      [
        "Pattern",
        {
          distance: 1,
          linkType: "forward",
          directLinks: ["Pass"],
          backlinks: [],
        },
      ],
      [
        "2025-w14",
        {
          distance: 1,
          linkType: "backward",
          directLinks: [],
          backlinks: ["Anchor Step"],
        },
      ],
      [
        "Compression",
        {
          distance: 2,
          linkType: "forward",
          directLinks: ["Connection", "West Coast Swing", "Pattern"],
          backlinks: [],
        },
      ],
      [
        "Stretch",
        {
          distance: 2,
          linkType: "forward",
          directLinks: ["Connection", "West Coast Swing", "Pattern"],
          backlinks: [],
        },
      ],
      [
        "Connection",
        {
          distance: 2,
          linkType: "backward",
          directLinks: ["West Coast Swing"],
          backlinks: ["Compression", "Stretch"],
        },
      ],
      [
        "Pass",
        {
          distance: 2,
          linkType: "forward",
          directLinks: ["West Coast Swing", "Pattern"],
          backlinks: [],
        },
      ],
    ]);

    const result = renderGraphTree(
      "Anchor Step",
      neighborhood,
      new Set(),
      () => undefined
    );

    // Check structure
    expect(result).toContain("Exploring from: Anchor Step");

    // Forward links (Pattern, WCS) should come before backward links (2025-w14)
    const lines = result.split("\n");
    const patternIndex = lines.findIndex((line) => line.includes("Pattern"));
    const wcsIndex = lines.findIndex((line) =>
      line.includes("West Coast Swing")
    );
    const weekIndex = lines.findIndex((line) => line.includes("2025-w14"));

    expect(patternIndex).toBeLessThan(weekIndex);
    expect(wcsIndex).toBeLessThan(weekIndex);

    // Check that children are properly nested
    expect(result).toMatch(/├─→ Pattern\n.*└─→ Pass/s);
    expect(result).toMatch(/├─→ West Coast Swing\n.*├─→ Compression/s);
  });
});
