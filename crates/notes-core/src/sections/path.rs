//! Section path resolution: matches an agent-supplied heading path against
//! an [`Outline`]'s sections.

use super::outline::{Outline, Section};

/// Error resolving a section path against an outline.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SectionResolveError {
    /// No section's path suffix-matched the input.
    NotFound { path: String },
    /// 2+ sections' paths suffix-matched the input. Candidates are the full
    /// canonical paths of each match (not the input), so the agent can
    /// lengthen the input to disambiguate.
    Ambiguous {
        path: String,
        candidates: Vec<String>,
    },
}

/// Resolve `path` against `outline`'s sections.
///
/// Matching is case-insensitive, whitespace-normalized suffix match: the
/// input is split on `" > "` (trimming around the separator and within each
/// segment), and a section matches if its own true segment list (the
/// ancestor chain of untouched heading texts - see [`Section::segments`])
/// has the input's segments as a trailing suffix. This means both a bare
/// unique heading name and a full disambiguating path work through the same
/// algorithm - no special-casing for pseudo-sections either. Matching against
/// `segments` rather than re-splitting the display `path` string also means a
/// heading whose own text happens to contain the literal `" > "` separator
/// is not addressable by its exact text: splitting the input produces more
/// segments than the section structurally has, so it can never match.
pub fn resolve_section<'a>(
    outline: &'a Outline,
    path: &str,
) -> Result<&'a Section, SectionResolveError> {
    let input_segments = normalize_segments(path);

    let matches: Vec<&Section> = outline
        .sections
        .iter()
        .filter(|section| {
            let section_segments: Vec<String> = section
                .segments
                .iter()
                .map(|segment| collapse_whitespace(segment.trim()).to_lowercase())
                .collect();
            is_trailing_suffix(&section_segments, &input_segments)
        })
        .collect();

    match matches.len() {
        0 => Err(SectionResolveError::NotFound {
            path: path.to_string(),
        }),
        1 => Ok(matches[0]),
        _ => Err(SectionResolveError::Ambiguous {
            path: path.to_string(),
            candidates: matches.iter().map(|s| s.path.clone()).collect(),
        }),
    }
}

/// Split a path on `" > "`, trimming and collapsing internal whitespace runs
/// within each segment, then lowercasing for case-insensitive comparison.
fn normalize_segments(path: &str) -> Vec<String> {
    path.split(" > ")
        .map(|segment| collapse_whitespace(segment.trim()).to_lowercase())
        .collect()
}

/// Collapse runs of whitespace within a string down to single spaces.
fn collapse_whitespace(s: &str) -> String {
    s.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// True if `needle` is a trailing suffix of `haystack` (both already
/// normalized), i.e. `haystack[haystack.len() - needle.len()..] == needle`.
fn is_trailing_suffix(haystack: &[String], needle: &[String]) -> bool {
    if needle.len() > haystack.len() || needle.is_empty() {
        return false;
    }
    let offset = haystack.len() - needle.len();
    haystack[offset..] == *needle
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sections::outline::build_outline;

    #[test]
    fn exact_single_segment_match() {
        let outline = build_outline("# Configure\nbody");
        let section = resolve_section(&outline, "Configure").unwrap();
        assert_eq!(section.path, "Configure");
    }

    #[test]
    fn multi_segment_suffix_match() {
        let content = "# Daily Log\n## 2026-W26-4\nbody";
        let outline = build_outline(content);

        // Full disambiguating path works.
        let by_full_path = resolve_section(&outline, "Daily Log > 2026-W26-4").unwrap();
        assert_eq!(by_full_path.path, "Daily Log > 2026-W26-4");

        // Bare unique heading name also works.
        let by_bare_name = resolve_section(&outline, "2026-W26-4").unwrap();
        assert_eq!(by_bare_name.path, "Daily Log > 2026-W26-4");
    }

    #[test]
    fn case_insensitive_matching() {
        let outline = build_outline("# Daily Log\nbody");
        let section = resolve_section(&outline, "daily log").unwrap();
        assert_eq!(section.path, "Daily Log");
    }

    #[test]
    fn whitespace_collapsed_and_trimmed_around_separator_and_segments() {
        let content = "# Daily Log\n## 2026-W26-4\nbody";
        let outline = build_outline(content);
        let section = resolve_section(&outline, "  Daily   Log  >   2026-W26-4  ").unwrap();
        assert_eq!(section.path, "Daily Log > 2026-W26-4");
    }

    #[test]
    fn ambiguous_match_returns_both_candidates_as_full_canonical_paths() {
        let content = "# Notes\nfirst\n# Notes\nsecond";
        let outline = build_outline(content);
        let err = resolve_section(&outline, "Notes").unwrap_err();
        match err {
            SectionResolveError::Ambiguous { path, candidates } => {
                assert_eq!(path, "Notes");
                assert_eq!(candidates, vec!["Notes".to_string(), "Notes".to_string()]);
            }
            other => panic!("expected Ambiguous, got {:?}", other),
        }
    }

    #[test]
    fn unknown_path_returns_not_found_naming_the_input() {
        let outline = build_outline("# Configure\nbody");
        let err = resolve_section(&outline, "Nonexistent").unwrap_err();
        match err {
            SectionResolveError::NotFound { path } => assert_eq!(path, "Nonexistent"),
            other => panic!("expected NotFound, got {:?}", other),
        }
    }

    #[test]
    fn frontmatter_and_preamble_resolve_to_pseudo_sections() {
        let content = "---\ntitle: Test\n---\npreamble text\n# Heading\nbody";
        let outline = build_outline(content);

        let frontmatter = resolve_section(&outline, "Frontmatter").unwrap();
        assert_eq!(frontmatter.path, "Frontmatter");

        let preamble = resolve_section(&outline, "Preamble").unwrap();
        assert_eq!(preamble.path, "Preamble");
    }

    #[test]
    fn real_heading_named_frontmatter_is_ambiguous_against_pseudo_section() {
        // A real heading that happens to be literally titled "Frontmatter"
        // should produce an ambiguity error against the pseudo-section rather
        // than a silent collision.
        let content = "---\ntitle: Test\n---\n# Frontmatter\nbody";
        let outline = build_outline(content);
        let err = resolve_section(&outline, "Frontmatter").unwrap_err();
        match err {
            SectionResolveError::Ambiguous { candidates, .. } => {
                assert_eq!(candidates.len(), 2);
            }
            other => panic!("expected Ambiguous, got {:?}", other),
        }
    }

    #[test]
    fn heading_containing_literal_separator_is_not_addressable_by_exact_path() {
        // Real vault fixture: a heading whose text itself contains " > ".
        // Because the input path is split on " > " before matching, this
        // heading can never be addressed by its exact full text - the
        // guaranteed failure mode is not-found/ambiguous, never a wrong match.
        let heading_text = "6. Observability is binary (queue size logged when > 0) — **P1, S**";
        let content = format!("### {}\nbody", heading_text);
        let outline = build_outline(&content);

        let err = resolve_section(&outline, heading_text).unwrap_err();
        assert!(matches!(err, SectionResolveError::NotFound { .. }));
    }
}
