import fs from "fs/promises";
import path from "path";
import { FileOperations } from "../tools/file-operations.js";

/**
 * Access log entry for note usage tracking
 */
interface AccessLogEntry {
  timestamp: Date;
  note: string;
  context?: string;
}

/**
 * Note usage statistics
 */
export interface NoteUsageStats {
  accessCount24h: number;
  accessCount7d: number;
  accessCount30d: number;
  lastAccessed: Date | null;
  backlinks: number;
  createdDate: Date | null;
}

/**
 * Memory system managing Index.md, WorkingMemory.md, and consolidation
 */
export class MemorySystem {
  private accessLog: AccessLogEntry[] = [];
  private indexContent: string | null = null;
  private workingMemoryContent: string | null = null;
  private consolidationInProgress = false;

  constructor(
    private vaultPath: string,
    private fileOps: FileOperations
  ) {}

  /**
   * Initialize memory system - load Index.md if it exists
   */
  async initialize(): Promise<void> {
    console.error("[MemorySystem] Initializing...");

    // Try to load Index.md
    try {
      const result = await this.fileOps.readNote("Index.md");
      this.indexContent = result.content;
      console.error("[MemorySystem] Loaded Index.md");
    } catch (error) {
      console.error("[MemorySystem] No Index.md found (will be created on first consolidation)");
    }

    // Try to load WorkingMemory.md
    try {
      const result = await this.fileOps.readNote("WorkingMemory.md");
      this.workingMemoryContent = result.content;
      console.error("[MemorySystem] Loaded WorkingMemory.md");
    } catch (error) {
      console.error("[MemorySystem] No WorkingMemory.md found (will be created when needed)");
    }
  }

  /**
   * Get Index.md content (long-term memory)
   */
  getIndex(): string | null {
    return this.indexContent;
  }

  /**
   * Get WorkingMemory.md content
   */
  getWorkingMemory(): string | null {
    return this.workingMemoryContent;
  }

  /**
   * Update WorkingMemory.md in memory (called after writes)
   */
  async refreshWorkingMemory(): Promise<void> {
    try {
      const result = await this.fileOps.readNote("WorkingMemory.md");
      this.workingMemoryContent = result.content;
    } catch (error) {
      this.workingMemoryContent = null;
    }
  }

  /**
   * Log note access for usage statistics
   */
  logAccess(noteName: string, context?: string): void {
    this.accessLog.push({
      timestamp: new Date(),
      note: noteName,
      context,
    });

    // Keep only last 30 days of logs
    const thirtyDaysAgo = new Date();
    thirtyDaysAgo.setDate(thirtyDaysAgo.getDate() - 30);

    this.accessLog = this.accessLog.filter(
      (entry) => entry.timestamp >= thirtyDaysAgo
    );
  }

  /**
   * Get usage statistics for notes
   */
  async getNoteUsage(
    notes: string[],
    period: "24h" | "7d" | "30d" | "all" = "all"
  ): Promise<Record<string, NoteUsageStats>> {
    const now = new Date();
    const cutoffTimes = {
      "24h": new Date(now.getTime() - 24 * 60 * 60 * 1000),
      "7d": new Date(now.getTime() - 7 * 24 * 60 * 60 * 1000),
      "30d": new Date(now.getTime() - 30 * 24 * 60 * 60 * 1000),
      all: new Date(0),
    };

    const cutoff = cutoffTimes[period];
    const stats: Record<string, NoteUsageStats> = {};

    for (const note of notes) {
      const accesses = this.accessLog.filter((entry) => entry.note === note);

      const accessCount24h = accesses.filter(
        (e) => e.timestamp >= cutoffTimes["24h"]
      ).length;
      const accessCount7d = accesses.filter(
        (e) => e.timestamp >= cutoffTimes["7d"]
      ).length;
      const accessCount30d = accesses.filter(
        (e) => e.timestamp >= cutoffTimes["30d"]
      ).length;

      const lastAccessed =
        accesses.length > 0
          ? accesses.reduce((latest, entry) =>
              entry.timestamp > latest ? entry.timestamp : latest
            , accesses[0].timestamp)
          : null;

      // Get file creation date
      let createdDate: Date | null = null;
      try {
        const notePath = path.join(this.vaultPath, `${note}.md`);
        const stats = await fs.stat(notePath);
        createdDate = stats.birthtime;
      } catch (error) {
        // File doesn't exist or error reading stats
      }

      stats[note] = {
        accessCount24h,
        accessCount7d,
        accessCount30d,
        lastAccessed,
        backlinks: 0, // Will be filled in by graph index
        createdDate,
      };
    }

    return stats;
  }

  /**
   * Load private memory indexes (requires explicit consent)
   */
  async loadPrivateMemory(): Promise<{
    longTermIndex: string | null;
    workingMemory: string | null;
  }> {
    let longTermIndex: string | null = null;
    let workingMemory: string | null = null;

    try {
      const result = await this.fileOps.readNote("private/Index.md");
      longTermIndex = result.content;
    } catch (error) {
      console.error("[MemorySystem] No private/Index.md found");
    }

    try {
      const result = await this.fileOps.readNote("private/WorkingMemory.md");
      workingMemory = result.content;
    } catch (error) {
      console.error("[MemorySystem] No private/WorkingMemory.md found");
    }

    return { longTermIndex, workingMemory };
  }

  /**
   * Check if consolidation is in progress
   */
  isConsolidating(): boolean {
    return this.consolidationInProgress;
  }

  /**
   * Check if consolidation is needed
   */
  async shouldConsolidate(): Promise<boolean> {
    try {
      const frontmatter = await this.fileOps.getFrontmatter("Index.md");
      if (!frontmatter || !frontmatter.lastConsolidation) {
        return true; // Never consolidated
      }

      const lastConsolidation = new Date(frontmatter.lastConsolidation);
      const deadline = this.getLastDeadline();

      return lastConsolidation < deadline;
    } catch (error) {
      return true; // No Index.md, needs consolidation
    }
  }

  /**
   * Get the last 3am deadline
   */
  private getLastDeadline(): Date {
    const now = new Date();
    const deadline = new Date();
    deadline.setHours(3, 0, 0, 0);

    // If it's before 3am today, use yesterday's 3am
    if (now.getHours() < 3) {
      deadline.setDate(deadline.getDate() - 1);
    }

    return deadline;
  }

  /**
   * Try to acquire consolidation lock
   */
  async tryAcquireConsolidationLock(): Promise<boolean> {
    const lockPath = path.join(this.vaultPath, ".obsidian/consolidation.lock");

    try {
      // Try to read existing lock
      const lockContent = await fs.readFile(lockPath, "utf-8");
      const lock = JSON.parse(lockContent);

      // Check if lock is stale (TTL expired)
      const now = Date.now();
      if (now - lock.timestamp > lock.ttl) {
        // Stale lock, claim it
        await this.writeLock(lockPath);
        return true;
      }

      // Valid lock held by another process
      return false;
    } catch (error) {
      // No lock file exists, create one
      await this.writeLock(lockPath);
      return true;
    }
  }

  /**
   * Write lock file
   */
  private async writeLock(lockPath: string): Promise<void> {
    const lock = {
      laptop: process.env.HOSTNAME || "unknown",
      timestamp: Date.now(),
      ttl: 600000, // 10 minutes
    };

    // Ensure .obsidian directory exists
    const obsidianDir = path.join(this.vaultPath, ".obsidian");
    await fs.mkdir(obsidianDir, { recursive: true });

    await fs.writeFile(lockPath, JSON.stringify(lock, null, 2), "utf-8");
  }

  /**
   * Release consolidation lock
   */
  async releaseConsolidationLock(): Promise<void> {
    const lockPath = path.join(this.vaultPath, ".obsidian/consolidation.lock");

    try {
      await fs.unlink(lockPath);
    } catch (error) {
      // Lock file doesn't exist or already deleted
    }
  }

  /**
   * Mark consolidation as in progress
   */
  startConsolidation(): void {
    this.consolidationInProgress = true;
  }

  /**
   * Mark consolidation as complete
   */
  endConsolidation(): void {
    this.consolidationInProgress = false;
  }

  /**
   * Reload Index.md after consolidation
   */
  async reloadIndex(): Promise<void> {
    try {
      const result = await this.fileOps.readNote("Index.md");
      this.indexContent = result.content;
      console.error("[MemorySystem] Reloaded Index.md after consolidation");
    } catch (error) {
      console.error("[MemorySystem] Failed to reload Index.md");
    }
  }
}
