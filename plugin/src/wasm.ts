/**
 * WASM module loader and typed wrappers.
 *
 * Provides a clean TypeScript API for interacting with the Rust sync-core
 * library compiled to WASM.
 */

import {
  initSync,
  init as wasmInit,
  health_check,
  version,
  generatePeerId as wasmGeneratePeerId,
  JsFileSystemBridge,
  WasmVault,
} from "../pkg/sync_wasm.js";

// Import WASM binary as base64-encoded ArrayBuffer (via esbuild plugin)
import wasmBinary from "../pkg/sync_wasm_bg.wasm";

import { log } from "./logger";

// Re-export types
export { JsFileSystemBridge, WasmVault };

// ========== Debug API Types ==========

/** Version vector as a map of peer ID hex strings to counter values */
export type VersionVector = Record<string, number>;

/** Registry oplog statistics */
export interface RegistryStats {
  changeCount: number;
  opCount: number;
}

/** Cheap metadata from .loro blob header (no document load required) */
export interface BlobMeta {
  changeCount: number;
  startTimestamp: number;
  endTimestamp: number;
  mode: string;
  startVersion: VersionVector;
  endVersion: VersionVector;
}

/** Full document info (requires document load) */
export interface DocumentInfo {
  path: string;
  version: VersionVector;
  docId: string | null;
  storedPath: string | null;
  changeCount: number;
  opCount: number;
  bodyLength: number;
  hasFrontmatter: boolean;
}

/**
 * Check if a document's current version includes all operations from a synced version.
 *
 * Returns true if `current_version` contains all operations from `synced_version`.
 * Use this to detect if a file modification event is purely from sync
 * (should be skipped to prevent re-broadcast) or includes local edits.
 */
export function versionIncludes(currentVersion: Uint8Array, syncedVersion: Uint8Array): boolean {
  if (!initialized) {
    throw new Error("WASM not initialized. Call initWasm() first.");
  }
  return WasmVault.versionIncludes(currentVersion, syncedVersion);
}

let initialized = false;

/**
 * Initialize the WASM module.
 *
 * Must be called before using any other WASM functions.
 * Safe to call multiple times (subsequent calls are no-ops).
 */
export async function initWasm(): Promise<void> {
  if (initialized) {
    return;
  }

  // Initialize WASM synchronously using the bundled binary
  initSync(wasmBinary);

  // Initialize panic hook and logging
  wasmInit();

  initialized = true;
  log.info(`sync-wasm v${version()} initialized`);
}

/**
 * Check if WASM is ready for use.
 */
export function isWasmReady(): boolean {
  return initialized;
}

/**
 * Verify WASM is working correctly.
 *
 * @returns 42 if working
 */
export function wasmHealthCheck(): number {
  if (!initialized) {
    throw new Error("WASM not initialized. Call initWasm() first.");
  }
  return health_check();
}

/**
 * Get the WASM module version.
 */
export function wasmVersion(): string {
  if (!initialized) {
    throw new Error("WASM not initialized. Call initWasm() first.");
  }
  return version();
}

/**
 * Generate a random peer ID.
 *
 * Returns a 16-character hex string that uniquely identifies this peer.
 * Existing vaults with legacy UUID peer IDs will continue to work.
 */
export function generatePeerId(): string {
  if (!initialized) {
    throw new Error("WASM not initialized. Call initWasm() first.");
  }
  return wasmGeneratePeerId();
}
