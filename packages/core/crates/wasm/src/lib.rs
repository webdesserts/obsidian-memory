//! WASM entry point for obsidian-memory-core
//!
//! This crate serves as the single WASM bundle that re-exports functionality from
//! the internal crates (wiki-links, obsidian-fs, semantic-embeddings).
//!
//! The architecture keeps parsing/utility logic in pure Rust crates that can be
//! tested natively, while this crate handles WASM bindings and JS interop.

use wasm_bindgen::prelude::*;

/// Initialize panic hook for better error messages in browser console.
/// Call this once at startup.
#[wasm_bindgen(start)]
pub fn init() {
    console_error_panic_hook::set_once();
}

/// Placeholder function to verify WASM is working.
/// Remove once real functionality is added.
#[wasm_bindgen]
pub fn ping() -> String {
    "pong from obsidian-memory-core".to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_ping() {
        assert_eq!(ping(), "pong from obsidian-memory-core");
    }
}
