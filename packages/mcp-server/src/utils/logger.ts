import { appendFileSync } from "fs";
import path from "path";
import { fileURLToPath } from "url";

const __dirname = path.dirname(fileURLToPath(import.meta.url));

// File-based debug logging
// Location: packages/mcp-server/debug.log (in project directory to avoid permission prompts)
// This file captures uncaught exceptions and unhandled promise rejections for troubleshooting
const DEBUG_LOG = path.join(__dirname, "../..", "debug.log");

/**
 * Log a debug message to both the debug log file and stderr
 *
 * @param message - The message to log
 */
export function debugLog(message: string) {
  const timestamp = new Date().toISOString();
  appendFileSync(DEBUG_LOG, `[${timestamp}] ${message}\n`);
  console.error(message);
}
