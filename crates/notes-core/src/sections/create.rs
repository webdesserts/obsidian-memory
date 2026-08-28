//! Section create resolution: given a note's content (possibly empty) and a
//! heading path, synthesize whatever heading chain is missing and splice a
//! new leaf section into place — the create-side counterpart to
//! [`super::write::resolve_section_for_write`].
//!
//! Matching is delegated to [`super::path::resolve_section`] for the
//! already-exists check, and to the same normalization
//! ([`super::path::normalize_segments`]/[`super::path::collapse_whitespace`])
//! for locating the deepest existing ancestor - so a heading path means the
//! same thing here as it does for reads and edits.

use super::outline::build_outline;
use super::path::{SectionResolveError, collapse_whitespace, normalize_segments, resolve_section};

/// Markdown's own heading depth limit (`######` is the deepest ATX heading).
const MAX_HEADING_LEVEL: u8 = 6;

/// Error creating a section.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SectionCreateError {
    /// The requested path already resolves to an existing section - a
    /// create would silently clobber it. Symmetric to `write_note`'s
    /// existing "Note already exists" message.
    AlreadyExists { path: String },
    /// The requested path itself (not an ancestor prefix) ambiguously
    /// matches 2+ existing sections, so "does this already exist?" can't be
    /// answered safely.
    Ambiguous {
        path: String,
        candidates: Vec<String>,
    },
    /// Creating every missing ancestor would need a heading nested past
    /// level 6, markdown's own depth limit.
    TooDeep { path: String },
}

/// A section created and spliced into place, ready to write back to storage.
#[derive(Debug)]
pub struct ResolvedSectionCreate {
    /// The note's full content with the new heading chain spliced in.
    pub full_content: String,
    /// The newly created leaf section's own text (heading line through
    /// body, nothing else) - exactly what re-extracting this section from
    /// `full_content` via `extract_section` would return, so its hash
    /// chains correctly into a later section-scoped edit.
    pub section_content: String,
}

/// Create a new section at `path` within `full_content`, synthesizing any
/// missing ancestor headings along the way.
///
/// `path` is matched the same way every other section tool matches it
/// (case-insensitive, whitespace-normalized, split on `" > "`). If `path`
/// already resolves to an existing section, this errors rather than
/// clobbering it - callers wanting to overwrite an existing section should
/// use [`super::write::resolve_section_for_write`] instead.
///
/// Otherwise, the deepest *existing* ancestor is found by matching
/// progressively shorter prefixes of `path`'s segments against each
/// section's own full ancestor chain (not a suffix match - an ancestor
/// chain is anchored from the root, unlike a bare lookup). Every segment
/// past that ancestor is created as a new heading, cascading one level per
/// segment (`ancestor.level + 1`, `+2`, ...; level 1 if no ancestor exists
/// at all), inserted as the ancestor's last child - immediately after its
/// current `end_line`, or at end-of-file when there's no ancestor. `body`
/// becomes the leaf heading's content verbatim, no separating blank line
/// (matching this crate's existing heading-adjacency convention).
///
/// An empty `full_content` (brand-new note) collapses into the same
/// no-ancestor-found path: `build_outline("")` has zero sections, so every
/// segment in `path` is "missing" and the whole chain is created from
/// scratch at level 1.
pub fn create_section(
    full_content: &str,
    path: &str,
    body: &str,
) -> Result<ResolvedSectionCreate, SectionCreateError> {
    let outline = build_outline(full_content);

    match resolve_section(&outline, path) {
        Ok(section) => {
            return Err(SectionCreateError::AlreadyExists {
                path: section.path.clone(),
            });
        }
        Err(SectionResolveError::Ambiguous { path, candidates }) => {
            return Err(SectionCreateError::Ambiguous { path, candidates });
        }
        Err(SectionResolveError::NotFound { .. }) => {}
    }

    let raw_segments: Vec<String> = path
        .split(" > ")
        .map(|segment| collapse_whitespace(segment.trim()))
        .collect();
    let normalized_input = normalize_segments(path);

    // Find the deepest existing ancestor: the longest proper prefix of
    // `normalized_input` that exactly matches some section's own full
    // ancestor chain. Ties (two sections sharing the same ancestor path)
    // resolve to the first in document order - a create's placement only
    // needs *an* existing anchor, not a uniquely disambiguated one.
    let mut deepest_ancestor_idx: Option<usize> = None;
    let mut matched_prefix_len = 0;
    'outer: for prefix_len in (1..normalized_input.len()).rev() {
        let prefix = &normalized_input[..prefix_len];
        for (idx, section) in outline.sections.iter().enumerate() {
            let section_normalized: Vec<String> = section
                .segments
                .iter()
                .map(|s| collapse_whitespace(s.trim()).to_lowercase())
                .collect();
            if section_normalized == prefix {
                deepest_ancestor_idx = Some(idx);
                matched_prefix_len = prefix_len;
                break 'outer;
            }
        }
    }

    let missing_segments = &raw_segments[matched_prefix_len..];
    let starting_level: u8 = match deepest_ancestor_idx {
        Some(idx) => outline.sections[idx].level + 1,
        None => 1,
    };
    let last_level = starting_level + (missing_segments.len() as u8) - 1;
    if last_level > MAX_HEADING_LEVEL {
        return Err(SectionCreateError::TooDeep {
            path: path.to_string(),
        });
    }

    let mut new_lines: Vec<String> = Vec::with_capacity(missing_segments.len() + 1);
    for (i, text) in missing_segments.iter().enumerate() {
        let level = starting_level + i as u8;
        new_lines.push(format!("{} {}", "#".repeat(level as usize), text));
    }
    let leaf_heading_line = new_lines
        .last()
        .cloned()
        .expect("path always has at least one segment, so at least one heading line is created");
    if !body.is_empty() {
        new_lines.extend(body.split('\n').map(str::to_string));
    }

    let section_content = if body.is_empty() {
        leaf_heading_line
    } else {
        format!("{}\n{}", leaf_heading_line, body)
    };

    let full_content = if full_content.is_empty() {
        new_lines.join("\n")
    } else {
        let mut lines: Vec<&str> = full_content.split('\n').collect();
        let insert_at = deepest_ancestor_idx
            .map(|idx| outline.sections[idx].end_line)
            .unwrap_or(lines.len());
        let new_line_refs: Vec<&str> = new_lines.iter().map(String::as_str).collect();
        lines.splice(insert_at..insert_at, new_line_refs);
        lines.join("\n")
    };

    Ok(ResolvedSectionCreate {
        full_content,
        section_content,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sections::outline::extract_section;

    #[test]
    fn create_section_on_brand_new_empty_note() {
        let resolved = create_section("", "Daily Log", "first entry").unwrap();
        assert_eq!(resolved.full_content, "# Daily Log\nfirst entry");
        assert_eq!(resolved.section_content, "# Daily Log\nfirst entry");
    }

    #[test]
    fn create_section_with_no_matching_ancestor_appends_top_level() {
        let content = "# Existing\nbody";
        let resolved = create_section(content, "New Section", "new body").unwrap();
        assert_eq!(
            resolved.full_content,
            "# Existing\nbody\n# New Section\nnew body"
        );
        assert_eq!(resolved.section_content, "# New Section\nnew body");
    }

    #[test]
    fn create_section_nested_under_existing_ancestor_single_missing_level() {
        let content = "# Top\nintro";
        let resolved = create_section(content, "Top > Middle", "middle body").unwrap();
        assert_eq!(
            resolved.full_content,
            "# Top\nintro\n## Middle\nmiddle body"
        );
        assert_eq!(resolved.section_content, "## Middle\nmiddle body");
    }

    #[test]
    fn create_section_cascades_multiple_missing_ancestors() {
        let content = "# Top\nintro";
        let resolved = create_section(content, "Top > Middle > Leaf", "leaf body").unwrap();
        assert_eq!(
            resolved.full_content,
            "# Top\nintro\n## Middle\n### Leaf\nleaf body"
        );
        assert_eq!(resolved.section_content, "### Leaf\nleaf body");
    }

    #[test]
    fn create_section_with_empty_body_produces_heading_only_section() {
        let content = "# Top\nintro";
        let resolved = create_section(content, "Top > Middle", "").unwrap();
        assert_eq!(resolved.full_content, "# Top\nintro\n## Middle");
        assert_eq!(resolved.section_content, "## Middle");
    }

    #[test]
    fn create_section_max_depth_exceeded_errors() {
        // "# One" through "###### Six" already occupy levels 1-6; one more
        // missing segment nested under "Six" would need level 7.
        let content = "# One\n## Two\n### Three\n#### Four\n##### Five\n###### Six\nbody";
        let err = create_section(
            content,
            "One > Two > Three > Four > Five > Six > Seven",
            "x",
        )
        .unwrap_err();
        assert_eq!(
            err,
            SectionCreateError::TooDeep {
                path: "One > Two > Three > Four > Five > Six > Seven".to_string()
            }
        );
    }

    #[test]
    fn create_section_when_already_exists_errors() {
        let content = "# Top\n## Middle\nbody";
        let err = create_section(content, "Top > Middle", "replacement").unwrap_err();
        assert_eq!(
            err,
            SectionCreateError::AlreadyExists {
                path: "Top > Middle".to_string()
            }
        );
    }

    #[test]
    fn create_section_on_ambiguous_existing_path_errors() {
        let content = "# Notes\nfirst\n# Notes\nsecond";
        let err = create_section(content, "Notes", "x").unwrap_err();
        match err {
            SectionCreateError::Ambiguous { path, candidates } => {
                assert_eq!(path, "Notes");
                assert_eq!(candidates.len(), 2);
            }
            other => panic!("expected Ambiguous, got {:?}", other),
        }
    }

    #[test]
    fn create_section_as_last_child_leaves_siblings_untouched() {
        // "Top" already has a child "Existing Child", and a sibling
        // top-level section "Top Two" follows it. The new section must land
        // as the LAST child of "Top" - after "Existing Child", still before
        // "Top Two" - without disturbing either.
        let content = "# Top\nintro\n## Existing Child\nchild body\n# Top Two\nmore";
        let resolved = create_section(content, "Top > New Child", "new body").unwrap();
        assert_eq!(
            resolved.full_content,
            "# Top\nintro\n## Existing Child\nchild body\n## New Child\nnew body\n# Top Two\nmore"
        );
    }

    #[test]
    fn create_section_trims_and_collapses_whitespace_in_new_heading_text() {
        let resolved = create_section("", "  Daily   Log  ", "body").unwrap();
        assert_eq!(resolved.full_content, "# Daily Log\nbody");
    }

    #[test]
    fn create_section_returned_hash_chains_into_reextraction() {
        // The D2 contract: `section_content` must equal what re-parsing
        // `full_content` and extracting the new leaf section would return -
        // this is what lets the returned hash be used for a later
        // section-scoped edit.
        let content = "# Top\nintro\n# Top Two\nmore";
        let resolved = create_section(content, "Top > Middle", "middle body").unwrap();

        let outline = build_outline(&resolved.full_content);
        let leaf = outline
            .sections
            .iter()
            .find(|s| s.path == "Top > Middle")
            .expect("newly created section should be discoverable in the new outline");
        let extracted = extract_section(&resolved.full_content, leaf);

        assert_eq!(extracted, resolved.section_content);
    }
}
