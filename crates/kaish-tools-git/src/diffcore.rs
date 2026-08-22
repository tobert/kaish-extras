//! The comparison machinery `status`, `log --stat` and `diff` all need
//! (architecture.md B.2, B.3, B.4).
//!
//! Three things live here, each because two or more verbs would otherwise
//! carry their own copy of it:
//!
//! - [`Class`] — a tree entry, an index entry and a working-tree file
//!   normalized onto one axis, so the three can be compared at all. The
//!   executable bit is its own class because git treats a `100644` ↔ `100755`
//!   flip as a modification.
//! - [`flatten_tree`] — a tree walked to `path → (oid, class)`, with an
//!   explicit stack rather than recursion.
//! - [`pair_exact_renames`] and [`line_delta`] — exact-match rename pairing,
//!   and the added/deleted line counts from `gix-imara-diff`.
//!
//! What is **not** here is `gix-diff`'s tree platform. Its rename tracking is
//! `blob`-gated and `blob` pulls `gix-command` (A.2), the spawn machinery the
//! tripwires forbid, so the comparison is flatten-and-compare and renames are
//! exact-match only — a blob oid reappearing at a new path, never a score.

use std::collections::{BTreeMap, VecDeque};

use gix_index::entry::Mode as IndexMode;
use gix_index::hash::ObjectId;
use gix_object::bstr::ByteSlice;
use gix_object::tree::EntryKind as TreeEntryKind;
use gix_object::FindExt;
use gix_odb::HeaderExt;

use crate::error::GitError;
use crate::model::EntryKind;
use crate::repo::ReadRepo;

/// A file's type, normalized so a tree entry, an index entry and a
/// working-tree file compare on the same axis. The executable bit is a class
/// of its own because git treats a mode flip (`100644` ↔ `100755`) as a
/// modification.
///
/// `Ord` so a rename candidate can be keyed by `(oid, class)`: pairing across
/// classes is what fabricates a rename out of a deleted symlink and an added
/// file that happen to share a blob.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) enum Class {
    File,
    Exec,
    Symlink,
    Commit,
}

impl Class {
    pub(crate) fn from_tree(kind: TreeEntryKind) -> Option<Class> {
        Some(match kind {
            TreeEntryKind::Blob => Class::File,
            TreeEntryKind::BlobExecutable => Class::Exec,
            TreeEntryKind::Link => Class::Symlink,
            TreeEntryKind::Commit => Class::Commit,
            TreeEntryKind::Tree => return None,
        })
    }

    pub(crate) fn from_index(mode: IndexMode) -> Option<Class> {
        Some(match mode {
            IndexMode::FILE => Class::File,
            IndexMode::FILE_EXECUTABLE => Class::Exec,
            IndexMode::SYMLINK => Class::Symlink,
            IndexMode::COMMIT => Class::Commit,
            _ => return None,
        })
    }

    /// The class of a working-tree file, from the metadata an `lstat`
    /// produced. `None` for a directory or anything exotic — where the index
    /// has a blob, git calls that a typechange, and the caller decides how to
    /// report it.
    pub(crate) fn from_metadata(meta: &std::fs::Metadata) -> Option<Class> {
        if meta.file_type().is_symlink() {
            Some(Class::Symlink)
        } else if meta.is_file() {
            if crate::worktree::is_executable(meta) {
                Some(Class::Exec)
            } else {
                Some(Class::File)
            }
        } else {
            None
        }
    }

    pub(crate) fn kind(self) -> EntryKind {
        match self {
            Class::File | Class::Exec => EntryKind::File,
            Class::Symlink => EntryKind::Symlink,
            Class::Commit => EntryKind::Commit,
        }
    }

    /// The six-digit octal git writes for this class — the same string
    /// `git ls-tree` and `git diff --raw` print.
    pub(crate) fn mode_str(self) -> &'static str {
        match self {
            Class::File => "100644",
            Class::Exec => "100755",
            Class::Symlink => "120000",
            Class::Commit => "160000",
        }
    }

    /// Whether two classes differ in *type* (file↔symlink↔submodule), which is
    /// a typechange, as opposed to only in the executable bit, which is a
    /// modification.
    pub(crate) fn is_typechange_from(self, other: Class) -> bool {
        fn family(c: Class) -> u8 {
            match c {
                Class::File | Class::Exec => 0,
                Class::Symlink => 1,
                Class::Commit => 2,
            }
        }
        family(self) != family(other)
    }
}

// ═══════════════════════════════════════════════════════════════════════════
// Flattening a tree
// ═══════════════════════════════════════════════════════════════════════════

/// How deep a tree [`flatten_tree`] walks before refusing.
///
/// A sanity cap, not a stack bound: this walk carries its own `Vec` stack and
/// never recurses, so a deep tree costs heap, not frames. Real trees are tens
/// of levels deep at the outside — the linux kernel's is about a dozen — so
/// nothing that is actually a checkout comes near it, and a hand-built chain
/// of nested single-entry trees stops here with a loud error instead of
/// walking forever.
///
/// A different bound from `verbs::status::MAX_STATUS_TREE_DEPTH` (256), which
/// bounds a genuinely self-recursive walk and is therefore measured against
/// stack exhaustion. Two mechanisms, two appropriate values; docs/issues.md
/// records why they are not expected to converge.
pub(crate) const MAX_FLAT_TREE_DEPTH: usize = 64;

/// Flatten a tree to `path → (oid, class)`, bounded in depth.
///
/// Leaves only: a subtree is a step to more entries, not an entry. `None` is
/// the empty tree — a root commit's parent, or an unborn HEAD — which
/// flattens to nothing, so every path on the other side reads as an addition.
///
/// Entries are consumed straight off `find_tree_iter` rather than collected
/// into an owned `Vec` first: a tree's *width* is bounded only by its own byte
/// size, and materializing a hostile tree's million entries before looking at
/// any of them is a cost the repository picks (docs/issues.md, "git tree walks
/// — bounded depth but not width").
pub(crate) fn flatten_tree(
    repo: &ReadRepo,
    op: &'static str,
    tree: Option<ObjectId>,
    out: &mut BTreeMap<String, (ObjectId, Class)>,
) -> Result<(), GitError> {
    let Some(tree) = tree else {
        return Ok(());
    };
    let mut stack: Vec<(ObjectId, String, usize)> = vec![(tree, String::new(), 0)];
    while let Some((oid, prefix, depth)) = stack.pop() {
        if depth > MAX_FLAT_TREE_DEPTH {
            return Err(GitError::TreeTooDeep {
                operation: op,
                limit: MAX_FLAT_TREE_DEPTH,
            });
        }
        let mut buf = Vec::new();
        let iter = repo
            .objects()
            .find_tree_iter(&oid, &mut buf)
            .map_err(|e| GitError::repository(op, "reading a tree", repo.git_dir(), e))?;
        for entry in iter {
            let entry =
                entry.map_err(|e| GitError::repository(op, "decoding a tree", repo.git_dir(), e))?;
            let name = entry.filename.to_str_lossy();
            let path = if prefix.is_empty() {
                name.into_owned()
            } else {
                format!("{prefix}/{name}")
            };
            match Class::from_tree(entry.mode.kind()) {
                // A subtree: a step to more leaves, not a leaf.
                None => stack.push((entry.oid.to_owned(), path, depth + 1)),
                Some(class) => {
                    out.insert(path, (entry.oid.to_owned(), class));
                }
            }
        }
    }
    Ok(())
}

// ═══════════════════════════════════════════════════════════════════════════
// Exact-match renames
// ═══════════════════════════════════════════════════════════════════════════

/// Pair added paths with deleted paths that carry the same blob and class,
/// returning `added index → deleted index`.
///
/// **Exact match only, and permanently so.** `gix-diff`'s rename tracker is
/// `blob`-gated and `blob` pulls `gix-command` (A.2), so a rename is a blob
/// oid reappearing at a new path and nothing else. A file that was modified
/// *and* moved has a different oid, never pairs, and is reported honestly as
/// a delete plus an add. Copy detection does not exist here at all.
///
/// The class is part of the key, not an afterthought: a symlink's blob is its
/// target string, so `ln -s hello a` and a file containing `hello` share an
/// oid, and pairing on oid alone would report a rename between two things git
/// would never call one.
///
/// Both slices must be in path order — the caller builds them from a
/// `BTreeMap` — so the source a rename claims is the lowest-sorting unclaimed
/// one, the same on every run rather than whatever a linear scan reached
/// first.
pub(crate) fn pair_exact_renames(
    added: &[(String, ObjectId, Class)],
    deleted: &[(String, ObjectId, Class)],
) -> BTreeMap<usize, usize> {
    let mut candidates: BTreeMap<(ObjectId, Class), VecDeque<usize>> = BTreeMap::new();
    for (i, (_, oid, class)) in deleted.iter().enumerate() {
        candidates.entry((*oid, *class)).or_default().push_back(i);
    }
    let mut pairs = BTreeMap::new();
    for (ai, (_, add_oid, add_class)) in added.iter().enumerate() {
        if let Some(di) = candidates
            .get_mut(&(*add_oid, *add_class))
            .and_then(VecDeque::pop_front)
        {
            pairs.insert(ai, di);
        }
    }
    pairs
}

// ═══════════════════════════════════════════════════════════════════════════
// Line counts
// ═══════════════════════════════════════════════════════════════════════════

/// One side of a changed path, as the caller can name it.
pub(crate) enum Side<'a> {
    /// The path does not exist on this side — an add's old side, or a
    /// delete's new side. Genuinely empty, not a declined read.
    Absent,
    /// An object in this repository's store.
    Object(ObjectId),
    /// Content already read from the working tree, which has no object to
    /// name (`git diff --raw` prints all-zeros for the same reason).
    Bytes(&'a [u8]),
}

/// What counting one changed path's lines produced.
///
/// Four outcomes, not two. "Declined by a limit" and "has no lines to count"
/// are different facts, and a caller that folded them together would report a
/// PNG as a file too large to read.
pub(crate) enum LineDelta {
    /// Both sides were read and diffed.
    Counted { added: u64, deleted: u64 },
    /// A side was over `max_blob_bytes`, so the delta was declined.
    OverCap,
    /// Binary content on at least one side — no line count exists to report.
    /// Git leaves these out of its `--numstat` totals too, printing `-`.
    Binary,
    /// A submodule gitlink — a commit in another repository, with no blob
    /// here to count lines in.
    Gitlink,
}

/// Added and deleted line counts for one changed path.
///
/// Both sides are read whole, each bounded by `max_blob_bytes` — that cap is
/// what stands between a repository and an allocation the repository picked,
/// so it is checked against the object header *before* any content is read.
pub(crate) fn line_delta(
    repo: &ReadRepo,
    op: &'static str,
    old: Side<'_>,
    new: Side<'_>,
    max_blob_bytes: u64,
) -> Result<LineDelta, GitError> {
    let old = load(repo, op, old, max_blob_bytes)?;
    let new = load(repo, op, new, max_blob_bytes)?;
    let (old, new) = match (&old, &new) {
        (Loaded::Content(o), Loaded::Content(n)) => (o.as_ref(), n.as_ref()),
        (Loaded::Gitlink, _) | (_, Loaded::Gitlink) => return Ok(LineDelta::Gitlink),
        _ => return Ok(LineDelta::OverCap),
    };
    // A NUL byte is git's own binary heuristic, and a binary file has no line
    // count worth reporting.
    if old.contains(&0) || new.contains(&0) {
        return Ok(LineDelta::Binary);
    }

    let old_text = String::from_utf8_lossy(old);
    let new_text = String::from_utf8_lossy(new);
    // `lines` tokenizes on line boundaries, so a "token" here is a line and
    // the counts are line counts — the same unit `git diff --numstat` reports.
    let input = gix_imara_diff::InternedInput::new(
        gix_imara_diff::sources::lines(old_text.as_ref()),
        gix_imara_diff::sources::lines(new_text.as_ref()),
    );
    let diff = gix_imara_diff::Diff::compute(gix_imara_diff::Algorithm::Myers, &input);
    Ok(LineDelta::Counted {
        added: u64::from(diff.count_additions()),
        deleted: u64::from(diff.count_removals()),
    })
}

/// One side's bytes, as far as the cap allowed us to read them.
enum Loaded<'a> {
    Content(std::borrow::Cow<'a, [u8]>),
    OverCap,
    Gitlink,
}

/// Materialize one side, refusing an object larger than the cap.
///
/// The header is read first, and the content only if the header says it fits.
/// `find` decompresses the whole object before returning, so checking the size
/// afterwards would measure an allocation the repository chose — the cap would
/// bound the line count and not the thing it exists to bound.
fn load<'a>(
    repo: &ReadRepo,
    op: &'static str,
    side: Side<'a>,
    max_blob_bytes: u64,
) -> Result<Loaded<'a>, GitError> {
    let oid = match side {
        Side::Absent => return Ok(Loaded::Content(std::borrow::Cow::Borrowed(&[]))),
        Side::Bytes(bytes) => return Ok(Loaded::Content(std::borrow::Cow::Borrowed(bytes))),
        Side::Object(oid) => oid,
    };
    let header = repo
        .objects()
        .header(oid)
        .map_err(|e| GitError::repository(op, "reading an object header", repo.git_dir(), e))?;
    if header.kind() != gix_object::Kind::Blob {
        return Ok(Loaded::Gitlink);
    }
    if header.size() > max_blob_bytes {
        return Ok(Loaded::OverCap);
    }
    let mut buf = Vec::new();
    let data = repo
        .objects()
        .find(&oid, &mut buf)
        .map_err(|e| GitError::repository(op, "reading a blob", repo.git_dir(), e))?;
    Ok(Loaded::Content(std::borrow::Cow::Owned(data.data.to_vec())))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn oid(byte: u8) -> ObjectId {
        ObjectId::from_hex(format!("{byte:02x}").repeat(20).as_bytes()).expect("hex oid")
    }

    /// The whole rename rule: same blob, same class, lowest-sorting unclaimed
    /// source. A second add sharing the oid finds the next source, not the
    /// same one twice.
    #[test]
    fn renames_pair_on_oid_and_class_in_path_order() {
        let added = vec![
            ("new/a".to_string(), oid(1), Class::File),
            ("new/b".to_string(), oid(1), Class::File),
        ];
        let deleted = vec![
            ("old/a".to_string(), oid(1), Class::File),
            ("old/b".to_string(), oid(1), Class::File),
        ];
        let pairs = pair_exact_renames(&added, &deleted);
        assert_eq!(pairs.get(&0), Some(&0));
        assert_eq!(pairs.get(&1), Some(&1));
    }

    /// A symlink's blob is its target string, so a link to `hello` and a file
    /// containing `hello` share an oid. Pairing them would invent a rename
    /// git would never report.
    #[test]
    fn a_rename_never_pairs_across_classes() {
        let added = vec![("f".to_string(), oid(2), Class::File)];
        let deleted = vec![("l".to_string(), oid(2), Class::Symlink)];
        assert!(pair_exact_renames(&added, &deleted).is_empty());
    }

    /// A different oid is a different file. This is the exact-match limit
    /// stated as a test: a modified-then-moved file must not pair.
    #[test]
    fn a_changed_blob_never_pairs() {
        let added = vec![("new".to_string(), oid(3), Class::File)];
        let deleted = vec![("old".to_string(), oid(4), Class::File)];
        assert!(pair_exact_renames(&added, &deleted).is_empty());
    }

    /// The modes are the six-digit strings git prints, not five-digit ones.
    #[test]
    fn modes_are_git_shaped() {
        assert_eq!(Class::File.mode_str(), "100644");
        assert_eq!(Class::Exec.mode_str(), "100755");
        assert_eq!(Class::Symlink.mode_str(), "120000");
        assert_eq!(Class::Commit.mode_str(), "160000");
    }

    /// An executable-bit flip is a modification, not a typechange; a
    /// file↔symlink flip is a typechange.
    #[test]
    fn only_a_family_change_is_a_typechange() {
        assert!(!Class::Exec.is_typechange_from(Class::File));
        assert!(Class::Symlink.is_typechange_from(Class::File));
        assert!(Class::Commit.is_typechange_from(Class::File));
    }
}
