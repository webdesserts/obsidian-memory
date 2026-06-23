//! A case-INSENSITIVE, case-PRESERVING in-memory filesystem double that models
//! APFS, for the Fix-3 receiver-convergence tests.
//!
//! The production bug (Fix 3 / the 06-22 case-war reborn) is intrinsically a
//! case-insensitive-filesystem interaction: writing `foo/x.md` lands INTO an
//! existing physical `Foo/`, and a single-step directory `rename("Foo", "foo")`
//! is a no-op. The shipped [`InMemoryFs`](vault_sync::InMemoryFs) is case-SENSITIVE
//! (`HashMap<String, Vec<u8>>`), so it cannot reproduce either, which is why the
//! single-vault tests miss the ping-pong. This double fills that gap.
//!
//! ## What it models (faithfully to APFS)
//!
//! - **Case-insensitive resolution.** `exists` / `read` / `write` / `delete` /
//!   `stat` / `rename` resolve by a lowercased key, so `foo/x.md` and `Foo/x.md`
//!   address the SAME entry (the same physical inode).
//! - **Case-PRESERVING display.** Each entry stores its display casing. `list`
//!   returns the STORED casing (NOT the lowercased key) — that is what drives the
//!   case-drift sweep's detection (disk casing vs index casing).
//! - **Directory two-step requirement.** A single-step case-only DIRECTORY rename
//!   (`Foo` → `foo`) is a NO-OP (it resolves to the same key), exactly as on APFS.
//!   Converging a directory's casing therefore requires the two-step
//!   `Foo` → `Foo.casemv-tmp` → `foo`, which the receiver's Facet-A path performs.
//!   A case-only FILE rename converges directly (writing the file under the new
//!   display casing is enough), matching APFS file semantics.

use async_trait::async_trait;
use std::collections::HashMap;
use std::sync::RwLock;
use std::sync::atomic::{AtomicBool, Ordering};
use vault_sync::fs::{FileEntry, FileStat, FileSystem, FsError, Result};

/// An entry's stored display path plus (for files) its bytes.
#[derive(Clone)]
struct Entry {
    /// The display casing this entry currently has on "disk" (what `list` returns).
    display: String,
    /// File contents; `None` marks a directory.
    content: Option<Vec<u8>>,
}

/// Case-insensitive + case-preserving in-memory filesystem (APFS model).
pub struct CaseInsensitiveFs {
    /// Keyed by lowercased path → display casing + content. The lowercased key gives
    /// case-insensitive resolution; the stored `display` gives case-preservation.
    entries: RwLock<HashMap<String, Entry>>,
    /// When set, a `rename` of a NON-`.sync/` path returns an error without mutating
    /// anything. Models a filesystem-level rename failure (EACCES / EXDEV / a transient
    /// I/O error) on a vault file so a test can prove the receiver's case-only
    /// convergence still never destroys content on the failure branch. The `.sync/`
    /// metadata writes (which use temp+rename for atomic index persistence) are left
    /// working so the failure is isolated to the content-move path under test.
    fail_rename: AtomicBool,
}

impl CaseInsensitiveFs {
    pub fn new() -> Self {
        let mut entries = HashMap::new();
        // Root directory always exists (mirrors InMemoryFs).
        entries.insert(
            String::new(),
            Entry {
                display: String::new(),
                content: None,
            },
        );
        Self {
            entries: RwLock::new(entries),
            fail_rename: AtomicBool::new(false),
        }
    }

    /// Make every subsequent `rename` fail (without mutating the tree) until reset.
    pub fn set_fail_rename(&self, fail: bool) {
        self.fail_rename.store(fail, Ordering::SeqCst);
    }

    fn normalize(path: &str) -> String {
        path.trim_matches('/').to_string()
    }

    fn key(path: &str) -> String {
        Self::normalize(path).to_ascii_lowercase()
    }

    fn parent_of(path: &str) -> Option<String> {
        let normalized = Self::normalize(path);
        if normalized.is_empty() {
            None
        } else {
            match normalized.rfind('/') {
                Some(pos) => Some(normalized[..pos].to_string()),
                None => Some(String::new()),
            }
        }
    }

    /// Create a directory (and its parents), case-insensitively. Idempotent: an
    /// existing directory under any casing is a no-op — but if it already exists
    /// under a DIFFERENT display casing, the existing casing is PRESERVED (APFS does
    /// not re-case an existing directory on `mkdir`).
    fn mkdir_sync(entries: &mut HashMap<String, Entry>, path: &str) {
        let normalized = Self::normalize(path);
        if normalized.is_empty() {
            return;
        }
        if let Some(parent) = Self::parent_of(&normalized) {
            Self::mkdir_sync(entries, &parent);
        }
        let key = normalized.to_ascii_lowercase();
        entries.entry(key).or_insert(Entry {
            display: normalized,
            content: None,
        });
    }
}

impl Default for CaseInsensitiveFs {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl FileSystem for CaseInsensitiveFs {
    async fn read(&self, path: &str) -> Result<Vec<u8>> {
        let key = Self::key(path);
        let entries = self.entries.read().unwrap();
        match entries.get(&key) {
            Some(Entry {
                content: Some(bytes),
                ..
            }) => Ok(bytes.clone()),
            _ => Err(FsError::NotFound(Self::normalize(path))),
        }
    }

    async fn write(&self, path: &str, content: &[u8]) -> Result<()> {
        let normalized = Self::normalize(path);
        let key = normalized.to_ascii_lowercase();
        let mut entries = self.entries.write().unwrap();
        if let Some(parent) = Self::parent_of(&normalized) {
            Self::mkdir_sync(&mut entries, &parent);
        }
        // A write under a new casing re-cases the FILE (APFS preserves the directory
        // casing the parent already has, but a file written at a new leaf casing takes
        // that casing). The parent dirs above keep whatever casing they were created
        // with — `mkdir_sync` above does not re-case them.
        entries.insert(
            key,
            Entry {
                display: normalized,
                content: Some(content.to_vec()),
            },
        );
        Ok(())
    }

    async fn list(&self, path: &str) -> Result<Vec<FileEntry>> {
        let dir_key = Self::key(path);
        let entries = self.entries.read().unwrap();
        if !dir_key.is_empty() && !entries.contains_key(&dir_key) {
            return Err(FsError::NotFound(Self::normalize(path)));
        }

        let prefix = if dir_key.is_empty() {
            String::new()
        } else {
            format!("{}/", dir_key)
        };

        let mut out = Vec::new();
        let mut seen = std::collections::HashSet::new();
        for entry in entries.values() {
            if entry.display.is_empty() {
                continue; // the root itself
            }
            let entry_key = entry.display.to_ascii_lowercase();
            // Immediate children of `dir_key` only.
            let rest_key = if prefix.is_empty() {
                entry_key.as_str()
            } else if let Some(r) = entry_key.strip_prefix(&prefix) {
                r
            } else {
                continue;
            };
            if rest_key.is_empty() || rest_key.contains('/') {
                continue;
            }
            // Return the STORED display casing for the child's name (case-preserving).
            let display_name = entry
                .display
                .rsplit('/')
                .next()
                .unwrap_or(&entry.display)
                .to_string();
            if seen.insert(display_name.clone()) {
                out.push(FileEntry {
                    name: display_name,
                    is_dir: entry.content.is_none(),
                });
            }
        }
        Ok(out)
    }

    async fn delete(&self, path: &str) -> Result<()> {
        let key = Self::key(path);
        let mut entries = self.entries.write().unwrap();
        if entries.remove(&key).is_some() {
            Ok(())
        } else {
            Err(FsError::NotFound(Self::normalize(path)))
        }
    }

    async fn exists(&self, path: &str) -> Result<bool> {
        let key = Self::key(path);
        Ok(self.entries.read().unwrap().contains_key(&key))
    }

    async fn stat(&self, path: &str) -> Result<FileStat> {
        let key = Self::key(path);
        let entries = self.entries.read().unwrap();
        match entries.get(&key) {
            Some(entry) => Ok(FileStat {
                mtime_millis: 0,
                size: entry.content.as_ref().map(|c| c.len() as u64).unwrap_or(0),
                is_dir: entry.content.is_none(),
            }),
            None => Err(FsError::NotFound(Self::normalize(path))),
        }
    }

    async fn mkdir(&self, path: &str) -> Result<()> {
        let mut entries = self.entries.write().unwrap();
        Self::mkdir_sync(&mut entries, path);
        Ok(())
    }

    async fn rename(&self, from: &str, to: &str) -> Result<()> {
        // Only inject failures for vault-content renames; let `.sync/` metadata renames
        // (atomic index persistence) succeed so the test isolates the move-path failure.
        let touches_sync = from.starts_with(".sync") || to.starts_with(".sync");
        if self.fail_rename.load(Ordering::SeqCst) && !touches_sync {
            return Err(FsError::Io(format!(
                "injected rename failure: {from} -> {to}"
            )));
        }
        let from_key = Self::key(from);
        let to_norm = Self::normalize(to);
        let to_key = to_norm.to_ascii_lowercase();

        let mut entries = self.entries.write().unwrap();
        let Some(entry) = entries.get(&from_key).cloned() else {
            return Err(FsError::NotFound(Self::normalize(from)));
        };

        // APFS DIRECTORY two-step model: a single-step case-only rename of a DIRECTORY
        // (the key is unchanged, only the casing differs) is a NO-OP — the OS keeps the
        // existing display casing. Converging requires going through a distinct
        // intermediate name (`Foo.casemv-tmp`), whose key differs. A FILE re-cases
        // directly (handled by the else branch below), matching APFS file semantics.
        let is_dir = entry.content.is_none();
        let case_only = from_key == to_key && Self::normalize(from) != to_norm;
        if is_dir && case_only {
            // No-op: directory keeps its current display casing.
            return Ok(());
        }

        // Move the entry itself, re-homing every descendant under the same key prefix
        // (a directory rename relocates all its children at once).
        let from_prefix = format!("{}/", from_key);
        let descendants: Vec<String> = entries
            .keys()
            .filter(|k| k.starts_with(&from_prefix))
            .cloned()
            .collect();

        entries.remove(&from_key);
        entries.insert(
            to_key.clone(),
            Entry {
                display: to_norm.clone(),
                content: entry.content,
            },
        );

        for child_key in descendants {
            let child = entries.remove(&child_key).unwrap();
            let new_child_key = format!("{}{}", to_key, &child_key[from_key.len()..]);
            // Rewrite the child's display path so its prefix reflects the new casing.
            let new_display = format!(
                "{}{}",
                to_norm,
                &child.display[Self::normalize(from).len()..]
            );
            entries.insert(
                new_child_key,
                Entry {
                    display: new_display,
                    content: child.content,
                },
            );
        }
        Ok(())
    }
}
