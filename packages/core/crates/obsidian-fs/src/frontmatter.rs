//! YAML frontmatter parsing for Obsidian notes
//!
//! Parses the YAML frontmatter block at the start of markdown files:
//! ```markdown
//! ---
//! title: My Note
//! tags: [rust, wasm]
//! ---
//!
//! Note content here...
//! ```

use serde_json::Value as JsonValue;
use std::collections::HashMap;

/// Parsed frontmatter as a map of string keys to JSON values.
/// Using JSON values allows flexible typing (strings, numbers, arrays, objects).
pub type Frontmatter = HashMap<String, JsonValue>;

/// A parsed note with frontmatter separated from content.
/// 
/// The `content` field borrows from `raw` to avoid unnecessary allocation.
#[derive(Debug, Clone, PartialEq)]
pub struct ParsedNote<'a> {
    /// The frontmatter key-value pairs, if present
    pub frontmatter: Option<Frontmatter>,
    /// The note content after the frontmatter (borrows from raw)
    pub content: &'a str,
    /// The raw file content (frontmatter + content)
    pub raw: &'a str,
}

/// Split a note into frontmatter YAML string and content, without parsing the YAML.
///
/// Returns (frontmatter_yaml, content) where frontmatter_yaml is None if
/// no valid frontmatter block was found.
pub fn split_frontmatter(raw: &str) -> (Option<&str>, &str) {
    // Frontmatter must start at the very beginning with ---
    if !raw.starts_with("---") {
        return (None, raw);
    }

    // Find the closing ---
    let after_opening = &raw[3..];

    // Skip the newline after opening ---
    let content_start = if after_opening.starts_with('\n') {
        &after_opening[1..]
    } else if after_opening.starts_with("\r\n") {
        &after_opening[2..]
    } else {
        // No newline after opening --- means invalid frontmatter
        return (None, raw);
    };

    // Find closing --- (must be at start of line)
    if let Some(close_pos) = find_closing_delimiter(content_start) {
        let yaml = &content_start[..close_pos];
        let after_close = &content_start[close_pos + 3..];

        // Skip newline after closing ---
        let content = if after_close.starts_with('\n') {
            &after_close[1..]
        } else if after_close.starts_with("\r\n") {
            &after_close[2..]
        } else {
            after_close
        };

        (Some(yaml), content)
    } else {
        // No closing delimiter found
        (None, raw)
    }
}

/// Find the position of the closing --- delimiter (must be at start of line)
fn find_closing_delimiter(s: &str) -> Option<usize> {
    let mut pos = 0;
    for line in s.lines() {
        if line == "---" || line == "---\r" {
            return Some(pos);
        }
        pos += line.len() + 1; // +1 for newline
    }
    None
}

/// Parse a note's raw content into frontmatter and content.
///
/// The frontmatter is parsed as YAML and converted to a HashMap with JSON values.
/// The returned `ParsedNote` borrows from the input string.
pub fn parse_frontmatter(raw: &str) -> ParsedNote<'_> {
    let (yaml_str, content) = split_frontmatter(raw);

    let frontmatter = yaml_str.and_then(|yaml| {
        // Parse YAML to serde_yaml::Value, then convert to JSON Value
        serde_yaml::from_str::<serde_yaml::Value>(yaml)
            .ok()
            .and_then(yaml_to_json_map)
    });

    ParsedNote {
        frontmatter,
        content,
        raw,
    }
}

/// Convert a YAML value to a JSON HashMap (for the top-level frontmatter)
fn yaml_to_json_map(yaml: serde_yaml::Value) -> Option<Frontmatter> {
    match yaml {
        serde_yaml::Value::Mapping(map) => {
            let mut result = HashMap::new();
            for (k, v) in map {
                if let serde_yaml::Value::String(key) = k {
                    result.insert(key, yaml_to_json(v));
                }
            }
            if result.is_empty() {
                None
            } else {
                Some(result)
            }
        }
        _ => None,
    }
}

/// Convert a YAML value to a JSON value
fn yaml_to_json(yaml: serde_yaml::Value) -> JsonValue {
    match yaml {
        serde_yaml::Value::Null => JsonValue::Null,
        serde_yaml::Value::Bool(b) => JsonValue::Bool(b),
        serde_yaml::Value::Number(n) => {
            if let Some(i) = n.as_i64() {
                JsonValue::Number(i.into())
            } else if let Some(f) = n.as_f64() {
                serde_json::Number::from_f64(f)
                    .map(JsonValue::Number)
                    .unwrap_or(JsonValue::Null)
            } else {
                JsonValue::Null
            }
        }
        serde_yaml::Value::String(s) => JsonValue::String(s),
        serde_yaml::Value::Sequence(seq) => {
            JsonValue::Array(seq.into_iter().map(yaml_to_json).collect())
        }
        serde_yaml::Value::Mapping(map) => {
            let obj: serde_json::Map<String, JsonValue> = map
                .into_iter()
                .filter_map(|(k, v)| {
                    if let serde_yaml::Value::String(key) = k {
                        Some((key, yaml_to_json(v)))
                    } else {
                        None
                    }
                })
                .collect();
            JsonValue::Object(obj)
        }
        serde_yaml::Value::Tagged(tagged) => yaml_to_json(tagged.value),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn split_note_with_frontmatter() {
        let raw = "---\ntitle: Test\n---\n\nContent here";
        let (yaml, content) = split_frontmatter(raw);
        assert_eq!(yaml, Some("title: Test\n"));
        assert_eq!(content, "\nContent here");
    }

    #[test]
    fn split_note_without_frontmatter() {
        let raw = "Just content, no frontmatter";
        let (yaml, content) = split_frontmatter(raw);
        assert!(yaml.is_none());
        assert_eq!(content, raw);
    }

    #[test]
    fn split_note_with_incomplete_frontmatter() {
        let raw = "---\ntitle: Test\nNo closing delimiter";
        let (yaml, content) = split_frontmatter(raw);
        assert!(yaml.is_none());
        assert_eq!(content, raw);
    }

    #[test]
    fn parse_simple_frontmatter() {
        let raw = "---\ntitle: My Note\ntags:\n  - rust\n  - wasm\n---\n\nNote content";
        let parsed = parse_frontmatter(raw);

        assert!(parsed.frontmatter.is_some());
        let fm = parsed.frontmatter.unwrap();
        assert_eq!(
            fm.get("title"),
            Some(&JsonValue::String("My Note".to_string()))
        );

        let tags = fm.get("tags").unwrap();
        assert!(tags.is_array());
        assert_eq!(tags.as_array().unwrap().len(), 2);

        assert_eq!(parsed.content, "\nNote content");
    }

    #[test]
    fn parse_frontmatter_with_numbers() {
        let raw = "---\nversion: 42\nprice: 19.99\n---\nContent";
        let parsed = parse_frontmatter(raw);

        let fm = parsed.frontmatter.unwrap();
        assert_eq!(fm.get("version"), Some(&JsonValue::Number(42.into())));
    }

    #[test]
    fn parse_frontmatter_with_booleans() {
        let raw = "---\ndraft: true\npublished: false\n---\nContent";
        let parsed = parse_frontmatter(raw);

        let fm = parsed.frontmatter.unwrap();
        assert_eq!(fm.get("draft"), Some(&JsonValue::Bool(true)));
        assert_eq!(fm.get("published"), Some(&JsonValue::Bool(false)));
    }

    #[test]
    fn parse_frontmatter_with_nested_objects() {
        let raw = "---\nauthor:\n  name: Alice\n  email: alice@example.com\n---\nContent";
        let parsed = parse_frontmatter(raw);

        let fm = parsed.frontmatter.unwrap();
        let author = fm.get("author").unwrap();
        assert!(author.is_object());
        assert_eq!(
            author.get("name"),
            Some(&JsonValue::String("Alice".to_string()))
        );
    }

    #[test]
    fn parse_empty_frontmatter() {
        let raw = "---\n---\nContent";
        let parsed = parse_frontmatter(raw);

        // Empty frontmatter should result in None
        assert!(parsed.frontmatter.is_none());
        assert_eq!(parsed.content, "Content");
    }

    #[test]
    fn parse_no_frontmatter() {
        let raw = "Just regular content";
        let parsed = parse_frontmatter(raw);

        assert!(parsed.frontmatter.is_none());
        assert_eq!(parsed.content, "Just regular content");
    }

    #[test]
    fn preserves_raw_content() {
        let raw = "---\ntitle: Test\n---\nContent";
        let parsed = parse_frontmatter(raw);
        assert_eq!(parsed.raw, raw);
    }
}
