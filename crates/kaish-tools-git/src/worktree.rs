//! Working-tree paths an index entry names, resolved so they stay inside the
//! working tree (architecture.md E.2).
//!
//! Shared by every verb that compares the index against the working tree —
//! `status` (B.2) and `diff` (B.4). One implementation, because the
//! containment rule is the same rule for both: a second copy would be a
//! second place for an escape to be reintroduced, and only one of them would
//! be under test.

use std::collections::BTreeMap;
use std::io::ErrorKind;
use std::path::{Component, Path, PathBuf};

use crate::error::GitError;


/// What an escaping index entry is called in a refusal. Never the entry
/// itself: the path is repository content, and echoing it turns the refusal
/// into an existence oracle for the host filesystem.
pub(crate) const ESCAPING_INDEX_ENTRY: &str = "working tree path (an index entry)";

/// Whether a repo-relative index path is one we will resolve at all.
///
/// The lexical screen that has to run before any `stat`. Git's own paths are
/// `/`-separated and carry only ordinary names, so `.`, `..`, an empty
/// component, or a leading `/` is evidence the index was not written by git.
///
/// The screen runs **per `/`-separated segment**, because `/` is the only
/// separator the code downstream knows: [`WorktreePaths::leaf`] splits on it
/// and [`join_repo_relative`] pushes each piece onto a `PathBuf` one at a time.
/// Screening the whole string instead would let a segment that *this platform*
/// reads as several components through as one — `evil\secret.txt` is a single
/// segment to the split and two components to a Windows `join`, so the
/// intermediate `evil` would be built by a screen it never passed. Per-segment
/// is also what keeps unix honest in the other direction: there the same name
/// is one ordinary file, and refusing it would be a lie about a legal path.
pub(crate) fn is_repo_relative(rel: &str) -> bool {
    !rel.is_empty() && rel.split('/').all(is_ordinary_component)
}

/// Whether one `/`-separated segment is an ordinary name we will join.
///
/// Two independent readings, and both earn their place. The string checks see
/// `.`, `..` and the empty segment without asking the platform anything. The
/// `Path::components` check is the platform's own reading of the same bytes,
/// and it is what catches a separator or a drive prefix the string checks have
/// no way to know about — a segment that yields anything but exactly one
/// `Normal` is one the `/` split did not really split.
///
/// A NUL byte is refused here rather than left to fail somewhere below. It
/// cannot occur in a path git wrote, every syscall wrapper would reject it
/// anyway, and a barrier that holds by coincidence is not one.
fn is_ordinary_component(segment: &str) -> bool {
    if segment.is_empty() || segment == "." || segment == ".." || segment.contains('\0') {
        return false;
    }
    let mut components = Path::new(segment).components();
    matches!(components.next(), Some(Component::Normal(_))) && components.next().is_none()
}

/// Resolves repo-relative index paths to host paths that are provably inside
/// the working tree.
///
/// The escape this closes: `symlink_metadata` does not follow the *final*
/// component, but the kernel resolves every component before it. An index
/// entry `evil/x` where the working tree's `evil` is a symlink out of the
/// mount lstats a host file outside the mount and then `std::fs::read`s it —
/// a leaf check never fires, because nothing about the leaf is a symlink.
/// `Path::starts_with` cannot see this either; it is component-lexical, and
/// the symlink is invisible to it.
///
/// So this generalizes `repo.rs`'s `open_leaf` to arbitrary depth: walk the
/// entry's *parent chain* a component at a time from the canonical working
/// tree ([`WorktreePaths::walk_chain`], which is also where the refusal is kept
/// from becoming an existence oracle), then lstat the leaf through the
/// canonical parent. The leaf itself may be a symlink — git tracks symlinks,
/// and their blob is the target string, which [`read_worktree_blob`] reads with
/// `read_link` and never follows.
///
/// Parents are cached because index entries share them heavily: one walk per
/// distinct directory rather than one per entry.
pub(crate) struct WorktreePaths<'a> {
    op: &'static str,
    /// The canonical working tree root — the ceiling for every index entry,
    /// and stricter than the mount root, which is where it has to be: an
    /// index entry names a path *in the working tree*.
    work_dir: &'a Path,
    /// The mount root, named in the refusal. The caller already knows it.
    ceiling: &'a Path,
    /// Repo-relative directory → its canonical host path, or `None` for a
    /// chain that is not on disk.
    dirs: BTreeMap<String, Option<PathBuf>>,
}

impl<'a> WorktreePaths<'a> {
    pub(crate) fn new(op: &'static str, work_dir: &'a Path, ceiling: &'a Path) -> Self {
        WorktreePaths {
            op,
            work_dir,
            ceiling,
            dirs: BTreeMap::new(),
        }
    }

    /// The host path to lstat `rel` at, or `None` when its directory chain is
    /// not on disk — which is how an entry gets reported as deleted.
    pub(crate) fn leaf(&mut self, rel: &str) -> Result<Option<PathBuf>, GitError> {
        // Defense in depth: `run` screens every index path as it reads the
        // index, so this is a second reader of the same rule rather than the
        // only one. It costs one pass over a short string and it is what keeps
        // this type safe to call from anywhere.
        if !is_repo_relative(rel) {
            return Err(self.escapes());
        }
        let (dir_rel, name) = rel.rsplit_once('/').unwrap_or(("", rel));
        let Some(dir) = self.real_dir(dir_rel)? else {
            return Ok(None);
        };
        // `dir` is canonical and inside `work_dir`, and `name` is a single
        // ordinary component, so the only symlink this path can contain is the
        // leaf — which is exactly what lstat declines to follow.
        Ok(Some(dir.join(name)))
    }

    /// The canonical host path of a repo-relative directory, ceiling-checked.
    ///
    /// Cached per directory: index entries share their parents heavily, and the
    /// cached value is the whole decision, refusals included.
    fn real_dir(&mut self, dir_rel: &str) -> Result<Option<PathBuf>, GitError> {
        if dir_rel.is_empty() {
            return Ok(Some(self.work_dir.to_path_buf()));
        }
        if let Some(cached) = self.dirs.get(dir_rel) {
            return Ok(cached.clone());
        }
        let resolved = self.walk_chain(dir_rel)?;
        self.dirs.insert(dir_rel.to_string(), resolved.clone());
        Ok(resolved)
    }

    /// Resolve a directory chain one component at a time, down from the
    /// canonical working tree.
    ///
    /// A whole-chain `canonicalize` cannot do this job, and the reason is the
    /// oracle it hands the repository. It answers `NotFound` for a symlink
    /// whose target is absent and succeeds for one whose target is present, so
    /// an escaping chain came back as a refusal in the first case and as an
    /// ordinary "the file was deleted" in the second — one observable bit
    /// saying whether an arbitrary host path exists, and the repository picks
    /// the path. `repo.rs`'s [`contain`] already refuses to make that
    /// distinction; this is the same rule at depth.
    ///
    /// Walking component-wise moves the decision onto something the repository
    /// planted and therefore already knows: whether a symlink is *present* in
    /// the chain. `symlink_metadata` does not follow the component it names, so
    /// each step is classified before it is traversed, and the one class that
    /// can leave the working tree — a symlink — is resolved and ceiling-checked
    /// on its own. Escaping and dangling give the identical refusal.
    ///
    /// Every other outcome is a fact about the working tree itself: a component
    /// that is not there, or an ordinary file where a directory has to be,
    /// means the entry is gone. No symlink was involved in either, so saying so
    /// reveals nothing outside the mount.
    fn walk_chain(&self, dir_rel: &str) -> Result<Option<PathBuf>, GitError> {
        let mut cur = self.work_dir.to_path_buf();
        for component in dir_rel.split('/') {
            // `cur` is canonical and `component` is a screened ordinary name,
            // so `next` introduces at most one new symlink — the one lstat
            // declines to follow.
            let next = cur.join(component);
            let meta = match std::fs::symlink_metadata(&next) {
                Ok(m) => m,
                // The two ways a chain is honestly absent. `NotADirectory` is
                // unreachable while the walk is the only reader — a non-dir is
                // caught below, at its own component — but a tree edited under
                // us can still produce it, and it means exactly what the
                // non-dir case means. Nothing else is swallowed.
                Err(e) if matches!(e.kind(), ErrorKind::NotFound | ErrorKind::NotADirectory) => {
                    return Ok(None)
                }
                Err(e) => {
                    // `next` is the canonical working tree plus screened
                    // ordinary names, so it names nothing the caller cannot
                    // already see.
                    return Err(GitError::repository(
                        self.op,
                        "resolving a worktree directory",
                        &next,
                        e,
                    ));
                }
            };
            if meta.file_type().is_symlink() {
                match std::fs::canonicalize(&next) {
                    // A symlink that stays inside the working tree is
                    // legitimate — checkouts use them — so it is followed, and
                    // the canonical target becomes the base for what follows.
                    Ok(real) if real.starts_with(self.work_dir) => cur = real,
                    _ => return Err(self.escapes()),
                }
            } else if meta.is_dir() {
                cur = next;
            } else {
                return Ok(None);
            }
        }
        Ok(Some(cur))
    }

    fn escapes(&self) -> GitError {
        GitError::EscapesMount {
            operation: self.op,
            what: ESCAPING_INDEX_ENTRY,
            repo: self.work_dir.to_path_buf(),
            ceiling: self.ceiling.to_path_buf(),
        }
    }
}

/// Join a slash-separated repo-relative path onto a real base directory.
pub(crate) fn join_repo_relative(base: &Path, rel: &str) -> std::path::PathBuf {
    let mut out = base.to_path_buf();
    for component in rel.split('/') {
        out.push(component);
    }
    out
}

/// Read a worktree path's git blob content: the file's bytes, or a symlink's
/// target (which is what git stores as the link's blob).
///
/// `rel` is the repo-relative path, for the one error that names it; `full` is
/// the contained host path [`WorktreePaths::leaf`] resolved.
///
/// The cap is checked against the size already in `meta`, *before* the read:
/// `std::fs::read` sizes its buffer from the same stat, so measuring after
/// reading would measure an allocation the repository chose. Symlinks are
/// exempt — `read_link` returns a path, not file content, and its size is the
/// kernel's `PATH_MAX`, not the repository's to pick.
pub(crate) fn read_worktree_blob(
    op: &'static str,
    rel: &str,
    full: &Path,
    meta: &std::fs::Metadata,
    max_blob_bytes: u64,
) -> Result<Vec<u8>, GitError> {
    if meta.file_type().is_symlink() {
        let target = std::fs::read_link(full)
            .map_err(|e| GitError::repository(op, "reading a symlink", full, e))?;
        Ok(os_path_bytes(&target))
    } else {
        if meta.len() > max_blob_bytes {
            return Err(GitError::BlobTooLarge {
                operation: op,
                path: rel.to_string(),
                size: meta.len(),
                cap: max_blob_bytes,
            });
        }
        std::fs::read(full).map_err(|e| GitError::repository(op, "reading a worktree file", full, e))
    }
}

#[cfg(unix)]
fn os_path_bytes(path: &Path) -> Vec<u8> {
    use std::os::unix::ffi::OsStrExt;
    path.as_os_str().as_bytes().to_vec()
}

#[cfg(not(unix))]
fn os_path_bytes(path: &Path) -> Vec<u8> {
    path.to_string_lossy().into_owned().into_bytes()
}

#[cfg(unix)]
pub(crate) fn is_executable(meta: &std::fs::Metadata) -> bool {
    use std::os::unix::fs::PermissionsExt;
    meta.permissions().mode() & 0o111 != 0
}

#[cfg(not(unix))]
pub(crate) fn is_executable(_meta: &std::fs::Metadata) -> bool {
    // Filesystems without a POSIX executable bit cannot report one; git makes
    // the same assumption via `core.fileMode = false`.
    false
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The index-path screen accepts what git writes and refuses what it does
    /// not.
    #[test]
    fn the_index_path_screen_accepts_only_ordinary_relative_paths() {
        for ok in ["a", "a/b", "a/b/c.txt", "a b/c", "..hidden", "a..b"] {
            assert!(is_repo_relative(ok), "must accept '{ok}'");
        }
        for bad in ["", "/", "/etc/passwd", "a//b", "../x", "a/../b", "./a", "a/."] {
            assert!(!is_repo_relative(bad), "must refuse '{bad}'");
        }
    }

    /// A NUL byte never reaches a syscall. It cannot appear in a real git path,
    /// and every downstream `CString` conversion would fail on it anyway — the
    /// barrier belongs here, where the refusal is deliberate rather than a
    /// coincidence of the layer below.
    #[test]
    fn the_index_path_screen_refuses_a_nul_byte() {
        assert!(!is_repo_relative("a\0b"));
        assert!(!is_repo_relative("dir/a\0b"));
        assert!(!is_repo_relative("\0"));
    }

    /// The screen must run per `/`-segment, because [`WorktreePaths::leaf`] and
    /// [`join_repo_relative`] split on `/` and nothing else.
    ///
    /// On Windows a backslash is a directory separator, so `evil\secret` is one
    /// segment to the split and two components to `PathBuf::join` — an
    /// intermediate directory that never met the screen. On unix the same name
    /// is one ordinary file, and refusing it would be a lie about a legal path.
    #[test]
    fn the_index_path_screen_is_per_slash_segment() {
        #[cfg(windows)]
        assert!(
            !is_repo_relative("evil\\secret.txt"),
            "a backslash is a separator here, so this is two unscreened components"
        );
        #[cfg(not(windows))]
        assert!(
            is_repo_relative("evil\\secret.txt"),
            "a backslash is an ordinary character in a unix filename"
        );
    }

    /// The same rule as a property, so it holds on whichever platform runs it:
    /// nothing the screen accepts may contain a segment that this platform's
    /// `Path` reads as more than one component. The `\` case is the one that
    /// makes it bite on Windows and stay quiet on unix.
    #[test]
    fn no_accepted_segment_is_more_than_one_component() {
        for rel in [
            "a",
            "a/b/c",
            "evil\\secret.txt",
            "a\\b/c\\d",
            "C:once",
            "\\\\server\\share",
        ] {
            if !is_repo_relative(rel) {
                continue;
            }
            for segment in rel.split('/') {
                assert_eq!(
                    Path::new(segment).components().count(),
                    1,
                    "accepted '{rel}' has a multi-component segment '{segment}'"
                );
            }
        }
    }
}
