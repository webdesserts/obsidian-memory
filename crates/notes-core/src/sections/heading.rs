//! Fence-aware ATX heading line parser.
//!
//! Scans markdown content line-by-line, recognizing ATX headings (`#` through
//! `######`) while skipping any lines inside fenced code blocks. See the
//! module doc comment on [`crate::sections`] for the full addressing grammar
//! and the documented limitations (setext headings, markdown-inline text).

/// A single parsed ATX heading line.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HeadingLine {
    /// Heading level, 1-6 (number of leading `#` characters).
    pub level: u8,
    /// Heading text, trimmed and with any closing `#` run stripped.
    pub text: String,
    /// 1-indexed line number within the content (matches `content.split('\n')`).
    pub line: usize,
}

/// Parse all ATX headings out of `content`, skipping lines inside fenced code blocks.
///
/// Lines are considered after trimming all leading whitespace (not just up to
/// 3 spaces per CommonMark) — this vault has real headings and fences nested
/// under list items with deeper indentation, and a stricter rule would produce
/// false negatives on them.
pub fn parse_headings(content: &str) -> Vec<HeadingLine> {
    let mut headings = Vec::new();
    let mut fence: Option<FenceState> = None;

    for (idx, line) in content.split('\n').enumerate() {
        let line_number = idx + 1;
        let trimmed = line.trim_start();

        if let Some(open_fence) = fence_marker(trimmed) {
            match &fence {
                None => {
                    fence = Some(open_fence);
                }
                Some(current)
                    if current.char == open_fence.char && open_fence.len >= current.len =>
                {
                    // A closing fence must use the same character and be at least
                    // as long as the opening fence (CommonMark rule); a different
                    // fence character nested inside does not close the block.
                    fence = None;
                }
                _ => {}
            }
            continue;
        }

        if fence.is_some() {
            continue;
        }

        if let Some((level, text)) = parse_heading_line(trimmed) {
            headings.push(HeadingLine {
                level,
                text,
                line: line_number,
            });
        }
    }

    headings
}

/// State of an open fenced code block: which character (`` ` `` or `~`) opened it
/// and how many characters long the opening marker was.
struct FenceState {
    char: char,
    len: usize,
}

/// If `trimmed` opens or closes a fence (3+ of the same backtick/tilde character,
/// optionally followed by an info string), return its marker character and length.
fn fence_marker(trimmed: &str) -> Option<FenceState> {
    let marker_char = trimmed.chars().next()?;
    if marker_char != '`' && marker_char != '~' {
        return None;
    }

    let len = trimmed.chars().take_while(|&c| c == marker_char).count();
    if len < 3 {
        return None;
    }

    Some(FenceState {
        char: marker_char,
        len,
    })
}

/// If `trimmed` is an ATX heading line, return its level and trimmed text.
fn parse_heading_line(trimmed: &str) -> Option<(u8, String)> {
    let hash_count = trimmed.chars().take_while(|&c| c == '#').count();
    if hash_count == 0 || hash_count > 6 {
        return None;
    }

    let rest = &trimmed[hash_count..];

    // The hash run must be followed by a space/tab or end-of-line. A line like
    // "#tag" (an inline Obsidian tag) is not a heading.
    let text = if rest.is_empty() {
        ""
    } else if rest.starts_with(' ') || rest.starts_with('\t') {
        rest.trim_start_matches([' ', '\t'])
    } else {
        return None;
    };

    // Strip an optional trailing run of `#` characters (CommonMark's closing
    // sequence convention), then trim surrounding whitespace. `.trim()` also
    // strips a trailing `\r` from CRLF line endings, so no separate handling
    // is needed there.
    let text = strip_trailing_hashes(text).trim().to_string();

    Some((hash_count as u8, text))
}

/// Strip a trailing run of `#` characters preceded by whitespace, per CommonMark's
/// closing-sequence convention (e.g. `"Heading ##"` -> `"Heading"`).
fn strip_trailing_hashes(text: &str) -> &str {
    let trimmed_end = text.trim_end_matches('\r');
    let without_hashes = trimmed_end.trim_end_matches('#');
    if without_hashes.len() == trimmed_end.len() {
        // No trailing hashes at all.
        return text;
    }
    // Only strip if the hash run is preceded by whitespace (or the run
    // consumed the whole line), matching CommonMark's requirement that the
    // closing sequence be separated from the text by a space.
    if without_hashes.is_empty() || without_hashes.ends_with([' ', '\t']) {
        &text[..without_hashes.len()]
    } else {
        text
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn levels_and_texts(content: &str) -> Vec<(u8, String)> {
        parse_headings(content)
            .into_iter()
            .map(|h| (h.level, h.text))
            .collect()
    }

    #[test]
    fn atx_levels_1_through_6_are_valid() {
        let content = "# One\n## Two\n### Three\n#### Four\n##### Five\n###### Six";
        assert_eq!(
            levels_and_texts(content),
            vec![
                (1, "One".to_string()),
                (2, "Two".to_string()),
                (3, "Three".to_string()),
                (4, "Four".to_string()),
                (5, "Five".to_string()),
                (6, "Six".to_string()),
            ]
        );
    }

    #[test]
    fn level_seven_is_not_a_heading() {
        let content = "####### Not a heading";
        assert_eq!(parse_headings(content), vec![]);
    }

    #[test]
    fn no_space_after_hash_is_not_a_heading() {
        // Models a real vault pattern: an inline Obsidian tag alone on its own line.
        let content = "#project-tag";
        assert_eq!(parse_headings(content), vec![]);
    }

    #[test]
    fn empty_heading_text_is_valid() {
        let content = "# ";
        let headings = parse_headings(content);
        assert_eq!(headings.len(), 1);
        assert_eq!(headings[0].text, "");
    }

    #[test]
    fn trailing_closing_hashes_are_stripped() {
        let content = "## Heading ##";
        let headings = parse_headings(content);
        assert_eq!(headings[0].text, "Heading");
    }

    #[test]
    fn unicode_heading_text_does_not_panic_and_round_trips() {
        let content = "# 日本語 🎉 heading";
        let headings = parse_headings(content);
        assert_eq!(headings[0].text, "日本語 🎉 heading");
    }

    #[test]
    fn crlf_line_is_tolerated() {
        let content = "# Heading\r\nBody\r\n";
        let headings = parse_headings(content);
        assert_eq!(headings.len(), 1);
        assert_eq!(headings[0].text, "Heading");
    }

    #[test]
    fn fenced_code_block_with_backticks_suppresses_headings_inside() {
        let content = "# Real Heading\n```\n# Not a heading\n```\n## Also Real";
        assert_eq!(
            levels_and_texts(content),
            vec![
                (1, "Real Heading".to_string()),
                (2, "Also Real".to_string())
            ]
        );
    }

    #[test]
    fn fenced_code_block_with_tildes_suppresses_headings_inside() {
        let content = "~~~\n# Not a heading\n~~~\n# Real Heading";
        assert_eq!(
            levels_and_texts(content),
            vec![(1, "Real Heading".to_string())]
        );
    }

    #[test]
    fn fenced_code_block_with_language_tag_suppresses_headings_inside() {
        let content = "```rust\n# Not a heading\n```\n# Real Heading";
        assert_eq!(
            levels_and_texts(content),
            vec![(1, "Real Heading".to_string())]
        );
    }

    #[test]
    fn fence_with_backtick_in_info_string_is_still_recognized_as_a_fence() {
        // CommonMark disallows a backtick in a backtick-fence's info string
        // (to avoid ambiguity with inline code spans), but that rule isn't
        // enforced here - accepted looseness, pinned as documented behavior
        // rather than left as an unspecified accident.
        let content = "```rust `inline` \n# Not a heading\n```\n# Real Heading";
        assert_eq!(
            levels_and_texts(content),
            vec![(1, "Real Heading".to_string())]
        );
    }

    #[test]
    fn different_fence_character_nested_inside_does_not_close_outer_fence() {
        // A `~~~` marker appearing textually inside a ``` fence must not close it.
        let content = "```\n~~~\n# Not a heading\n```\n# Real Heading";
        assert_eq!(
            levels_and_texts(content),
            vec![(1, "Real Heading".to_string())]
        );
    }

    #[test]
    fn unclosed_fence_suppresses_headings_for_rest_of_file() {
        let content = "# Real Heading\n```\n# Not a heading\n## Also not a heading";
        assert_eq!(
            levels_and_texts(content),
            vec![(1, "Real Heading".to_string())]
        );
    }

    #[test]
    fn setext_style_headings_are_not_parsed() {
        let content = "Text\n---\nOther\n===";
        assert_eq!(parse_headings(content), vec![]);
    }

    #[test]
    fn indented_fence_under_list_item_suppresses_headings_inside() {
        // Real vault pattern: fenced code blocks indented under list items.
        let content = "- item\n  ```tsx\n  # not a heading\n  ```\n# Real Heading";
        assert_eq!(
            levels_and_texts(content),
            vec![(1, "Real Heading".to_string())]
        );
    }
}
