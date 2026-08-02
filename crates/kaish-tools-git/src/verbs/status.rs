//! `git status` — hand-composed on the plumbing (architecture.md B.2, A.2).
//!
//! `gix-status` is disqualified: it hard-requires `gix-filter`, which pulls
//! `gix-command`, the spawn machinery the tripwires forbid (confirmed in PR 0).
//! So status is composed here, from three halves the design names:
//!
//! - **Staged** — the HEAD tree against the index (stage 0). gix has no
//!   tree↔index diff and `gix-diff`'s rename tracker is `blob`-gated (→
//!   `gix-command`), so the two sides are flattened to `path → (oid, mode)`
//!   maps and compared directly. Renames are **exact-match only** (a blob oid
//!   reappearing at a new path); a modified-then-moved file has a new oid and
//!   is reported honestly as a delete plus an add, never as a scored rename.
//! - **Unstaged** — the index against the working tree, by hashing worktree
//!   content and comparing oids (not the racy-clean stat cache, which we never
//!   persist — that is what keeps the `.git` fingerprint test green, D.4).
//! - **Untracked / ignored** — a worktree walk driven by `gix-worktree`'s
//!   ignore `Stack` (`gix-ignore` / `gix-glob` under it), honoring
//!   `--untracked` depth and `--ignored`.
//!
//! Conflicts are index entries at a nonzero stage; each becomes one unmerged
//! entry with the porcelain `XY` git spells for the stages present.
//!
//! The text surface renders git's porcelain `XY` letters; `--json` speaks the
//! self-describing words in [`crate::model`] (decision 9). Both are computed
//! from one [`StatusReport`], so they cannot disagree.

use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::io::ErrorKind;
use std::path::{Component, Path, PathBuf};

use clap::{Parser, ValueEnum};

use gix_object::bstr::{BStr, ByteSlice};

use gix_index::entry::Mode as IndexMode;
use gix_index::hash::ObjectId;
use gix_object::tree::EntryKind as TreeEntryKind;
use gix_object::{FindExt, TreeRefIter};

use kaish_tool_api::GlobalFlags;

use crate::error::GitError;
use crate::model::{EntryKind, EntryStatus, StatusEntry, StatusReport, StatusTotals};
use crate::repo::ReadRepo;

/// How much of the untracked space to report (architecture.md B.2).
#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum, Default)]
#[clap(rename_all = "lower")]
pub(crate) enum Untracked {
    /// Report no untracked files at all.
    No,
    /// Report untracked files, collapsing a wholly-untracked directory to a
    /// single `dir/` row — git's default.
    #[default]
    Normal,
    /// Recurse into untracked directories and report every file.
    All,
}

/// `git status`'s argv surface (architecture.md B.2).
#[derive(Parser, Debug)]
#[command(
    name = "status",
    about = "Show staged, unstaged, untracked, ignored and conflicted paths"
)]
pub(crate) struct StatusArgs {
    /// Untracked reporting depth: `no`, `normal` (default), or `all`.
    #[arg(long = "untracked", value_name = "MODE", default_value = "normal")]
    pub untracked: Untracked,

    /// Include ignored entries.
    #[arg(long = "ignored", default_value_t = false)]
    pub ignored: bool,

    /// Restrict to paths under these literal paths or simple globs (`*`, `**`,
    /// `?`). Repeatable. Git pathspec magic (`:(exclude)`, `:!`, `:/`) is a
    /// loud usage error, never silently matched.
    #[arg(long = "path", value_name = "PATH")]
    pub path: Vec<String>,

    /// Maximum entries to report. Truncation is always reported
    /// (`truncated: true` and a stderr note), never silent.
    #[arg(long = "limit", value_name = "N", default_value_t = 1000)]
    pub limit: usize,

    /// Repository to inspect. Defaults to the current directory; discovery
    /// searches upward, never past the mount root that contains the path.
    #[arg(long = "repo", value_name = "PATH")]
    pub repo: Option<String>,

    #[command(flatten)]
    pub global: GlobalFlags,

    /// Validation-only sink: `ToolArgs::to_argv()` always emits `--` before
    /// positionals, and `status` takes none. Read nothing off this field.
    #[arg(hide = true)]
    pub operands: Vec<String>,
}

/// Everything `run` needs, decoupled from clap so `verbs::status::run` stays a
/// plain function testable without a `ToolCtx`.
pub(crate) struct StatusOptions {
    /// Untracked reporting depth.
    pub untracked: Untracked,
    /// Whether to include ignored entries.
    pub ignored: bool,
    /// Repo-relative path/glob filters (already prefixed by the caller's cwd).
    pub paths: Vec<String>,
    /// The effective row cap: the smaller of `--limit` and the embedder's cap.
    pub limit: usize,
    /// The embedder's `max_blob_bytes`. Status hashes every tracked file's
    /// content, so this is what stands between a repository and an allocation
    /// it chose.
    pub max_blob_bytes: u64,
}

// ═══════════════════════════════════════════════════════════════════════════
// The porcelain XY letters and their JSON words
// ═══════════════════════════════════════════════════════════════════════════

/// One column's change, carrying both its porcelain letter and its JSON word.
///
/// The single source both surfaces render from: the text `XY` pair reads the
/// letters, `--json` reads the words. They cannot drift because they are the
/// same value.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Code {
    Unmodified,
    Added,
    Modified,
    Deleted,
    Renamed,
    Typechange,
    Untracked,
    Ignored,
    /// An unmerged column, rendered `U`. Its word is `modified` — the word set
    /// (B.2) has no `unmerged`, and the `conflicted` boolean is the real
    /// signal; the word still says "this side changed".
    Unmerged,
}

impl Code {
    /// The git porcelain letter for this column.
    fn letter(self) -> char {
        match self {
            Code::Unmodified => ' ',
            Code::Added => 'A',
            Code::Modified => 'M',
            Code::Deleted => 'D',
            Code::Renamed => 'R',
            Code::Typechange => 'T',
            Code::Untracked => '?',
            Code::Ignored => '!',
            Code::Unmerged => 'U',
        }
    }

    /// The self-describing JSON word for this column.
    fn word(self) -> EntryStatus {
        match self {
            Code::Unmodified => EntryStatus::None,
            Code::Added => EntryStatus::Added,
            // `Unmerged` has no word of its own; `modified` plus the
            // `conflicted` flag carries it.
            Code::Modified | Code::Unmerged => EntryStatus::Modified,
            Code::Deleted => EntryStatus::Deleted,
            Code::Renamed => EntryStatus::Renamed,
            Code::Typechange => EntryStatus::Typechange,
            Code::Untracked => EntryStatus::Untracked,
            Code::Ignored => EntryStatus::Ignored,
        }
    }
}

/// One entry, mid-composition, before it becomes a [`StatusEntry`].
#[derive(Debug, Clone)]
struct Building {
    index: Code,
    worktree: Code,
    orig_path: Option<String>,
    kind: EntryKind,
    conflicted: bool,
}

impl Building {
    fn empty(kind: EntryKind) -> Self {
        Building {
            index: Code::Unmodified,
            worktree: Code::Unmodified,
            orig_path: None,
            kind,
            conflicted: false,
        }
    }

    /// Whether this entry carries a staged change.
    fn is_staged(&self) -> bool {
        !matches!(
            self.index,
            Code::Unmodified | Code::Untracked | Code::Ignored
        )
    }

    /// Whether this entry carries an unstaged change.
    fn is_unstaged(&self) -> bool {
        !matches!(
            self.worktree,
            Code::Unmodified | Code::Untracked | Code::Ignored
        )
    }

    fn is_untracked(&self) -> bool {
        self.index == Code::Untracked || self.worktree == Code::Untracked
    }

    fn is_ignored(&self) -> bool {
        self.index == Code::Ignored || self.worktree == Code::Ignored
    }

    fn finish(self, path: String) -> StatusEntry {
        let porcelain = [self.index.letter(), self.worktree.letter()];
        StatusEntry {
            path,
            orig_path: self.orig_path,
            kind: self.kind,
            index: self.index.word(),
            worktree: self.worktree.word(),
            conflicted: self.conflicted,
            porcelain,
        }
    }
}

// ═══════════════════════════════════════════════════════════════════════════
// Normalized item classes, for comparing tree modes to index modes
// ═══════════════════════════════════════════════════════════════════════════

/// A file's type, normalized so a tree entry and an index entry compare on the
/// same axis. The executable bit is a class of its own because git treats a
/// mode flip (`100644` ↔ `100755`) as a modification.
///
/// `Ord` so a rename candidate can be keyed by `(oid, class)`: pairing across
/// classes is what fabricates a rename out of a deleted symlink and an added
/// file that happen to share a blob.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
enum Class {
    File,
    Exec,
    Symlink,
    Commit,
}

impl Class {
    fn from_tree(kind: TreeEntryKind) -> Option<Class> {
        Some(match kind {
            TreeEntryKind::Blob => Class::File,
            TreeEntryKind::BlobExecutable => Class::Exec,
            TreeEntryKind::Link => Class::Symlink,
            TreeEntryKind::Commit => Class::Commit,
            TreeEntryKind::Tree => return None,
        })
    }

    fn from_index(mode: IndexMode) -> Option<Class> {
        Some(match mode {
            IndexMode::FILE => Class::File,
            IndexMode::FILE_EXECUTABLE => Class::Exec,
            IndexMode::SYMLINK => Class::Symlink,
            IndexMode::COMMIT => Class::Commit,
            _ => return None,
        })
    }

    fn kind(self) -> EntryKind {
        match self {
            Class::File | Class::Exec => EntryKind::File,
            Class::Symlink => EntryKind::Symlink,
            Class::Commit => EntryKind::Commit,
        }
    }

    /// Whether two classes differ in *type* (file↔symlink↔submodule), which is
    /// a typechange, as opposed to only in the executable bit, which is a
    /// modification.
    fn is_typechange_from(self, other: Class) -> bool {
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
// The composition
// ═══════════════════════════════════════════════════════════════════════════

/// Compose a status report for `repo` under `opts` (architecture.md B.2).
pub(crate) fn run(repo: &ReadRepo, opts: &StatusOptions) -> Result<StatusReport, GitError> {
    const OP: &str = "status";

    let head = repo.head()?;
    let Some(work_dir) = repo.work_dir().map(Path::to_path_buf) else {
        return Err(GitError::NeedsWorktree {
            operation: OP,
            repo: repo.git_dir().to_path_buf(),
        });
    };

    let filter = PathFilter::parse(OP, &opts.paths)?;

    // The index, and its stage-0 map plus the conflict map.
    let index = repo.open_index()?;
    let mut idx0: BTreeMap<String, (ObjectId, Class)> = BTreeMap::new();
    // path -> ([stage1, stage2, stage3], class-of-highest-stage)
    let mut conflicts: BTreeMap<String, ([bool; 3], Class)> = BTreeMap::new();
    let mut tracked: BTreeSet<String> = BTreeSet::new();
    if let Some(index) = &index {
        for entry in index.entries() {
            let path = entry.path(index).to_string();
            // The lexical half of the index-path screen (E.2). Every path here
            // is about to be joined onto the working tree, and the join is a
            // string operation: an entry that is absolute or carries `.`,
            // `..` or an empty component leaves the tree before anything is
            // stat'ed. Git will not write such an entry, so an index that has
            // one was not written by git — the honest answer is to refuse the
            // whole operation rather than skip a row and report the rest as if
            // the repository were sound. Non-echoing: the entry is repository
            // content, and repeating it back is an oracle.
            if !is_repo_relative(&path) {
                return Err(GitError::EscapesMount {
                    operation: OP,
                    what: ESCAPING_INDEX_ENTRY,
                    repo: work_dir.clone(),
                    ceiling: repo.ceiling().to_path_buf(),
                });
            }
            tracked.insert(path.clone());
            let Some(class) = Class::from_index(entry.mode) else {
                // A sparse-directory entry (cone mode). This build does not
                // model sparse indexes; skip it rather than mis-report it.
                continue;
            };
            match entry.stage_raw() {
                0 => {
                    idx0.insert(path, (entry.id, class));
                }
                stage @ 1..=3 => {
                    let slot = conflicts.entry(path).or_insert(([false; 3], class));
                    slot.0[(stage - 1) as usize] = true;
                    slot.1 = class;
                }
                _ => {}
            }
        }
    }

    // The HEAD tree, flattened to the same shape as the index map.
    let head_tree = repo.head_tree_id()?;
    let tree_map = flatten_head_tree(repo, head_tree)?;

    let mut entries: BTreeMap<String, Building> = BTreeMap::new();

    // ── Staged half: HEAD tree vs index stage 0 ─────────────────────────────
    stage_the_index(&tree_map, &idx0, &conflicts, &mut entries);

    // ── Unstaged half: index stage 0 vs working tree ────────────────────────
    let mut paths = WorktreePaths::new(OP, &work_dir, repo.ceiling());
    unstage_the_worktree(OP, &mut paths, &idx0, opts, &mut entries)?;

    // ── Untracked and ignored: the worktree walk ────────────────────────────
    walk_untracked_and_ignored(repo, &work_dir, index.as_ref(), &tracked, opts, &mut entries)?;

    // ── Conflicts: one unmerged entry each ──────────────────────────────────
    for (path, (stages, class)) in &conflicts {
        let (x, y) = unmerged_codes(*stages);
        entries.insert(
            path.clone(),
            Building {
                index: x,
                worktree: y,
                orig_path: None,
                kind: class.kind(),
                conflicted: true,
            },
        );
    }

    // ── Filter, total, sort, truncate ───────────────────────────────────────
    entries.retain(|path, _| filter.matches(path));

    let mut totals = StatusTotals::default();
    for b in entries.values() {
        if b.is_staged() {
            totals.staged += 1;
        }
        if b.is_unstaged() {
            totals.unstaged += 1;
        }
        if b.is_untracked() {
            totals.untracked += 1;
        }
        if b.is_ignored() {
            totals.ignored += 1;
        }
        if b.conflicted {
            totals.conflicted += 1;
        }
    }
    let clean = totals.staged == 0
        && totals.unstaged == 0
        && totals.untracked == 0
        && totals.conflicted == 0;

    let mut finished: Vec<StatusEntry> = entries
        .into_iter()
        .map(|(path, b)| b.finish(path))
        .collect();
    // Already sorted: `entries` was a BTreeMap keyed by path.
    let truncated = finished.len() > opts.limit;
    if truncated {
        finished.truncate(opts.limit);
    }

    Ok(StatusReport {
        head,
        entries: finished,
        totals,
        clean,
        truncated,
    })
}

/// Diff the HEAD tree against the index (stage 0), detecting exact renames.
fn stage_the_index(
    tree: &BTreeMap<String, (ObjectId, Class)>,
    idx0: &BTreeMap<String, (ObjectId, Class)>,
    conflicts: &BTreeMap<String, ([bool; 3], Class)>,
    out: &mut BTreeMap<String, Building>,
) {
    // Collect raw additions and deletions first; renames are paired from them.
    let mut added: Vec<(String, ObjectId, Class)> = Vec::new();
    let mut deleted: Vec<(String, ObjectId, Class)> = Vec::new();

    for (path, (oid, class)) in idx0 {
        if conflicts.contains_key(path) {
            continue;
        }
        match tree.get(path) {
            None => added.push((path.clone(), *oid, *class)),
            Some((tree_oid, tree_class)) => {
                let code = if class.is_typechange_from(*tree_class) {
                    Some(Code::Typechange)
                } else if oid != tree_oid || class != tree_class {
                    // oid differs, or only the executable bit flipped.
                    Some(Code::Modified)
                } else {
                    None
                };
                if let Some(code) = code {
                    out.entry(path.clone())
                        .or_insert_with(|| Building::empty(class.kind()))
                        .index = code;
                }
            }
        }
    }
    for (path, (oid, class)) in tree {
        if conflicts.contains_key(path) || idx0.contains_key(path) {
            continue;
        }
        deleted.push((path.clone(), *oid, *class));
    }

    // Exact-match renames: a deleted blob oid reappearing at an added path,
    // *of the same class*. No similarity scoring — that needs `gix-diff`'s
    // `blob` feature, which pulls `gix-command` (A.2). A modified-then-moved
    // file has a different oid, so it never pairs, and is reported as delete +
    // add (the honest limitation, asserted by the rename fixture).
    //
    // The class is part of the key, not an afterthought: a symlink's blob is
    // its target string, so `ln -s hello a` and a file containing `hello`
    // share an oid, and pairing on oid alone reports a rename between two
    // things git would never call one. The candidates are keyed into queues
    // in path order — `deleted` comes out of a `BTreeMap` — so the source a
    // rename claims is the lowest-sorting unclaimed one, the same on every
    // run, rather than whatever a linear scan reached first.
    let mut candidates: BTreeMap<(ObjectId, Class), VecDeque<usize>> = BTreeMap::new();
    for (i, (_, oid, class)) in deleted.iter().enumerate() {
        candidates.entry((*oid, *class)).or_default().push_back(i);
    }
    let mut consumed_deletes: BTreeSet<usize> = BTreeSet::new();
    for (add_path, add_oid, add_class) in &added {
        let claimed = candidates
            .get_mut(&(*add_oid, *add_class))
            .and_then(VecDeque::pop_front);
        let Some(di) = claimed else {
            out.entry(add_path.clone())
                .or_insert_with(|| Building::empty(add_class.kind()))
                .index = Code::Added;
            continue;
        };
        consumed_deletes.insert(di);
        let (orig, _, _) = &deleted[di];
        let b = out
            .entry(add_path.clone())
            .or_insert_with(|| Building::empty(add_class.kind()));
        b.index = Code::Renamed;
        b.orig_path = Some(orig.clone());
    }
    for (i, (path, _, class)) in deleted.iter().enumerate() {
        if consumed_deletes.contains(&i) {
            continue;
        }
        out.entry(path.clone())
            .or_insert_with(|| Building::empty(class.kind()))
            .index = Code::Deleted;
    }
}

/// Compare each stage-0 index entry against the working tree.
fn unstage_the_worktree(
    op: &'static str,
    paths: &mut WorktreePaths<'_>,
    idx0: &BTreeMap<String, (ObjectId, Class)>,
    opts: &StatusOptions,
    out: &mut BTreeMap<String, Building>,
) -> Result<(), GitError> {
    for (path, (oid, class)) in idx0 {
        // A submodule's working-tree state is a whole recursive question this
        // build does not answer; report only what the index and HEAD say about
        // the gitlink, never a spurious worktree change.
        if *class == Class::Commit {
            continue;
        }
        let Some(full) = paths.leaf(path)? else {
            // The entry's directory chain is not on disk, so neither is the
            // entry.
            out.entry(path.clone())
                .or_insert_with(|| Building::empty(class.kind()))
                .worktree = Code::Deleted;
            continue;
        };
        let meta = match std::fs::symlink_metadata(&full) {
            Ok(m) => m,
            Err(e) if e.kind() == ErrorKind::NotFound => {
                out.entry(path.clone())
                    .or_insert_with(|| Building::empty(class.kind()))
                    .worktree = Code::Deleted;
                continue;
            }
            Err(e) => {
                return Err(GitError::repository(op, "reading a worktree file", &full, e));
            }
        };

        let fs_class = if meta.file_type().is_symlink() {
            Class::Symlink
        } else if meta.is_file() {
            if is_executable(&meta) {
                Class::Exec
            } else {
                Class::File
            }
        } else {
            // A directory (or something exotic) where the index has a blob.
            // git calls this a typechange.
            out.entry(path.clone())
                .or_insert_with(|| Building::empty(class.kind()))
                .worktree = Code::Typechange;
            continue;
        };

        if class.is_typechange_from(fs_class) {
            out.entry(path.clone())
                .or_insert_with(|| Building::empty(class.kind()))
                .worktree = Code::Typechange;
            continue;
        }

        let content = read_worktree_blob(op, path, &full, &meta, opts.max_blob_bytes)?;
        let fs_oid = gix_object::compute_hash(gix_index::hash::Kind::Sha1, gix_object::Kind::Blob, &content)
            .map_err(|e| GitError::repository(op, "hashing a worktree file", &full, e))?;
        let changed = fs_oid != *oid || *class != fs_class;
        if changed {
            out.entry(path.clone())
                .or_insert_with(|| Building::empty(class.kind()))
                .worktree = Code::Modified;
        }
    }
    Ok(())
}

// ═══════════════════════════════════════════════════════════════════════════
// Index paths, contained (architecture.md E.2)
// ═══════════════════════════════════════════════════════════════════════════

/// What an escaping index entry is called in a refusal. Never the entry
/// itself: the path is repository content, and echoing it turns the refusal
/// into an existence oracle for the host filesystem.
const ESCAPING_INDEX_ENTRY: &str = "working tree path (an index entry)";

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
fn is_repo_relative(rel: &str) -> bool {
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
/// So this generalizes `repo.rs`'s `open_leaf` to arbitrary depth: resolve the
/// entry's whole *parent chain* with `canonicalize`, ceiling-check the result
/// against the canonical working tree, and only then lstat the leaf through
/// the canonical parent. The leaf itself may be a symlink — git tracks
/// symlinks, and their blob is the target string, which
/// [`read_worktree_blob`] reads with `read_link` and never follows.
///
/// Parents are cached because index entries share them heavily: one
/// `canonicalize` per distinct directory rather than one per entry.
struct WorktreePaths<'a> {
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
    fn new(op: &'static str, work_dir: &'a Path, ceiling: &'a Path) -> Self {
        WorktreePaths {
            op,
            work_dir,
            ceiling,
            dirs: BTreeMap::new(),
        }
    }

    /// The host path to lstat `rel` at, or `None` when its directory chain is
    /// not on disk — which is how an entry gets reported as deleted.
    fn leaf(&mut self, rel: &str) -> Result<Option<PathBuf>, GitError> {
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
    fn real_dir(&mut self, dir_rel: &str) -> Result<Option<PathBuf>, GitError> {
        if dir_rel.is_empty() {
            return Ok(Some(self.work_dir.to_path_buf()));
        }
        if let Some(cached) = self.dirs.get(dir_rel) {
            return Ok(cached.clone());
        }
        let candidate = join_repo_relative(self.work_dir, dir_rel);
        let resolved = match std::fs::canonicalize(&candidate) {
            Ok(real) if real.starts_with(self.work_dir) => Some(real),
            Ok(_) => return Err(self.escapes()),
            // The two ways a directory chain is honestly absent: nothing at
            // that name, or an ordinary file where a directory has to be. Both
            // mean the entry is gone from the working tree, which is what the
            // caller is told — the same semantics a missing leaf has always
            // had here. Nothing else is swallowed.
            Err(e) if matches!(e.kind(), ErrorKind::NotFound | ErrorKind::NotADirectory) => None,
            Err(e) => {
                // `candidate` is `work_dir` plus screened ordinary components,
                // so it names nothing the caller cannot already see.
                return Err(GitError::repository(
                    self.op,
                    "resolving a worktree directory",
                    &candidate,
                    e,
                ));
            }
        };
        self.dirs.insert(dir_rel.to_string(), resolved.clone());
        Ok(resolved)
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

/// Walk the working tree for untracked and ignored entries, driven by
/// `gix-worktree`'s ignore `Stack` (architecture.md B.2, A.2).
fn walk_untracked_and_ignored(
    repo: &ReadRepo,
    work_dir: &Path,
    index: Option<&gix_index::State>,
    tracked: &BTreeSet<String>,
    opts: &StatusOptions,
    out: &mut BTreeMap<String, Building>,
) -> Result<(), GitError> {
    const OP: &str = "status";

    // Directory prefixes that contain a tracked path, so the walk knows which
    // directories to descend even though nothing directly in them is tracked.
    let mut tracked_dirs: BTreeSet<String> = BTreeSet::new();
    for path in tracked {
        let mut cur = path.as_str();
        while let Some(slash) = cur.rfind('/') {
            cur = &cur[..slash];
            if !tracked_dirs.insert(cur.to_string()) {
                break;
            }
        }
    }

    // The ignore stack: `info/exclude` as the globals (read through the
    // containment guard), per-directory `.gitignore` read by `gix-worktree` as
    // it descends — the documented residual carve-out.
    let parse = gix_ignore::search::Ignore::default();
    let mut globals = gix_ignore::Search::default();
    let exclude = repo.read_info_exclude()?;
    if !exclude.is_empty() {
        globals.add_patterns_buffer(
            &exclude,
            repo.common_dir().join("info").join("exclude"),
            None,
            parse,
        );
    }
    let ignore = gix_worktree::stack::state::Ignore::new(
        gix_ignore::Search::default(),
        globals,
        None,
        gix_worktree::stack::state::ignore::Source::WorktreeThenIdMappingIfNotSkipped,
        parse,
    );
    let state = gix_worktree::stack::State::IgnoreStack(ignore);

    // `from_state_and_ignore_case` needs an index to seed id-mappings; an empty
    // one is correct when the repository has none yet.
    let empty;
    let index_ref = match index {
        Some(index) => index,
        None => {
            empty = gix_index::State::new(gix_index::hash::Kind::Sha1);
            &empty
        }
    };
    let mut stack = gix_worktree::Stack::from_state_and_ignore_case(
        work_dir,
        false,
        state,
        index_ref,
        index_ref.path_backing(),
    );

    let mut walker = Walker {
        op: OP,
        work_dir,
        objects: repo.objects(),
        tracked,
        tracked_dirs: &tracked_dirs,
        opts,
        stack: &mut stack,
        out,
    };
    walker.walk_dir("")
}

/// The recursive worktree walk. Split out so the borrow of `stack` and `out`
/// is one mutable context threaded through the recursion.
struct Walker<'a> {
    op: &'static str,
    work_dir: &'a Path,
    objects: &'a gix_odb::Handle,
    tracked: &'a BTreeSet<String>,
    tracked_dirs: &'a BTreeSet<String>,
    opts: &'a StatusOptions,
    stack: &'a mut gix_worktree::Stack,
    out: &'a mut BTreeMap<String, Building>,
}

impl Walker<'_> {
    fn walk_dir(&mut self, rel_dir: &str) -> Result<(), GitError> {
        let dir = if rel_dir.is_empty() {
            self.work_dir.to_path_buf()
        } else {
            join_repo_relative(self.work_dir, rel_dir)
        };
        let mut names: Vec<String> = Vec::new();
        for entry in std::fs::read_dir(&dir)
            .map_err(|e| GitError::repository(self.op, "listing a worktree directory", &dir, e))?
        {
            let entry =
                entry.map_err(|e| GitError::repository(self.op, "listing a worktree directory", &dir, e))?;
            // Non-UTF-8 names are outside what this build's path model can
            // carry; skipping is honest — git would show them escaped, and we
            // would rather omit than mangle. (Fixtures are all UTF-8.)
            if let Ok(name) = entry.file_name().into_string() {
                names.push(name);
            }
        }
        // Sorted, so `gix-worktree`'s directory cache is used in order and the
        // output is deterministic.
        names.sort();

        for name in names {
            // Never descend `.git` — neither the main one at the root nor a
            // nested submodule/worktree `.git` deeper down.
            if name == ".git" {
                continue;
            }
            let rel = if rel_dir.is_empty() {
                name.clone()
            } else {
                format!("{rel_dir}/{name}")
            };
            let full = join_repo_relative(self.work_dir, &rel);
            let meta = match std::fs::symlink_metadata(&full) {
                Ok(m) => m,
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => continue,
                Err(e) => {
                    return Err(GitError::repository(self.op, "reading a worktree entry", &full, e))
                }
            };
            let is_dir = meta.is_dir() && !meta.file_type().is_symlink();

            if is_dir {
                self.visit_dir(&rel)?;
            } else {
                self.visit_file(&rel, &meta)?;
            }
        }
        Ok(())
    }

    fn visit_dir(&mut self, rel: &str) -> Result<(), GitError> {
        // A directory whose own path is tracked is a submodule gitlink: the
        // index records it as one entry, not as a tree of files. This build
        // does not descend submodules, so it is neither untracked nor walked —
        // the staged/unstaged halves already spoke for the gitlink.
        if self.tracked.contains(rel) {
            return Ok(());
        }
        let ignored = self.excluded_kind(rel, true)?;
        if ignored {
            if self.opts.ignored {
                // git collapses an ignored directory to a single `dir/` row.
                self.emit(rel, EntryKind::Dir, Code::Ignored);
            }
            // Never descend an ignored directory for untracked entries.
            return Ok(());
        }
        if self.tracked_dirs.contains(rel) {
            // A directory with tracked content: descend to find untracked
            // entries within it.
            return self.walk_dir(rel);
        }
        // A wholly-untracked directory.
        match self.opts.untracked {
            Untracked::No => Ok(()),
            Untracked::Normal => {
                self.emit(rel, EntryKind::Dir, Code::Untracked);
                Ok(())
            }
            Untracked::All => self.walk_dir(rel),
        }
    }

    fn visit_file(&mut self, rel: &str, meta: &std::fs::Metadata) -> Result<(), GitError> {
        if self.tracked.contains(rel) {
            // Tracked: the staged/unstaged halves already spoke for it.
            return Ok(());
        }
        let kind = if meta.file_type().is_symlink() {
            EntryKind::Symlink
        } else {
            EntryKind::File
        };
        if self.excluded_kind(rel, false)? {
            if self.opts.ignored {
                self.emit(rel, kind, Code::Ignored);
            }
        } else if self.opts.untracked != Untracked::No {
            self.emit(rel, kind, Code::Untracked);
        }
        Ok(())
    }

    /// Whether `rel` is ignored, per the `gix-worktree` stack.
    fn excluded_kind(&mut self, rel: &str, is_dir: bool) -> Result<bool, GitError> {
        let mode = is_dir.then_some(gix_index::entry::Mode::DIR);
        let bstr: &BStr = rel.into();
        let platform = self
            .stack
            .at_entry(bstr, mode, self.objects)
            .map_err(|e| GitError::repository(self.op, "consulting .gitignore", self.work_dir, e))?;
        Ok(platform.excluded_kind().is_some())
    }

    /// Record an untracked or ignored entry: both porcelain columns take the
    /// same letter (`??` or `!!`), which is how git spells them.
    fn emit(&mut self, rel: &str, kind: EntryKind, code: Code) {
        self.out.insert(
            rel.to_string(),
            Building {
                index: code,
                worktree: code,
                orig_path: None,
                kind,
                conflicted: false,
            },
        );
    }
}

/// Flatten the HEAD tree to `path → (blob oid, class)`, recursing into
/// subtrees. Sub*trees* themselves are not entries — only their leaves are.
fn flatten_head_tree(
    repo: &ReadRepo,
    tree_id: Option<ObjectId>,
) -> Result<BTreeMap<String, (ObjectId, Class)>, GitError> {
    const OP: &str = "status";
    let mut out = BTreeMap::new();
    let Some(tree_id) = tree_id else {
        // Unborn HEAD: the base is the empty tree, so every staged file is an
        // addition. Nothing to flatten.
        return Ok(out);
    };
    flatten_subtree(repo, &tree_id, "", &mut out).map_err(|e| match e {
        FlattenError::Git(e) => e,
        FlattenError::Decode(path, msg) => GitError::repository(
            OP,
            format!("decoding tree at '{path}'"),
            repo.git_dir(),
            std::io::Error::other(msg),
        ),
    })?;
    Ok(out)
}

enum FlattenError {
    Git(GitError),
    Decode(String, String),
}

fn flatten_subtree(
    repo: &ReadRepo,
    tree_id: &ObjectId,
    prefix: &str,
    out: &mut BTreeMap<String, (ObjectId, Class)>,
) -> Result<(), FlattenError> {
    const OP: &str = "status";
    // Own the tree bytes and its entries before recursing, so the borrow of the
    // scratch buffer ends before the next `find`.
    let mut buf = Vec::new();
    let data = repo
        .objects()
        .find(tree_id, &mut buf)
        .map_err(|e| FlattenError::Git(GitError::repository(OP, "reading a tree", repo.git_dir(), e)))?;
    let mut children: Vec<(ObjectId, String, gix_object::tree::EntryMode)> = Vec::new();
    let mut subtrees: Vec<(ObjectId, String)> = Vec::new();
    for entry in TreeRefIter::from_bytes(data.data, gix_index::hash::Kind::Sha1) {
        let entry = entry.map_err(|e| FlattenError::Decode(prefix.to_string(), e.to_string()))?;
        let name = entry.filename.to_str_lossy().into_owned();
        let path = if prefix.is_empty() {
            name
        } else {
            format!("{prefix}/{name}")
        };
        if entry.mode.is_tree() {
            subtrees.push((entry.oid.to_owned(), path));
        } else {
            children.push((entry.oid.to_owned(), path, entry.mode));
        }
    }
    drop(buf);

    for (oid, path, mode) in children {
        if let Some(class) = Class::from_tree(mode.kind()) {
            out.insert(path, (oid, class));
        }
    }
    for (oid, path) in subtrees {
        flatten_subtree(repo, &oid, &path, out)?;
    }
    Ok(())
}

/// The porcelain `XY` codes for an unmerged path, from which stages are
/// present (git's unmerged table).
fn unmerged_codes(stages: [bool; 3]) -> (Code, Code) {
    let [s1, s2, s3] = stages;
    match (s1, s2, s3) {
        (true, true, true) => (Code::Unmerged, Code::Unmerged), // UU both modified
        (false, true, true) => (Code::Added, Code::Added),       // AA both added
        (true, true, false) => (Code::Unmerged, Code::Deleted),  // UD deleted by them
        (true, false, true) => (Code::Deleted, Code::Unmerged),  // DU deleted by us
        (false, true, false) => (Code::Added, Code::Unmerged),   // AU added by us
        (false, false, true) => (Code::Unmerged, Code::Added),   // UA added by them
        (true, false, false) => (Code::Deleted, Code::Deleted),  // DD both deleted
        // No stages at all cannot be a conflict; fall back to something inert.
        (false, false, false) => (Code::Unmodified, Code::Unmodified),
    }
}

// ═══════════════════════════════════════════════════════════════════════════
// Path filtering (literal paths and simple globs; no pathspec magic)
// ═══════════════════════════════════════════════════════════════════════════

/// A `--path` filter: literal paths and simple globs, resolved repo-relative.
struct PathFilter {
    specs: Vec<Spec>,
}

enum Spec {
    /// A literal path: matches itself and anything under it.
    Literal(String),
    /// A glob, matched against the whole repo-relative path.
    Glob(gix_glob::Pattern),
}

impl PathFilter {
    fn parse(op: &'static str, paths: &[String]) -> Result<Self, GitError> {
        let mut specs = Vec::new();
        for raw in paths {
            // Git pathspec magic is a loud usage error, never silently matched
            // (B). A leading `:` is the entry point to every magic form.
            if let Some(magic) = pathspec_magic(raw) {
                return Err(GitError::PathspecMagic {
                    operation: op,
                    spec: raw.clone(),
                    magic,
                });
            }
            let normalized = raw.trim_start_matches('/').trim_end_matches('/');
            if normalized.is_empty() {
                continue;
            }
            if normalized.contains(['*', '?', '[']) {
                match gix_glob::Pattern::from_bytes(normalized.as_bytes()) {
                    Some(pattern) => specs.push(Spec::Glob(pattern)),
                    None => {
                        return Err(GitError::Usage {
                            operation: op,
                            message: format!("path '{raw}' is not a usable glob"),
                        })
                    }
                }
            } else {
                specs.push(Spec::Literal(normalized.to_string()));
            }
        }
        Ok(PathFilter { specs })
    }

    fn matches(&self, path: &str) -> bool {
        if self.specs.is_empty() {
            return true;
        }
        self.specs.iter().any(|spec| match spec {
            Spec::Literal(lit) => {
                path == lit || path.starts_with(&format!("{lit}/"))
            }
            Spec::Glob(pattern) => {
                let bstr: &BStr = path.into();
                let basename = path.rfind('/').map(|p| p + 1);
                pattern.matches_repo_relative_path(
                    bstr,
                    basename,
                    Some(false),
                    gix_glob::pattern::Case::Sensitive,
                    gix_glob::wildmatch::Mode::empty(),
                )
            }
        })
    }
}

/// Recognize git pathspec magic, returning the offending token if present.
fn pathspec_magic(spec: &str) -> Option<String> {
    if let Some(rest) = spec.strip_prefix(':') {
        // `:(...)` long form, `:!`/`:^` exclude, `:/` from-root, or the
        // short magic signature letters — any leading colon is magic here.
        let token = if rest.starts_with('(') {
            let end = rest.find(')').map(|e| e + 1).unwrap_or(rest.len());
            format!(":{}", &rest[..end])
        } else if let Some(c) = rest.chars().next() {
            format!(":{c}")
        } else {
            ":".to_string()
        };
        return Some(token);
    }
    None
}

// ═══════════════════════════════════════════════════════════════════════════
// Small filesystem helpers
// ═══════════════════════════════════════════════════════════════════════════

/// Join a slash-separated repo-relative path onto a real base directory.
fn join_repo_relative(base: &Path, rel: &str) -> std::path::PathBuf {
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
fn read_worktree_blob(
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
fn is_executable(meta: &std::fs::Metadata) -> bool {
    use std::os::unix::fs::PermissionsExt;
    meta.permissions().mode() & 0o111 != 0
}

#[cfg(not(unix))]
fn is_executable(_meta: &std::fs::Metadata) -> bool {
    // Filesystems without a POSIX executable bit cannot report one; git makes
    // the same assumption via `core.fileMode = false`.
    false
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unmerged_table_matches_git_letters() {
        let cases = [
            ([true, true, true], ('U', 'U')),
            ([false, true, true], ('A', 'A')),
            ([true, true, false], ('U', 'D')),
            ([true, false, true], ('D', 'U')),
            ([false, true, false], ('A', 'U')),
            ([false, false, true], ('U', 'A')),
            ([true, false, false], ('D', 'D')),
        ];
        for (stages, (x, y)) in cases {
            let (cx, cy) = unmerged_codes(stages);
            assert_eq!((cx.letter(), cy.letter()), (x, y), "stages {stages:?}");
        }
    }

    /// Every non-unmerged column letter must round-trip to a word and back to
    /// the same letter — the property the text/JSON cross-check rests on.
    #[test]
    fn letters_and_words_agree() {
        for code in [
            Code::Unmodified,
            Code::Added,
            Code::Modified,
            Code::Deleted,
            Code::Renamed,
            Code::Typechange,
            Code::Untracked,
            Code::Ignored,
        ] {
            let word = code.word();
            let back = word_to_letter(word);
            assert_eq!(code.letter(), back, "{code:?} letter/word disagree");
        }
    }

    /// The inverse of `Code::word` for the letters a non-conflict entry can
    /// carry — used by the cross-check test to prove a rendered letter matches
    /// its JSON word.
    fn word_to_letter(word: EntryStatus) -> char {
        match word {
            EntryStatus::None => ' ',
            EntryStatus::Added => 'A',
            EntryStatus::Modified => 'M',
            EntryStatus::Deleted => 'D',
            EntryStatus::Renamed => 'R',
            EntryStatus::Copied => 'C',
            EntryStatus::Typechange => 'T',
            EntryStatus::Untracked => '?',
            EntryStatus::Ignored => '!',
        }
    }

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

    #[test]
    fn pathspec_magic_is_recognized() {
        assert_eq!(pathspec_magic(":(exclude)src").as_deref(), Some(":(exclude)"));
        assert_eq!(pathspec_magic(":!src").as_deref(), Some(":!"));
        assert_eq!(pathspec_magic(":/").as_deref(), Some(":/"));
        assert_eq!(pathspec_magic("src/lib.rs"), None);
        assert_eq!(pathspec_magic("*.rs"), None);
    }

    #[test]
    fn literal_path_filter_matches_dir_and_contents() {
        let filter = PathFilter::parse("status", &["src".into()]).expect("parses");
        assert!(filter.matches("src"));
        assert!(filter.matches("src/lib.rs"));
        assert!(!filter.matches("README.md"));
        assert!(!filter.matches("srcx"));
    }

    #[test]
    fn empty_filter_matches_everything() {
        let filter = PathFilter::parse("status", &[]).expect("parses");
        assert!(filter.matches("anything/at/all"));
    }
}
