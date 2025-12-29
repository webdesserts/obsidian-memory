//! WASM entry point for obsidian-memory-core
//!
//! This crate serves as the single WASM bundle that re-exports functionality from
//! the internal crates (wiki-links, obsidian-fs, semantic-embeddings).
//!
//! The architecture keeps parsing/utility logic in pure Rust crates that can be
//! tested natively, while this crate handles WASM bindings and JS interop.

use serde::Serialize;
use wasm_bindgen::prelude::*;
use wiki_links::WikiLink;

/// Initialize panic hook for better error messages in browser console.
/// Call this once at startup.
#[wasm_bindgen(start)]
pub fn init() {
    console_error_panic_hook::set_once();
}

/// JS-compatible representation matching the original TypeScript WikiLink interface.
/// Temporary compatibility layer until we have a pure Rust MCP server.
#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct WikiLinkJs {
    /// The target note name (without path or .md extension)
    target: String,
    /// The full target as written (path + fragment, before the |)
    full_target: String,
    /// Optional display alias
    #[serde(skip_serializing_if = "Option::is_none")]
    alias: Option<String>,
    /// Whether this is an embed (![[...]])
    is_embed: bool,
    /// Header reference if present
    #[serde(skip_serializing_if = "Option::is_none")]
    header: Option<String>,
    /// Block ID if present
    #[serde(skip_serializing_if = "Option::is_none")]
    block_id: Option<String>,
}

impl From<&WikiLink> for WikiLinkJs {
    fn from(link: &WikiLink) -> Self {
        // Build full_target: path + fragment (header or block_id)
        let path = link.path();
        let full_target = match (&link.header, &link.block_id) {
            (Some(h), _) => format!("{}#{}", path, h),
            (_, Some(b)) => format!("{}#^{}", path, b),
            _ => path,
        };

        // target matches original TS behavior: name + extension, but .md is stripped
        // e.g., "CLAUDE.local" -> "CLAUDE.local", "Note.md" -> "Note"
        let target = match &link.extension {
            Some(ext) if ext == "md" => link.name.clone(),
            Some(ext) => format!("{}.{}", link.name, ext),
            None => link.name.clone(),
        };

        WikiLinkJs {
            target,
            full_target,
            alias: link.alias.clone(),
            is_embed: link.is_embed,
            header: link.header.clone(),
            block_id: link.block_id.clone(),
        }
    }
}

/// Parse all wiki links from markdown content.
///
/// Returns a JS array of WikiLink objects with computed fields included.
#[wasm_bindgen(js_name = parseWikiLinks)]
pub fn parse_wiki_links(content: &str) -> Result<JsValue, JsError> {
    let links: Vec<WikiLinkJs> = wiki_links::parse_wiki_links(content)
        .iter()
        .map(WikiLinkJs::from)
        .collect();
    serde_wasm_bindgen::to_value(&links).map_err(|e| JsError::new(&e.to_string()))
}

/// Extract all unique note names from wiki links in content.
///
/// Returns a JS array of strings.
#[wasm_bindgen(js_name = extractLinkedNotes)]
pub fn extract_linked_notes(content: &str) -> Result<JsValue, JsError> {
    let notes = wiki_links::extract_linked_notes(content);
    serde_wasm_bindgen::to_value(&notes).map_err(|e| JsError::new(&e.to_string()))
}

#[cfg(test)]
mod tests {
    #[test]
    fn test_parse_wiki_links() {
        let links = wiki_links::parse_wiki_links("[[Test]]");
        assert_eq!(links.len(), 1);
        assert_eq!(links[0].name, "Test");
    }

    #[test]
    fn test_extract_linked_notes() {
        let notes = wiki_links::extract_linked_notes("[[A]] and [[B]]");
        assert_eq!(notes.len(), 2);
    }
}
