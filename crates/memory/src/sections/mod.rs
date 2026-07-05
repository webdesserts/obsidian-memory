//! Section addressing: heading-path-based access to a subset of a note's
//! content, for reading and editing sections of oversized notes without
//! transferring or rewriting the whole file.
//!
//! This is a **fallback for oversized notes**, not the primary editing path -
//! notes should stay small and heavily linked where possible.
//!
//! ## Addressing grammar
//!
//! A section's **path** is the ancestor chain of heading texts (from
//! outermost to itself) joined by `" > "`, e.g. `"Daily Log > 2026-W26-4"`.
//! Two pseudo-sections exist for content outside any heading:
//!
//! - **`"Frontmatter"`** - the YAML frontmatter block, present iff the note
//!   starts with a delimited `---`...`---` block (independent of whether the
//!   YAML inside parses - an unparseable-but-delimited block is still
//!   addressable, e.g. to fix broken YAML via a section edit).
//! - **`"Preamble"`** - content between the frontmatter (or file start) and
//!   the first heading. A headingless note with no frontmatter is entirely
//!   one big Preamble section. Omitted when that span would be empty.
//!
//! A heading section spans its heading line through just before the next
//! heading of the same or higher level (or end of file).
//!
//! ### Matching
//!
//! Matching a path against the outline is a **case-insensitive,
//! whitespace-normalized suffix match**: the input is split on `" > "`
//! (trimming around the separator and collapsing whitespace within each
//! segment), and a section matches if its own full segment list has the
//! input's segments as a trailing suffix. This means a bare unique heading
//! name (`"Configure"`) and a full disambiguating path
//! (`"Daily Log > 2026-W26-4"`) both work through one algorithm - including
//! for the pseudo-sections, which get no special-casing. A real heading
//! literally titled "Frontmatter" correctly produces an *ambiguous* match
//! against the pseudo-section rather than a silent collision.
//!
//! Zero matches is a not-found error naming the input and pointing at the
//! `outline` tool. Two or more matches is an ambiguity error listing each
//! candidate's full canonical path, so the agent can lengthen the input to
//! disambiguate.
//!
//! ### Known limitations (v1, by design)
//!
//! - **Setext headings** (`Text\n===` / `Text\n---`) are not recognized.
//!   Reliably distinguishing a setext underline from a thematic break or a
//!   markdown table separator row is real complexity for a syntax this
//!   vault's content doesn't use.
//! - **Markdown inline syntax** in heading text (bold, italic, inline code,
//!   links, wiki-links) is preserved as-written for both display and
//!   matching - only whitespace is normalized. An agent addressing
//!   `## **Bold Heading**` must include the `**` in the path segment.
//! - **Headings whose text contains the literal `" > "` separator** are not
//!   addressable by their exact full text, since the input path is split on
//!   `" > "` before matching. The guaranteed failure mode is
//!   not-found/ambiguous, never a wrong-section match. Whole-note editing
//!   remains the fallback for such notes.
//!
//! ## Extraction contract
//!
//! All section line coordinates use the same 1-indexed,
//! `full_content.split('\n')`-based system as the rest of the crate's editing
//! tools (matching `apply_line_edits`/`number_lines`, including the
//! trailing-newline "phantom empty final line" convention).
//! [`outline::extract_section`] is the single extraction path used by both
//! reads and writes: it returns exactly the inclusive line range joined with
//! `"\n"`, nothing prepended or appended.

pub mod heading;
pub mod outline;
// `path::resolve_section` has no caller yet - `read_note`/`edit_note`/
// `replace_in_note` section support (later commits) are the callers.
#[allow(dead_code)]
pub mod path;
