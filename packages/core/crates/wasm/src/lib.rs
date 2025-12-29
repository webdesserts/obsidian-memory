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
use obsidian_fs::{NoteRef, ResolutionOptions};

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

// =============================================================================
// obsidian-fs bindings
// =============================================================================

/// JS-compatible representation of NoteRef
#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct NoteRefJs {
    /// The path without extension: "knowledge/Note"
    path: String,
    /// Just the note name: "Note"
    name: String,
}

impl From<NoteRef> for NoteRefJs {
    fn from(note_ref: NoteRef) -> Self {
        NoteRefJs {
            path: note_ref.path,
            name: note_ref.name,
        }
    }
}

/// Normalize a note reference (strip memory: URI scheme, [[brackets]], and .md extension)
///
/// Accepts:
/// - Plain name: "Note"
/// - Path: "knowledge/Note"
/// - Memory URI: "memory:knowledge/Note"
/// - Wiki link: "[[knowledge/Note]]"
/// - With .md: "knowledge/Note.md"
///
/// Returns an object with `path` and `name` properties.
#[wasm_bindgen(js_name = normalizeNoteReference)]
pub fn normalize_note_reference(note_ref: &str) -> Result<JsValue, JsError> {
    let result: NoteRefJs = obsidian_fs::normalize_note_reference(note_ref).into();
    serde_wasm_bindgen::to_value(&result).map_err(|e| JsError::new(&e.to_string()))
}

/// Extract note name from a path (last component without extension)
///
/// @param notePath - Path to the note (can be memory: URI, [[wiki link]], or plain path)
/// @returns Just the note name
#[wasm_bindgen(js_name = extractNoteName)]
pub fn extract_note_name(note_path: &str) -> String {
    obsidian_fs::normalize_note_reference(note_path).name
}

/// Generate search paths for a note name.
///
/// Returns an array of paths to try (without .md extension).
/// Paths include: root, knowledge/, journal/, projects/, and optionally private/
#[wasm_bindgen(js_name = generateSearchPaths)]
pub fn generate_search_paths(note_name: &str, include_private: bool) -> Result<JsValue, JsError> {
    let paths = obsidian_fs::generate_search_paths(note_name, include_private);
    serde_wasm_bindgen::to_value(&paths).map_err(|e| JsError::new(&e.to_string()))
}

/// Resolve a note path from available options using priority order.
///
/// Priority: root → knowledge/ → journal/ → projects/ → others → private/
///
/// @param availablePaths - Array of paths to choose from
/// @param includePrivate - Whether to include private paths in resolution
/// @returns The best matching path, or undefined if no paths provided
#[wasm_bindgen(js_name = resolveNotePath)]
pub fn resolve_note_path(available_paths: Vec<String>, include_private: bool) -> Option<String> {
    if available_paths.is_empty() {
        return None;
    }
    let path_refs: Vec<&str> = available_paths.iter().map(|s| s.as_str()).collect();
    let options = ResolutionOptions { include_private };
    obsidian_fs::resolve_note_path(&path_refs, &options)
}

/// Ensure .md extension on note paths
#[wasm_bindgen(js_name = ensureMarkdownExtension)]
pub fn ensure_markdown_extension(note_path: &str) -> String {
    obsidian_fs::ensure_markdown_extension(note_path)
}

/// Validate that a relative path is safe (no directory traversal)
///
/// Returns the cleaned path (with leading slash stripped) or throws an error.
#[wasm_bindgen(js_name = validateRelativePath)]
pub fn validate_relative_path(path: &str) -> Result<String, JsError> {
    obsidian_fs::validate_relative_path(path).map_err(|e| JsError::new(&e.to_string()))
}

/// Parse frontmatter from markdown content.
///
/// Returns an object with `content` (the markdown body) and optional `frontmatter` (parsed YAML).
#[wasm_bindgen(js_name = parseFrontmatter)]
pub fn parse_frontmatter(content: &str) -> Result<JsValue, JsError> {
    let parsed = obsidian_fs::parse_frontmatter(content);
    
    #[derive(Serialize)]
    struct ParsedNoteJs {
        content: String,
        raw: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        frontmatter: Option<serde_json::Value>,
    }
    
    // Convert HashMap<String, JsonValue> to serde_json::Value::Object
    let frontmatter_value = parsed.frontmatter.map(|fm| {
        serde_json::Value::Object(fm.into_iter().collect())
    });
    
    let result = ParsedNoteJs {
        content: parsed.content.to_string(),
        raw: parsed.raw.to_string(),
        frontmatter: frontmatter_value,
    };
    
    serde_wasm_bindgen::to_value(&result).map_err(|e| JsError::new(&e.to_string()))
}

#[cfg(test)]
mod tests {
    use super::*;
    
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
    
    #[test]
    fn test_extract_note_name() {
        assert_eq!(extract_note_name("knowledge/Note"), "Note");
        assert_eq!(extract_note_name("memory:knowledge/Note"), "Note");
        assert_eq!(extract_note_name("[[Note]]"), "Note");
    }
    
    #[test]
    fn test_resolve_note_path() {
        let paths = vec!["knowledge/Note".to_string(), "Note".to_string()];
        assert_eq!(resolve_note_path(paths, false), Some("Note".to_string()));
    }
    
    #[test]
    fn test_ensure_markdown_extension() {
        assert_eq!(ensure_markdown_extension("Note"), "Note.md");
        assert_eq!(ensure_markdown_extension("Note.md"), "Note.md");
    }
}
