//! Portable, machine-independent content hashing for a note's materialized state.
//!
//! Two replicas that hold the same logical document must agree on its content hash
//! byte-for-byte, on every machine and across process restarts. This is the
//! foundation the conflict cascade (P2) relies on to decide a distinct-UUID
//! path collision: "same content" must be a deterministic, reproducible fact, not
//! a process-local accident.
//!
//! The hash is therefore taken over [`ContentDoc::to_markdown`] — the canonical
//! materialized markdown, which already emits frontmatter in a fixed sorted order
//! (Chunk 1b) — with line endings normalized so a CRLF-vs-LF difference (a
//! platform artifact, not a content difference) hashes equal. We use **blake3**:
//! fixed output, pure-Rust portable. A process-seeded hash (the standard-library
//! default hasher) is forbidden here precisely because its output varies per
//! process and would split replicas on identical content.

use crate::content_doc::ContentDoc;

/// A per-document content fingerprint plus the emptiness flag the cascade needs.
///
/// `content_hash` identifies the document's content for "same content?" decisions;
/// `is_empty` distinguishes a truly-blank stub from real content for the cascade's
/// empty-wins rule (rule 2: when an empty doc and a non-empty doc collide on a path,
/// the empty one is dropped). A document is "empty" ONLY if it has neither body nor
/// frontmatter — see [`content_summary`]. Frontmatter alone (tags, title, …) is real
/// content INV-3 forbids the cascade from silently dropping, so a blank-body doc that
/// still carries frontmatter is NOT empty.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ContentSummary {
    pub content_hash: [u8; 32],
    pub is_empty: bool,
}

/// Compute the portable content hash of a document's materialized markdown.
///
/// Returns the same `[u8; 32]` on every machine for the same logical content:
/// it hashes [`ContentDoc::to_markdown`] (frontmatter sorted, body verbatim) with
/// line endings normalized to LF, so a CRLF-vs-LF-only difference hashes equal.
pub fn content_hash(doc: &ContentDoc) -> [u8; 32] {
    let markdown = normalize_line_endings(&doc.to_markdown());
    *blake3::hash(markdown.as_bytes()).as_bytes()
}

/// Build the [`ContentSummary`] for a document.
///
/// `is_empty` is the INV-5.1 emptiness predicate the conflict cascade consumes: a
/// document is empty ONLY if it is truly blank — both its body (trimmed) and its
/// frontmatter are empty (see [`is_truly_empty`]). It is defined purely from the
/// document's logical content with no machine-local input, so every replica agrees.
pub fn content_summary(doc: &ContentDoc) -> ContentSummary {
    ContentSummary {
        content_hash: content_hash(doc),
        is_empty: is_truly_empty(doc),
    }
}

/// Whether a document is truly blank: no body content AND no frontmatter.
///
/// This is the INV-5.1 emptiness predicate. It deliberately includes the frontmatter
/// clause that the spec's bare `body.trim().is_empty()` formula omits: frontmatter
/// (title, tags, …) is real content, and the cascade's empty-wins rule (rule 2)
/// DELETES the empty loser when an empty and a non-empty doc collide on a path.
/// Treating a frontmatter-only doc as empty would let that rule silently drop its
/// frontmatter — an INV-3 violation (no materialized content is ever silently lost).
/// So a doc is empty only when it carries neither a body nor frontmatter.
fn is_truly_empty(doc: &ContentDoc) -> bool {
    doc.body().to_string().trim().is_empty() && doc.frontmatter_is_empty()
}

/// Normalize line endings to LF so a CRLF-vs-LF-only difference — a platform
/// artifact rather than a content difference — does not perturb the content hash.
fn normalize_line_endings(s: &str) -> String {
    s.replace("\r\n", "\n").replace('\r', "\n")
}

#[cfg(test)]
mod tests {
    use super::*;

    const AUTHOR_A: u64 = 0x0101_0101_0101_0101;
    const AUTHOR_B: u64 = 0x0202_0202_0202_0202;

    /// Two documents materializing to the identical markdown hash equal — the
    /// baseline "same content ⇒ same hash" the cascade depends on.
    #[test]
    fn identical_materialized_markdown_hashes_equal() {
        let content = "---\ntitle: Note\n---\n\n# Heading\n\nBody text.";
        let a = ContentDoc::from_markdown(content, AUTHOR_A).unwrap();
        let b = ContentDoc::from_markdown(content, AUTHOR_A).unwrap();

        assert_eq!(
            content_hash(&a),
            content_hash(&b),
            "identical materialized markdown must hash to the same value"
        );
    }

    /// A CRLF-vs-LF-only difference is a platform artifact, not a content
    /// difference, so the two must hash equal. This is the line-ending
    /// normalization requirement — without it the hash would split a Windows
    /// replica from a Unix one on otherwise-identical content.
    #[test]
    fn crlf_and_lf_only_difference_hashes_equal() {
        let lf = ContentDoc::from_markdown("line one\nline two\nline three", AUTHOR_A).unwrap();
        let crlf =
            ContentDoc::from_markdown("line one\r\nline two\r\nline three", AUTHOR_B).unwrap();

        // Sanity: the materialized bodies genuinely differ by line ending, so this
        // test is exercising normalization and not a no-op.
        assert_ne!(
            lf.to_markdown(),
            crlf.to_markdown(),
            "precondition: the two materialized markdowns differ only by line ending"
        );

        assert_eq!(
            content_hash(&lf),
            content_hash(&crlf),
            "CRLF vs LF-only difference must hash equal (line endings normalized)"
        );
    }

    /// A truly-blank document — no body content and no frontmatter — is empty.
    /// This is the only state the cascade's empty-wins rule may delete on a
    /// collision, because there is genuinely no content to lose.
    #[test]
    fn truly_blank_doc_is_empty() {
        let whitespace_body = ContentDoc::from_markdown("   \n\t\n  ", AUTHOR_A).unwrap();
        assert!(
            content_summary(&whitespace_body).is_empty,
            "a whitespace-only body with no frontmatter must be reported empty"
        );

        let completely_empty = ContentDoc::from_markdown("", AUTHOR_B).unwrap();
        assert!(
            content_summary(&completely_empty).is_empty,
            "an empty document (no body, no frontmatter) must be reported empty"
        );
    }

    /// Frontmatter makes a blank-body document NON-empty (INV-5.1) — this is the
    /// INV-3-protecting case. A doc with `title`/`tags` but no body carries real
    /// content; if it were classified empty, the cascade's empty-wins rule (rule 2)
    /// would silently DELETE it when it collided with a body-only doc on the same
    /// path, losing the frontmatter — a direct INV-3 violation. So `is_empty` here
    /// MUST be false.
    ///
    /// This corrects the spec's bare `empty(d) ⇔ body.trim().is_empty()` formula,
    /// which omits the frontmatter clause and contradicts both its own prose
    /// ("frontmatter-only counts as NON-empty") and INV-3. The emptiness predicate
    /// requires BOTH the body and the frontmatter to be blank.
    #[test]
    fn frontmatter_makes_blank_body_doc_non_empty() {
        let doc =
            ContentDoc::from_markdown("---\ntitle: Note\ntags: [a, b]\n---\n", AUTHOR_A).unwrap();

        // Precondition: this document genuinely has frontmatter and a blank body —
        // exactly the frontmatter-only state INV-3 protects.
        assert!(
            doc.to_markdown().contains("title: Note"),
            "precondition: the document carries frontmatter"
        );
        assert!(
            doc.body().to_string().trim().is_empty(),
            "precondition: the document's body is blank"
        );

        assert!(
            !content_summary(&doc).is_empty,
            "frontmatter-only (blank body, non-empty frontmatter) must be NON-empty so the cascade never silently drops the frontmatter (INV-3)"
        );
    }

    /// The converse guard: a real (non-empty) body makes a document non-empty
    /// regardless of whether it has frontmatter. Together with
    /// `frontmatter_makes_blank_body_doc_non_empty`, this pins emptiness to "truly
    /// blank" — empty only when BOTH body and frontmatter are absent.
    #[test]
    fn non_empty_body_is_not_empty() {
        let with_fm =
            ContentDoc::from_markdown("---\ntitle: Note\n---\n\n# Heading\n\nReal body.", AUTHOR_A)
                .unwrap();
        let without_fm = ContentDoc::from_markdown("# Heading\n\nReal body.", AUTHOR_A).unwrap();

        assert!(
            !content_summary(&with_fm).is_empty,
            "a document with a real body (and frontmatter) must not be empty"
        );
        assert!(
            !content_summary(&without_fm).is_empty,
            "a document with a real body (no frontmatter) must not be empty"
        );
    }

    /// Portability tripwire: two independently-constructed `ContentDoc`s built from
    /// the same markdown hash byte-equal. This guards against a future regression
    /// to a seeded/process-dependent hash — such a hash would vary between the two
    /// construction sites and fail this assertion.
    #[test]
    fn hash_is_portable_across_independent_construction() {
        let markdown = "---\nid: 42\n---\n\n# Title\n\nSome body content here.";
        let first = ContentDoc::from_markdown(markdown, AUTHOR_A).unwrap();
        let second = ContentDoc::from_markdown(markdown, AUTHOR_B).unwrap();

        assert_eq!(
            content_hash(&first),
            content_hash(&second),
            "the content hash must be portable — identical for the same content regardless of construction"
        );
    }
}
