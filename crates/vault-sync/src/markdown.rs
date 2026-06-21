//! Markdown parsing and serialization with frontmatter support.
//!
//! Handles the conversion between raw markdown files and structured data
//! (frontmatter YAML + body text).

use std::collections::BTreeMap;
use std::collections::HashMap;

/// Parsed markdown document
#[derive(Debug, Clone)]
pub struct ParsedMarkdown {
    /// Frontmatter as key-value pairs (None if no frontmatter)
    pub frontmatter: Option<HashMap<String, serde_yaml::Value>>,
    /// Markdown body (everything after frontmatter)
    pub body: String,
}

/// Parse a markdown file into frontmatter and body.
///
/// Frontmatter must be delimited by `---` at the start of the file:
/// ```markdown
/// ---
/// title: My Note
/// tags: [a, b, c]
/// ---
///
/// # Content here
/// ```
pub fn parse(content: &str) -> ParsedMarkdown {
    // Check for frontmatter delimiter
    if !content.starts_with("---") {
        return ParsedMarkdown {
            frontmatter: None,
            body: content.to_string(),
        };
    }

    // Find the closing delimiter
    let rest = &content[3..];
    let closing = rest.find("\n---");

    match closing {
        Some(pos) => {
            let yaml_content = &rest[..pos].trim();
            let body_start = pos + 4; // Skip "\n---"

            // Skip any leading newlines after frontmatter
            let body = rest[body_start..].trim_start_matches('\n').to_string();

            // Parse YAML frontmatter
            let frontmatter =
                match serde_yaml::from_str::<HashMap<String, serde_yaml::Value>>(yaml_content) {
                    Ok(fm) if !fm.is_empty() => Some(fm),
                    Ok(_) => None,  // Empty frontmatter
                    Err(_) => None, // Invalid YAML, treat as no frontmatter
                };

            ParsedMarkdown { frontmatter, body }
        }
        None => {
            // No closing delimiter, treat entire content as body
            ParsedMarkdown {
                frontmatter: None,
                body: content.to_string(),
            }
        }
    }
}

/// Serialize frontmatter and body back to markdown.
///
/// Frontmatter keys are emitted in a deterministic, content-derived order (sorted
/// lexicographically). This is load-bearing for convergence (INV-4): the frontmatter
/// arrives as a `HashMap`, whose iteration order is process-randomized, and
/// `serde_yaml` does NOT sort keys. Without an explicit sort, two replicas that
/// applied the same frontmatter edits in different orders would emit different field
/// orderings and spuriously diverge on byte-identical content. Sorting into a
/// `BTreeMap` before serializing makes the materialized markdown a pure function of
/// the document's logical state, independent of how the map happened to be built.
pub fn serialize(frontmatter: Option<&HashMap<String, serde_yaml::Value>>, body: &str) -> String {
    match frontmatter {
        Some(fm) if !fm.is_empty() => {
            let ordered: BTreeMap<&String, &serde_yaml::Value> = fm.iter().collect();
            let yaml = serde_yaml::to_string(&ordered).unwrap_or_default();
            format!("---\n{}---\n\n{}", yaml, body)
        }
        _ => body.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_with_frontmatter() {
        let content = r#"---
title: Test Note
tags:
  - rust
  - crdt
---

# Hello World

This is the body."#;

        let parsed = parse(content);
        assert!(parsed.frontmatter.is_some());
        let fm = parsed.frontmatter.unwrap();
        assert_eq!(
            fm.get("title").unwrap(),
            &serde_yaml::Value::String("Test Note".to_string())
        );
        assert!(parsed.body.starts_with("# Hello World"));
    }

    #[test]
    fn test_parse_without_frontmatter() {
        let content = "# Just a heading\n\nSome content.";
        let parsed = parse(content);
        assert!(parsed.frontmatter.is_none());
        assert_eq!(parsed.body, content);
    }

    #[test]
    fn test_roundtrip() {
        let mut fm = HashMap::new();
        fm.insert(
            "title".to_string(),
            serde_yaml::Value::String("My Note".to_string()),
        );
        let body = "# Content\n\nParagraph.";

        let serialized = serialize(Some(&fm), body);
        let parsed = parse(&serialized);

        assert!(parsed.frontmatter.is_some());
        assert_eq!(
            parsed.frontmatter.as_ref().unwrap().get("title"),
            fm.get("title")
        );
        assert_eq!(parsed.body, body);
    }

    /// `serialize` must emit frontmatter keys in a fixed (sorted) order regardless
    /// of the order in which the source `HashMap` is populated. This is the
    /// unit-level guard for the INV-4 frontmatter-ordering fix; the document-level
    /// regression test lives in `content_doc.rs` (AC-INV-4-DET).
    #[test]
    fn test_serialize_field_order_is_deterministic() {
        // Two maps with the same entries inserted in opposite orders.
        let mut fm_forward = HashMap::new();
        fm_forward.insert("alpha".to_string(), serde_yaml::Value::String("1".into()));
        fm_forward.insert("beta".to_string(), serde_yaml::Value::String("2".into()));
        fm_forward.insert("gamma".to_string(), serde_yaml::Value::String("3".into()));

        let mut fm_reverse = HashMap::new();
        fm_reverse.insert("gamma".to_string(), serde_yaml::Value::String("3".into()));
        fm_reverse.insert("beta".to_string(), serde_yaml::Value::String("2".into()));
        fm_reverse.insert("alpha".to_string(), serde_yaml::Value::String("1".into()));

        let body = "# Body";
        let a = serialize(Some(&fm_forward), body);
        let b = serialize(Some(&fm_reverse), body);

        assert_eq!(a, b, "serialized output must not depend on map build order");
        // And the order is the sorted order, not insertion order.
        let alpha_pos = a.find("alpha").unwrap();
        let beta_pos = a.find("beta").unwrap();
        let gamma_pos = a.find("gamma").unwrap();
        assert!(
            alpha_pos < beta_pos && beta_pos < gamma_pos,
            "frontmatter keys should be emitted in sorted order"
        );
    }
}
