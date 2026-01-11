/**
 * Log levels in order of verbosity.
 * Each level includes all less verbose levels (e.g., "log" includes "warn" and "error").
 */
export type LogLevel = "debug" | "log" | "warn" | "error" | "none";

const LEVEL_PRIORITY: Record<LogLevel, number> = {
  debug: 0,
  log: 1,
  warn: 2,
  error: 3,
  none: 4,
};

/** Configuration for the logger */
interface LoggerConfig {
  /** Minimum level to output. Defaults to "log" (includes log, warn, error). */
  level: LogLevel;
  /** Prefix for all messages. Defaults to "p2p-sync:". */
  prefix: string;
}

const defaultConfig: LoggerConfig = {
  level: "log",
  prefix: "p2p-sync:",
};

let config: LoggerConfig = { ...defaultConfig };

/**
 * Configure the logger.
 * 
 * @example
 * // Enable debug logging
 * configureLogger({ level: "debug" });
 * 
 * // Disable all logging
 * configureLogger({ level: "none" });
 */
export function configureLogger(options: Partial<LoggerConfig>): void {
  config = { ...config, ...options };
}

/** Get the current logger configuration */
export function getLoggerConfig(): Readonly<LoggerConfig> {
  return config;
}

/** Reset logger to default configuration */
export function resetLoggerConfig(): void {
  config = { ...defaultConfig };
}

function shouldLog(level: LogLevel): boolean {
  return LEVEL_PRIORITY[level] >= LEVEL_PRIORITY[config.level];
}

function formatMessage(message: string): string {
  return `${config.prefix}${config.prefix.endsWith(":") ? " " : ""}${message}`;
}

/**
 * Logger with configurable levels and consistent formatting.
 * 
 * All methods mirror console.* but add the "p2p-sync:" prefix
 * and respect the configured log level.
 */
export const log = {
  /** Debug-level logging. Only shown when level is "debug". */
  debug(message: string, ...args: unknown[]): void {
    if (shouldLog("debug")) {
      console.log(formatMessage(message), ...args);
    }
  },

  /** Info-level logging. Shown when level is "log" or lower. */
  info(message: string, ...args: unknown[]): void {
    if (shouldLog("log")) {
      console.log(formatMessage(message), ...args);
    }
  },

  /** Warning-level logging. Shown when level is "warn" or lower. */
  warn(message: string, ...args: unknown[]): void {
    if (shouldLog("warn")) {
      console.warn(formatMessage(message), ...args);
    }
  },

  /** Error-level logging. Always shown unless level is "none". */
  error(message: string, ...args: unknown[]): void {
    if (shouldLog("error")) {
      console.error(formatMessage(message), ...args);
    }
  },
};
