//! Shared tree-listing walk for `git ls` and `git show`'s tree form
//! (architecture.md B.5, B.6, D4/D5 in the PR4 design notes) — one row shape,
//! one walk, so the two verbs cannot drift into two subtly different
//! listings of the same tree.

use gix_index::hash::ObjectId;
use gix_object::bstr::ByteSlice;
use gix_object::tree::EntryKind as TreeEntryKind;
use gix_object::FindExt;
use gix_odb::HeaderExt;

use crate::error::GitError;
use crate::model::{EntryKind, TreeRow};
use crate::repo::ReadRepo;

/// How deep the `ls`/`show` listing walk may recurse before it is refused.
///
/// The same value the shared tree comparison bounds itself to
/// ([`crate::diffcore`]'s `MAX_FLAT_TREE_DEPTH`, which carried the name
/// `verbs::log::MAX_STAT_TREE_DEPTH` until PR 5 moved that walk) — a second
/// constant with the same number rather than a shared one, because the two
/// walks read different data (this one builds rows with mode/oid/size; that
/// one flattens to a path→oid map for a diff) and sharing a number is not the
/// same as sharing a walk. `verbs::status`'s `MAX_STATUS_TREE_DEPTH` is a third, at 256; each
/// bound names the walk it governs so the bare name cannot be read as one
/// shared limit.
///
/// This walk recurses on the real call stack, so the bound is stack safety,
/// not a sanity cap. A loud refusal, not a silent truncation: a walk that
/// stopped quietly at the limit would make everything below it look absent
/// from the revision, and a repository can nest single-entry trees as deep as
/// it likes to force unbounded recursion.
const MAX_LISTING_TREE_DEPTH: usize = 64;

/// What a repo-relative path names inside a tree — the shared navigation
/// behind `ls`'s `<PATH>` and `show`'s `<rev>:<path>`.
pub(crate) enum TreeTarget {
    /// The path names a subtree: list its entries.
    Tree(ObjectId),
    /// The path names a single non-tree entry: report it as one row.
    Entry(TreeRow),
}

fn kind_of(mode: TreeEntryKind) -> EntryKind {
    match mode {
        TreeEntryKind::Blob | TreeEntryKind::BlobExecutable => EntryKind::File,
        TreeEntryKind::Link => EntryKind::Symlink,
        TreeEntryKind::Commit => EntryKind::Commit,
        TreeEntryKind::Tree => EntryKind::Dir,
    }
}

/// The six-digit octal mode string `git ls-tree` prints.
///
/// Two normalizations, both of which `git ls-tree` also applies, so our
/// `mode` agrees with its column byte for byte (D8 in the PR4 design notes):
///
/// - A tree's raw on-disk mode is five octal digits (`40000`) — one of git's
///   own on-disk quirks — while every display of it (`ls-tree`, `diff --raw`)
///   pads it to six (`040000`).
/// - `kind()` discards the permission bits git does not record, so a tree
///   entry written as `100664` prints `100644`. This is not a divergence:
///   git's own tree reader canonicalizes the same way (`canon_mode` in
///   `tree-walk.c`), and `git ls-tree` prints `100644` for that entry too.
///   Pinned against a live `git ls-tree` oracle by
///   `ls.rs::a_noncanonical_tree_mode_prints_the_mode_git_ls_tree_prints`.
fn mode_of(mode: gix_object::tree::EntryMode) -> String {
    // `as_octal_str()` returns a `BStr`, whose `Display` writes its chunks
    // directly and ignores formatter width/fill flags — `{:0>6}` on the BStr
    // itself would not pad. Going through an owned `String` first is what
    // makes the padding apply.
    let raw = mode.kind().as_octal_str().to_string();
    format!("{raw:0>6}")
}

/// The size to report for one entry: `None` for a tree or a submodule
/// gitlink (a made-up zero would claim a size nobody measured), the object's
/// real byte size otherwise.
fn size_of(repo: &ReadRepo, op: &'static str, kind: EntryKind, oid: ObjectId) -> Result<Option<u64>, GitError> {
    match kind {
        EntryKind::Dir | EntryKind::Commit => Ok(None),
        EntryKind::File | EntryKind::Symlink => Ok(Some(
            repo.objects()
                .header(oid)
                .map_err(|e| GitError::repository(op, "reading an object header", repo.git_dir(), e))?
                .size(),
        )),
    }
}

/// Resolve `path` against `root_tree`, walking down one component at a time.
///
/// An empty path (the default, root of the repository) names the tree
/// itself. A path whose final component is a subtree is a [`TreeTarget::Tree`]
/// to list; any other final component is a single [`TreeTarget::Entry`] —
/// matching `git ls-tree <rev> -- <path>`'s own behavior of describing a
/// non-directory pathspec as one row rather than an error.
pub(crate) fn resolve_path_in_tree(
    repo: &ReadRepo,
    op: &'static str,
    root_tree: ObjectId,
    rev: &str,
    path: &str,
) -> Result<TreeTarget, GitError> {
    let trimmed = path.trim_matches('/');
    if trimmed.is_empty() {
        return Ok(TreeTarget::Tree(root_tree));
    }

    let components: Vec<&str> = trimmed.split('/').collect();
    let mut current = root_tree;
    let mut consumed = String::new();

    for (i, name) in components.iter().enumerate() {
        let mut buf = Vec::new();
        let iter = repo
            .objects()
            .find_tree_iter(&current, &mut buf)
            .map_err(|e| GitError::repository(op, "reading a tree", repo.git_dir(), e))?;
        let mut found = None;
        for entry in iter {
            let entry = entry.map_err(|e| GitError::repository(op, "decoding a tree", repo.git_dir(), e))?;
            if entry.filename.to_str_lossy() == *name {
                found = Some((entry.oid.to_owned(), entry.mode));
                break;
            }
        }
        drop(buf);

        let Some((oid, mode)) = found else {
            return Err(GitError::NoSuchPath {
                operation: op,
                rev: rev.to_string(),
                path: trimmed.to_string(),
                repo: repo.git_dir().to_path_buf(),
            });
        };
        consumed = if consumed.is_empty() {
            (*name).to_string()
        } else {
            format!("{consumed}/{name}")
        };

        let kind = kind_of(mode.kind());
        let is_last = i == components.len() - 1;
        if is_last {
            if kind == EntryKind::Dir {
                return Ok(TreeTarget::Tree(oid));
            }
            let size = size_of(repo, op, kind, oid)?;
            return Ok(TreeTarget::Entry(TreeRow {
                path: consumed,
                kind,
                mode: mode_of(mode),
                oid: oid.to_string(),
                size,
            }));
        }
        // A non-final component must itself be a directory to descend into.
        if kind != EntryKind::Dir {
            return Err(GitError::NoSuchPath {
                operation: op,
                rev: rev.to_string(),
                path: trimmed.to_string(),
                repo: repo.git_dir().to_path_buf(),
            });
        }
        current = oid;
    }
    // `components` is non-empty (the `trimmed.is_empty()` case returned
    // above), so the loop always returns through one of the branches above.
    unreachable!("path components are non-empty here")
}

/// List `tree`'s entries into `out`, honoring `--limit` and `--recursive`.
///
/// Non-recursive: one level, directories included as their own rows (kind
/// `dir`, size `null`) — exactly `git ls-tree`'s default. Recursive: only
/// leaves are reported (files, symlinks, submodule gitlinks), at their full
/// repo-relative path, and a directory is a step to more leaves rather than a
/// row of its own — the same shape `git ls-tree -r` reports (without `-t`).
///
/// Entries are read one at a time straight off `find_tree_iter`, in the order
/// git wrote them, expanding each directory before moving to the next
/// sibling — the same natural depth-first order `git ls-tree -r` reports in
/// (a directory's children appear where it sits in its parent's listing, not
/// batched to the end). Deliberately not collected into an owned `Vec`
/// first: a tree object's own entry count is bounded only by its raw byte
/// size, so a hostile tree with millions of entries must not be materialized
/// before `WalkParams::limit` or the truncation check ever gets a chance to
/// apply.
///
/// The two knobs a tree walk needs, bundled so the walk functions stay under
/// clippy's argument-count lint rather than reaching for
/// `#[allow(clippy::too_many_arguments)]`.
#[derive(Debug, Clone, Copy)]
pub(crate) struct WalkParams {
    /// Whether to expand subtrees.
    pub recursive: bool,
    /// The effective row cap.
    pub limit: usize,
}

/// The rows collected so far, and whether the cap has already been hit.
/// Bundled for the same argument-count reason as [`WalkParams`].
struct Collector<'a> {
    out: &'a mut Vec<TreeRow>,
    truncated: &'a mut bool,
}

pub(crate) fn list_tree(
    repo: &ReadRepo,
    op: &'static str,
    tree: ObjectId,
    prefix: &str,
    params: WalkParams,
    out: &mut Vec<TreeRow>,
    truncated: &mut bool,
) -> Result<(), GitError> {
    let mut collector = Collector { out, truncated };
    list_tree_at_depth(repo, op, tree, prefix, params, &mut collector, 0)
}

fn list_tree_at_depth(
    repo: &ReadRepo,
    op: &'static str,
    tree: ObjectId,
    prefix: &str,
    params: WalkParams,
    collector: &mut Collector<'_>,
    depth: usize,
) -> Result<(), GitError> {
    if *collector.truncated {
        return Ok(());
    }
    if depth > MAX_LISTING_TREE_DEPTH {
        return Err(GitError::TreeTooDeep {
            operation: op,
            limit: MAX_LISTING_TREE_DEPTH,
        });
    }

    // Read one entry at a time straight off the iterator rather than
    // collecting the whole tree into an owned `Vec` first: a tree object's
    // width (its own entry count) is bounded only by its raw byte size, so a
    // hostile tree with millions of entries must not be materialized before
    // `params.limit` or `*collector.truncated` ever gets a chance to apply.
    // `buf` is this call's own scratch buffer — a recursive call three lines
    // down owns a separate one, so nothing here holds `buf` borrowed across
    // that recursion — and stepping the iterator in place still visits
    // siblings in the order git wrote them, expanding each directory before
    // moving to the next (the same depth-first order the old collect-first
    // version produced).
    let mut buf = Vec::new();
    let iter = repo
        .objects()
        .find_tree_iter(&tree, &mut buf)
        .map_err(|e| GitError::repository(op, "reading a tree", repo.git_dir(), e))?;
    for entry in iter {
        if *collector.truncated {
            return Ok(());
        }
        let entry = entry.map_err(|e| GitError::repository(op, "decoding a tree", repo.git_dir(), e))?;
        let name = entry.filename.to_str_lossy().into_owned();
        let path = if prefix.is_empty() { name } else { format!("{prefix}/{name}") };
        let kind = kind_of(entry.mode.kind());
        let mode = entry.mode;
        let oid = entry.oid.to_owned();

        if kind == EntryKind::Dir && params.recursive {
            // Omitted from the rows in recursive mode — the row's job is to
            // name a leaf, and a directory is only ever a step to more of
            // them (matching `git ls-tree -r` without `-t`).
            list_tree_at_depth(repo, op, oid, &path, params, collector, depth + 1)?;
            continue;
        }
        if collector.out.len() >= params.limit {
            *collector.truncated = true;
            return Ok(());
        }
        let size = size_of(repo, op, kind, oid)?;
        collector.out.push(TreeRow {
            path,
            kind,
            mode: mode_of(mode),
            oid: oid.to_string(),
            size,
        });
    }
    Ok(())
}
