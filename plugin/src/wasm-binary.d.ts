/**
 * Type declaration for WASM binary imports.
 *
 * The esbuild wasmPlugin loads .wasm files as base64-encoded ArrayBuffers.
 */
declare module "*.wasm" {
  const content: ArrayBuffer;
  export default content;
}
