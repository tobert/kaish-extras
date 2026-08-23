//! The comparison machinery `status`, `log --stat` and `diff` all need
//! (architecture.md B.2, B.3, B.4).
//!
//! What lives here, each because two or more verbs would otherwise carry
//! their own copy of it:
//!
//! - [`Class`] — a tree entry, an index entry and a working-tree file
//!   normalized onto one axis, so the three can be compared at all. The
//!   executable bit is its own class because git treats a `100644` ↔ `100755`
//!   flip as a modification.
//! - [`flatten_tree`] — a tree walked to `path → (oid, class)`, with an
//!   explicit stack rather than recursion.
//! - [`pair_exact_renames`] and [`line_delta`] — exact-match rename pairing,
//!   and the added/deleted line counts from `gix-imara-diff`.
//! - [`line_hunks`] (`textdiff` only) — the same comparison, keeping the
//!   hunks instead of only the counts.
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
#[cfg(feature = "textdiff")]
use crate::model::{DiffHunk, DiffLine, DiffOp};
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

    /// The class of an index entry, read the way git reads a mode: the
    /// file-type bits (`0o170000`) pick the class, and for a regular file the
    /// owner-execute bit (`0o100`) picks `File` or `Exec`. Every other
    /// permission bit is noise git does not record — `100664` and `100600`
    /// are both `File`, and `git status` reports nothing for either
    /// (`status.rs::an_index_mode_git_reads_as_a_file_is_not_reported_deleted`).
    ///
    /// `None` where git has no answer either: `040000` is a sparse-directory
    /// entry, which needs an index expansion this build does not do, and any
    /// other file-type bits abort real git's own index reader
    /// (`BUG: unsupported ce_mode`). The callers refuse — see
    /// [`class_of_index_mode`], which is what both of them actually call.
    ///
    /// One mode cannot be seen here at all: `gix_index`'s decoder builds the
    /// mode with `Mode::from_bits_truncate`, which drops every bit outside
    /// the union of its five named modes (`0o160755`) before this function
    /// runs. A file-type bit of `0o010000` is among them, so an entry written
    /// as `110644` arrives as `100644` and reads as a plain file, where real
    /// git aborts on it. Pinned in
    /// `status.rs::an_index_mode_gix_truncates_reads_as_a_file_where_git_aborts`.
    pub(crate) fn from_index(mode: IndexMode) -> Option<Class> {
        /// The file-type bits of a mode, git's `S_IFMT`.
        const IFMT: u32 = 0o170000;
        /// The owner-execute bit, the only permission bit git records.
        const EXEC: u32 = 0o100;
        let bits = mode.bits();
        Some(match bits & IFMT {
            0o100000 if bits & EXEC == EXEC => Class::Exec,
            0o100000 => Class::File,
            0o120000 => Class::Symlink,
            0o160000 => Class::Commit,
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

/// The class of an index entry, or the refusal for a mode this build cannot
/// place — the form both `status` and `diff` use.
///
/// Skipping the entry instead is the bug this replaced: a skipped entry is
/// absent from the stage-0 map, and a path present in HEAD but absent from
/// that map is reported `deleted`. A file that is still on disk, still
/// tracked, and reported deleted is a wrong answer wearing a normal status,
/// which is worse than a refusal that names what it could not read.
pub(crate) fn class_of_index_mode(
    op: &'static str,
    repo: &std::path::Path,
    mode: IndexMode,
) -> Result<Class, GitError> {
    Class::from_index(mode).ok_or_else(|| GitError::UnsupportedIndexMode {
        operation: op,
        repo: repo.to_path_buf(),
        // Six digits, the width `git ls-files -s` prints — `040000` and
        // `100644` line up in a report that carries both.
        mode: format!("{:06o}", mode.bits()),
    })
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

/// One end of a comparison, flattened: every path it holds, with the blob oid
/// and class at that path.
pub(crate) type PathMap = BTreeMap<String, (ObjectId, Class)>;

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
    out: &mut PathMap,
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
    /// A blob in this repository's store.
    Object(ObjectId),
    /// A submodule gitlink. Named by the caller from the tree or index mode,
    /// never probed from the object store: a gitlink's oid is a commit in
    /// *another* repository, so asking this store for its header fails with
    /// "could not be found" rather than answering "not a blob". Reading the
    /// class the caller already has is both cheaper and the only way that
    /// works.
    Gitlink,
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
    let (old, new) = match load_both(repo, op, old, new, max_blob_bytes)? {
        Sides::Text { old, new } => (old, new),
        Sides::OverCap => return Ok(LineDelta::OverCap),
        Sides::Binary => return Ok(LineDelta::Binary),
        Sides::Gitlink => return Ok(LineDelta::Gitlink),
    };
    let input = intern(&old, &new);
    let diff = gix_imara_diff::Diff::compute(gix_imara_diff::Algorithm::Myers, &input);
    Ok(LineDelta::Counted {
        added: u64::from(diff.count_additions()),
        deleted: u64::from(diff.count_removals()),
    })
}

/// Both sides materialized, or the one word that says why they were not.
enum Sides {
    /// Both sides are text this build will diff. Lossy UTF-8: content with a
    /// NUL byte never reaches here, but content that merely is not valid
    /// UTF-8 does, and carries U+FFFD where the invalid bytes were.
    Text { old: String, new: String },
    /// A side was over `max_blob_bytes`.
    OverCap,
    /// A NUL byte on at least one side — git's own binary heuristic.
    Binary,
    /// A submodule gitlink.
    Gitlink,
}

/// Read both sides and classify them, the one place the cap, the binary
/// heuristic and the gitlink rule are applied.
fn load_both(
    repo: &ReadRepo,
    op: &'static str,
    old: Side<'_>,
    new: Side<'_>,
    max_blob_bytes: u64,
) -> Result<Sides, GitError> {
    let old = load(repo, op, old, max_blob_bytes)?;
    let new = load(repo, op, new, max_blob_bytes)?;
    let (old, new) = match (&old, &new) {
        (Loaded::Content(o), Loaded::Content(n)) => (o.as_ref(), n.as_ref()),
        (Loaded::Gitlink, _) | (_, Loaded::Gitlink) => return Ok(Sides::Gitlink),
        _ => return Ok(Sides::OverCap),
    };
    // A NUL byte is git's own binary heuristic, and a binary file has no line
    // count worth reporting and no hunks worth rendering.
    if old.contains(&0) || new.contains(&0) {
        return Ok(Sides::Binary);
    }
    Ok(Sides::Text {
        old: String::from_utf8_lossy(old).into_owned(),
        new: String::from_utf8_lossy(new).into_owned(),
    })
}

/// Intern both sides as lines.
///
/// `lines` tokenizes on line boundaries and keeps each line's terminator in
/// the token, so a "token" here is a line, the counts are line counts — the
/// same unit `git diff --numstat` reports — and a file that lost its final
/// newline differs from one that kept it.
fn intern<'a>(
    old: &'a str,
    new: &'a str,
) -> gix_imara_diff::InternedInput<&'a str> {
    gix_imara_diff::InternedInput::new(
        gix_imara_diff::sources::lines(old),
        gix_imara_diff::sources::lines(new),
    )
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
        Side::Gitlink => return Ok(Loaded::Gitlink),
        Side::Object(oid) => oid,
    };
    let header = repo
        .objects()
        .header(oid)
        .map_err(|e| GitError::repository(op, "reading an object header", repo.git_dir(), e))?;
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

// ═══════════════════════════════════════════════════════════════════════════
// Hunks (the `textdiff` feature)
// ═══════════════════════════════════════════════════════════════════════════

/// How many bytes of a section heading git's default rule keeps
/// (`funcbuf[80]` in xdiff's `xemit.c`), before trailing whitespace is
/// removed.
#[cfg(feature = "textdiff")]
const MAX_SECTION_BYTES: usize = 80;

/// What [`line_hunks`] was asked for.
#[cfg(feature = "textdiff")]
pub(crate) struct HunkOptions {
    /// Context lines around each change — `--context`, default 3.
    pub context: u32,
    /// The embedder's `max_hunk_bytes_per_file`, bounding the total bytes of
    /// hunk line text one file may produce.
    pub max_bytes: u64,
}

/// What building one changed path's hunks produced.
///
/// The same four outcomes [`LineDelta`] has, for the same reasons, with the
/// hunks carried alongside the counts on the one that has both.
#[cfg(feature = "textdiff")]
pub(crate) enum HunkOutcome {
    /// Both sides were read and diffed.
    Counted {
        /// Lines added, the same number [`line_delta`] reports.
        added: u64,
        /// Lines deleted, the same number [`line_delta`] reports.
        deleted: u64,
        /// The hunks that fit under `max_bytes`.
        hunks: Vec<DiffHunk>,
        /// Whether `max_bytes` stopped the hunks short. The counts stay
        /// exact — they are a property of the diff, not of what was emitted.
        capped: bool,
    },
    /// A side was over `max_blob_bytes`, so nothing was read.
    OverCap,
    /// Binary content on at least one side.
    Binary,
    /// A submodule gitlink.
    Gitlink,
}

/// The added and deleted line counts for one changed path, **and** its hunks.
///
/// One `gix-imara-diff` run per file, the same one [`line_delta`] would have
/// made on the same content — `--patch` does not diff a file the counting
/// path would have skipped, and does not diff any file twice. What it adds is
/// `postprocess_lines`, git's own indent heuristic, which moves a slider to
/// where git would put it and costs one linear pass.
#[cfg(feature = "textdiff")]
pub(crate) fn line_hunks(
    repo: &ReadRepo,
    op: &'static str,
    old: Side<'_>,
    new: Side<'_>,
    max_blob_bytes: u64,
    opts: &HunkOptions,
) -> Result<HunkOutcome, GitError> {
    let (old, new) = match load_both(repo, op, old, new, max_blob_bytes)? {
        Sides::Text { old, new } => (old, new),
        Sides::OverCap => return Ok(HunkOutcome::OverCap),
        Sides::Binary => return Ok(HunkOutcome::Binary),
        Sides::Gitlink => return Ok(HunkOutcome::Gitlink),
    };
    let input = intern(&old, &new);
    let mut diff = gix_imara_diff::Diff::compute(gix_imara_diff::Algorithm::Myers, &input);
    // git runs the indent heuristic by default (`diff.indentHeuristic`, on
    // since 2.14), and it decides *where* an ambiguous run of added lines is
    // placed. Skipping it would put our hunks a line or two off git's for the
    // same change while reporting the same counts.
    diff.postprocess_lines(&input);
    let (hunks, capped) = assemble(&input, &diff, opts);
    Ok(HunkOutcome::Counted {
        added: u64::from(diff.count_additions()),
        deleted: u64::from(diff.count_removals()),
        hunks,
        capped,
    })
}

/// One group of changes that will be emitted as a single hunk: the old and
/// new line ranges it covers, context included, and the changes inside it.
#[cfg(feature = "textdiff")]
struct Group {
    old: std::ops::Range<u32>,
    new: std::ops::Range<u32>,
    changes: Vec<gix_imara_diff::Hunk>,
}

/// Turn a computed diff into hunks, bounded by `opts.max_bytes`.
///
/// The bound is applied to each group **before** its lines are built: a
/// group's byte cost is the sum of its tokens' lengths, which the interner
/// can answer without allocating anything. A cap that trimmed a `Vec` after
/// filling it would have already paid for what it threw away — the defect
/// found in `treewalk.rs` and again in curl's response path.
#[cfg(feature = "textdiff")]
fn assemble(
    input: &gix_imara_diff::InternedInput<&str>,
    diff: &gix_imara_diff::Diff,
    opts: &HunkOptions,
) -> (Vec<DiffHunk>, bool) {
    let old_len = input.before.len() as u32;
    let new_len = input.after.len() as u32;
    let ctx = opts.context;

    // Group the changes. Two changes join one hunk when their context
    // regions touch or overlap — the gap between them is at most `2 * ctx` —
    // which is git's own rule in `xdl_emit_diff`.
    let mut groups: Vec<Group> = Vec::new();
    for change in diff.hunks() {
        let old = change.before.start.saturating_sub(ctx)..(change.before.end + ctx).min(old_len);
        let new = change.after.start.saturating_sub(ctx)..(change.after.end + ctx).min(new_len);
        match groups.last_mut() {
            Some(last) if old.start <= last.old.end => {
                last.old.end = last.old.end.max(old.end);
                last.new.end = last.new.end.max(new.end);
                last.changes.push(change);
            }
            _ => groups.push(Group {
                old,
                new,
                changes: vec![change],
            }),
        }
    }

    let mut hunks = Vec::with_capacity(groups.len());
    let mut used: u64 = 0;
    let mut capped = false;
    // git scans backwards for a section heading only as far as the previous
    // hunk's first line, so the whole file is scanned once across all hunks
    // rather than once per hunk. `-1` before the first hunk means "scan to
    // the start of the file".
    let mut section_limit: i64 = -1;
    for group in &groups {
        let cost = group_bytes(input, group);
        if used.saturating_add(cost) > opts.max_bytes {
            capped = true;
            break;
        }
        used += cost;
        let section = section_for(input, group.old.start, section_limit);
        section_limit = i64::from(group.old.start);
        hunks.push(materialize(input, group, section));
    }
    (hunks, capped)
}

/// What one group's line text will cost, without building any of it.
#[cfg(feature = "textdiff")]
fn group_bytes(input: &gix_imara_diff::InternedInput<&str>, group: &Group) -> u64 {
    let mut total: u64 = 0;
    // Every old-side line in range is emitted once, as context or as a
    // deletion; every new-side line that is an insertion is emitted too. The
    // new-side context lines are the same lines as the old-side context ones
    // and are not emitted twice.
    for token in &input.before[group.old.start as usize..group.old.end as usize] {
        total += input.interner[*token].len() as u64;
    }
    for change in &group.changes {
        for token in &input.after[change.after.start as usize..change.after.end as usize] {
            total += input.interner[*token].len() as u64;
        }
    }
    total
}

/// Build one group's [`DiffHunk`].
#[cfg(feature = "textdiff")]
fn materialize(
    input: &gix_imara_diff::InternedInput<&str>,
    group: &Group,
    section: Option<String>,
) -> DiffHunk {
    let mut lines = Vec::new();
    let mut o = group.old.start;
    let mut n = group.new.start;
    let old_line = |i: u32| -> &str { input.interner[input.before[i as usize]] };
    let new_line = |i: u32| -> &str { input.interner[input.after[i as usize]] };

    for change in &group.changes {
        while o < change.before.start {
            lines.push(line(DiffOp::Context, old_line(o)));
            o += 1;
            n += 1;
        }
        while o < change.before.end {
            lines.push(line(DiffOp::Delete, old_line(o)));
            o += 1;
        }
        while n < change.after.end {
            lines.push(line(DiffOp::Insert, new_line(n)));
            n += 1;
        }
    }
    // Trailing context. `n` is not advanced with it: nothing reads it after
    // the last change, and the new-side count comes from the group's range.
    while o < group.old.end {
        lines.push(line(DiffOp::Context, old_line(o)));
        o += 1;
    }

    let old_lines = group.old.end - group.old.start;
    let new_lines = group.new.end - group.new.start;
    DiffHunk {
        // git's convention for an empty side: the header names the line the
        // hunk *follows*, so a new file reads `@@ -0,0 +1,3 @@`.
        old_start: if old_lines == 0 {
            group.old.start
        } else {
            group.old.start + 1
        },
        old_lines,
        new_start: if new_lines == 0 {
            group.new.start
        } else {
            group.new.start + 1
        },
        new_lines,
        section,
        lines,
    }
}

/// One line, with its terminator taken off and recorded.
#[cfg(feature = "textdiff")]
fn line(op: DiffOp, raw: &str) -> DiffLine {
    match raw.strip_suffix('\n') {
        Some(text) => DiffLine {
            op,
            text: text.to_string(),
            no_newline: false,
        },
        // The last line of a file that does not end in a newline. Patch text
        // marks it `\ No newline at end of file`; a CR is content and stays.
        None => DiffLine {
            op,
            text: raw.to_string(),
            no_newline: true,
        },
    }
}

/// The section heading for a hunk starting at old line `start`, by git's
/// default rule: the nearest preceding line whose first character is an ASCII
/// letter, `_` or `$`, truncated to [`MAX_SECTION_BYTES`] with trailing
/// whitespace removed.
///
/// `limit` is the previous hunk's first line, exclusive — the same bound
/// git's `get_func_line` takes, which is what keeps the total scan linear in
/// the file rather than quadratic in the hunk count. `-1` scans to the start.
#[cfg(feature = "textdiff")]
fn section_for(
    input: &gix_imara_diff::InternedInput<&str>,
    start: u32,
    limit: i64,
) -> Option<String> {
    let mut l = i64::from(start) - 1;
    while l > limit && l >= 0 {
        let raw: &str = input.interner[input.before[l as usize]];
        if raw
            .as_bytes()
            .first()
            .is_some_and(|b| b.is_ascii_alphabetic() || *b == b'_' || *b == b'$')
        {
            // Truncate on a character boundary at or below git's byte limit:
            // git counts bytes, and a String cannot be cut inside one.
            let mut end = raw.len().min(MAX_SECTION_BYTES);
            while end > 0 && !raw.is_char_boundary(end) {
                end -= 1;
            }
            let text = raw[..end].trim_end();
            return (!text.is_empty()).then(|| text.to_string());
        }
        l -= 1;
    }
    None
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
