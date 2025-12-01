import pino from "pino";
import path from "path";
import { fileURLToPath } from "url";

const __dirname = path.dirname(fileURLToPath(import.meta.url));

// File-based debug logging
// Location: packages/mcp-server/debug.log (in project directory to avoid permission prompts)
const DEBUG_LOG = path.join(__dirname, "../..", "debug.log");

/**
 * Pino logger configured to write to both stderr and debug.log file
 */
export const logger = pino(
  {
    level: "info",
    timestamp: () => `,"time":"${new Date().toISOString()}"`,
    formatters: {
      level: (label) => {
        return { level: label };
      },
    },
  },
  pino.multistream([
    // Write to stderr for MCP protocol
    { stream: process.stderr },
    // Write to debug.log file
    { stream: pino.destination({ dest: DEBUG_LOG, sync: false }) },
  ])
);
