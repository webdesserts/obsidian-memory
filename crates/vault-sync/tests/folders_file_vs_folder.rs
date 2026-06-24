//! Folder-structural acceptance tests — file-vs-folder collision (AC-INV-1.5d).
//!
//! A file node and a folder node collide at one display path. The folder wins the path;
//! the file relocates INSIDE it at `<folder>/<filename>`, UUID + content preserved, zero
//! loss (INV-1.5d, DECIDED relocate-inside). To construct the shape, a folder is named to
//! coincide with a file — files are `.md`, so the folder segment ends in `.md`.
//!
//! Everything runs against `InMemoryFs` — no test touches a real vault. The
//! replica/handshake/edit helpers live in the shared [`common`] harness.

mod common;
use common::*;

use std::collections::BTreeSet;

// ===================== AC-INV-1.5d — file-vs-folder (relocate inside) =====================
//
// A file node and a folder node collide at one display path. The folder wins the path;
// the file relocates INSIDE it at `<folder>/<filename>`, UUID + content preserved, zero
// loss (INV-1.5d, DECIDED relocate-inside). To construct the shape, a folder is named to
// coincide with a file — files are `.md`, so the folder segment ends in `.md`.

mod ac_inv_1_5d_file_vs_folder {
    use super::*;

    /// A creates a FOLDER `Notes.md/` (by indexing `Notes.md/x.md`); B creates a FILE
    /// `Notes.md`. After they converge, the folder keeps `Notes.md`, and B's file
    /// relocates to `Notes.md/Notes.md` — its UUID and body preserved, both present, on
    /// both replicas and in both directions.
    #[tokio::test]
    async fn folder_wins_path_file_relocates_inside() {
        for direction in ["a_first", "b_first"] {
            let (a, b, fs_a, fs_b) = two_vaults().await;

            // A: a folder named `Notes.md` holding a child file.
            write_and_index(&a, &fs_a, "Notes.md/x.md", "# Inside\n\nFolder child.\n").await;
            // B: a real file at `Notes.md`.
            write_and_index(&b, &fs_b, "Notes.md", "# File\n\nA real note.\n").await;
            let file_uuid = uuid_at(&b, "Notes.md");
            let child_uuid = uuid_at(&a, "Notes.md/x.md");

            match direction {
                "a_first" => sync_both_ways(&a, &b).await,
                _ => sync_both_ways(&b, &a).await,
            }

            // The folder wins `Notes.md`: its child still lives there, with its UUID.
            for (label, vault) in [("A", &a), ("B", &b)] {
                assert_eq!(
                    uuid_at(vault, "Notes.md/x.md"),
                    child_uuid,
                    "[{direction}] {label}: the folder's child keeps its path + UUID"
                );
                // The relocated file lives INSIDE the folder, same UUID.
                assert_eq!(
                    uuid_at(vault, "Notes.md/Notes.md"),
                    file_uuid,
                    "[{direction}] {label}: B's file relocated to <folder>/<filename>, UUID preserved"
                );
            }

            // Exactly the two files materialize — the folder child + the relocated file
            // — on both replicas. (`Notes.md` is now a directory, not a `.md` file.)
            let expected =
                BTreeSet::from(["Notes.md/x.md".to_string(), "Notes.md/Notes.md".to_string()]);
            assert_eq!(md_files(&a).await, expected, "[{direction}] A's file set");
            assert_eq!(md_files(&b).await, expected, "[{direction}] B's file set");

            // Both bodies survive (INV-3): the child's and the relocated file's.
            for (label, fs) in [("A", &fs_a), ("B", &fs_b)] {
                assert!(
                    read_md_str(fs, "Notes.md/x.md")
                        .await
                        .contains("Folder child."),
                    "[{direction}] {label}: the folder child's body survived"
                );
                assert!(
                    read_md_str(fs, "Notes.md/Notes.md")
                        .await
                        .contains("A real note."),
                    "[{direction}] {label}: the relocated file's body survived inside the folder"
                );
            }
        }
    }

    /// File-vs-folder whose relocation target is ALREADY occupied: A's folder `Notes.md/`
    /// already holds a live `Notes.md/Notes.md`, and B creates a file `Notes.md` that
    /// would relocate onto exactly that path. The relocate surfaces a file collision the
    /// same pass resolves — min-UUID wins `Notes.md/Notes.md`, the other gets a conflict
    /// file — nothing lost.
    #[tokio::test]
    async fn relocation_onto_occupied_target_falls_to_cascade() {
        let (a, b, fs_a, fs_b) = two_vaults().await;

        // A: a folder `Notes.md/` that already holds a live file at the relocation
        // target `Notes.md/Notes.md`.
        write_and_index(
            &a,
            &fs_a,
            "Notes.md/Notes.md",
            "# Occupant\n\nAlready here.\n",
        )
        .await;
        let occupant = uuid_at(&a, "Notes.md/Notes.md");
        // B: a file at `Notes.md` that will relocate INTO the folder, onto the occupant.
        write_and_index(&b, &fs_b, "Notes.md", "# Incoming\n\nRelocating in.\n").await;
        let incoming = uuid_at(&b, "Notes.md");

        sync_both_ways(&a, &b).await;

        // The relocation target `Notes.md/Notes.md` resolves by min-UUID; the other gets
        // a conflict file off it.
        let target_winner = occupant.min(incoming);
        let target_loser = occupant.max(incoming);
        let further = conflict_path_for("Notes.md/Notes.md", &target_loser);

        for (label, vault) in [("A", &a), ("B", &b)] {
            assert_eq!(
                uuid_at(vault, "Notes.md/Notes.md"),
                target_winner,
                "[{label}] min-UUID wins the relocation target"
            );
            assert_eq!(
                uuid_at(vault, &further),
                target_loser,
                "[{label}] the other lands at a conflict file off the target"
            );
        }

        // Both documents survive (INV-3): occupant + incoming, at distinct paths.
        let expected = BTreeSet::from(["Notes.md/Notes.md".to_string(), further.clone()]);
        assert_eq!(
            md_files(&a).await,
            expected,
            "A: occupant + relocated-loser conflict"
        );
        assert_eq!(md_files(&b).await, expected, "B: same exact set");
        for (label, fs) in [("A", &fs_a), ("B", &fs_b)] {
            let bodies = format!(
                "{}\n{}",
                read_md_str(fs, "Notes.md/Notes.md").await,
                read_md_str(fs, &further).await,
            );
            assert!(
                bodies.contains("Already here."),
                "[{label}] occupant body survived"
            );
            assert!(
                bodies.contains("Relocating in."),
                "[{label}] incoming body survived"
            );
        }
    }
}
