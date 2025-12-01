import fs from "fs/promises";
import path from "path";
import { FileOperations } from "../file-operations.js";
import { logger } from "../utils/logger.js";

/**
 * Memory system managing Working Memory.md and consolidation
 */
export class MemorySystem {
  private workingMemoryContent: string | null = null;

  constructor(
    private vaultPath: string,
    private fileOps: FileOperations
  ) {}

  /**
   * Initialize memory system - load Working Memory.md if it exists
   */
  async initialize(): Promise<void> {
    logger.info({ group: "MemorySystem" }, "Initializing");

    // Try to load Working Memory.md
    try {
      const result = await this.fileOps.readNote("Working Memory.md");
      this.workingMemoryContent = result.content;
      logger.info({ group: "MemorySystem" }, "Loaded Working Memory.md");
    } catch (error) {
      logger.info({ group: "MemorySystem" }, "No Working Memory.md found (will be created when needed)");
    }
  }

  /**
   * Get Working Memory.md content
   */
  getWorkingMemory(): string | null {
    return this.workingMemoryContent;
  }

  /**
   * Update Working Memory.md in memory (called after writes)
   */
  async refreshWorkingMemory(): Promise<void> {
    try {
      const result = await this.fileOps.readNote("Working Memory.md");
      this.workingMemoryContent = result.content;
    } catch (error) {
      this.workingMemoryContent = null;
    }
  }

  /**
   * Load private memory (requires explicit consent)
   */
  async loadPrivateMemory(): Promise<{
    workingMemory: string | null;
  }> {
    let workingMemory: string | null = null;

    try {
      const result = await this.fileOps.readNote("private/Working Memory.md");
      workingMemory = result.content;
    } catch (error) {
      logger.info({ group: "MemorySystem" }, "No private/Working Memory.md found");
    }

    return { workingMemory };
  }
}
