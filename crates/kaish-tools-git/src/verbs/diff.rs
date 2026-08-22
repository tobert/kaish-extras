//! `git diff` — the typed change model, no patch text (architecture.md B.4).
//!
//! **The structure is primary.** An agent that receives `{"path": …,
//! "status": "renamed", "additions": 12}` can decide what to read next
//! without parsing anything; an agent handed patch text has to re-derive that
//! structure from a format designed for a pager, and will get renames, mode
//! changes and binary markers slightly wrong. Patch text is a rendering of
//! this model, gated behind `--patch`, and this build does not assemble it
//! yet — `--patch` exits 4 naming the `textdiff` feature (E.5).
//!
//! Five endpoint pairs, chosen by flags rather than by git(1)'s positional
//! grammar, and stated in every result:
//!
//! | Invocation | From | To |
//! |---|---|---|
//! | `git diff` | index | worktree |
//! | `git diff --staged` | `HEAD` | index |
//! | `git diff --from <A>` | `A` | worktree |
//! | `git diff --to <B>` | `HEAD` | `B` |
//! | `git diff --from <A> --to <B>` | `A` | `B` |
//!
//! Bare `git diff` is git parity — unstaged changes only — decided on a
//! five-model survey that was unanimous about what the short spelling means
//! (B.4). `A..B` range syntax is not accepted: `--from`/`--to` is the one
//! spelling, and two spellings for one concept is drift.
//!
//! Both sides are flattened to `path → (oid, class)` and compared directly,
//! which is also what `--staged` does. F.4 planned to build the tree's index
//! in memory and diff tree↔tree instead, on the premise that gix has no
//! tree↔index diff; it does not, but neither does this need one — the same
//! flatten-and-compare `status`'s staged half has used since PR 2 answers
//! HEAD↔index directly, with `git diff --staged --name-status` as its oracle.

use std::collections::{BTreeMap, BTreeSet};

use clap::Parser;

use gix_index::hash::ObjectId;

use kaish_tool_api::GlobalFlags;

use crate::diffcore::{self, Class, LineDelta, PathMap, Side};
use crate::error::GitError;
use crate::model::{DiffEndpoint, DiffFile, DiffReport, DiffTotals, EntryStatus};
use crate::pathfilter::PathFilter;
use crate::repo::ReadRepo;
use crate::worktree::{read_worktree_blob, WorktreePaths};

/// The revision `--to <B>` compares against when `--from` is absent.
pub(crate) const DEFAULT_FROM_REV: &str = "HEAD";

/// `git diff`'s argv surface (architecture.md B.4).
#[derive(Parser, Debug)]
#[command(name = "diff", about = "Compare two ends of a repository — which files changed, and by how much")]
pub(crate) struct DiffArgs {
    /// Compare `HEAD` against the index instead of the index against the
    /// working tree — the staged changes, git's own `--staged`. Cannot be
    /// combined with `--from` or `--to`.
    #[arg(long = "staged", default_value_t = false, conflicts_with_all = ["from", "to"])]
    pub staged: bool,

    /// Compare from this revision. Alone it compares against the working
    /// tree; with `--to` it compares two revisions. `HEAD`, a branch, a tag,
    /// an oid, and the `~N` / `^N` suffixes are the whole grammar — `A..B` is
    /// refused, because `--from A --to B` is the one spelling for a range.
    #[arg(long = "from", value_name = "REV")]
    pub from: Option<String>,

    /// Compare to this revision. Alone it compares `HEAD` to it.
    #[arg(long = "to", value_name = "REV")]
    pub to: Option<String>,

    /// Restrict the comparison to these paths. Repeatable. A literal path
    /// matches itself and everything under it; `*`, `**`, `?` and `[...]`
    /// are globs over the whole repo-relative path. Git pathspec magic
    /// (`:(exclude)`, `:!`, `:/`) is refused by name, never matched as a
    /// literal path.
    #[arg(long = "path", value_name = "PATH")]
    pub path: Vec<String>,

    /// Report paths and statuses only. Nothing is read from the object store
    /// or the working tree beyond what naming the change needs, so
    /// `additions`, `deletions` and `binary` come back `null` rather than
    /// zero.
    #[arg(long = "name-only", default_value_t = false)]
    pub name_only: bool,

    /// Include the unified patch text. **This build exits 4**: assembling
    /// hunks is the `textdiff` feature, built in a later phase. The
    /// changed-file and line counts this verb reports by default need no
    /// flag.
    #[arg(long = "patch", default_value_t = false)]
    pub patch: bool,

    /// Context lines around each hunk, default 3. Only `--patch` output has
    /// hunks, so passing this exits 4 for the same reason `--patch` does.
    #[arg(long = "context", value_name = "N")]
    pub context: Option<usize>,

    /// Pair a deleted path with an added path carrying the same content.
    /// **This is already what happens** — rename pairing is on unless
    /// `--no-find-renames` turns it off — so the flag only makes the choice
    /// explicit. **Exact match only**: a rename is a blob oid reappearing at
    /// a new path, so `similarity` is always 100, and a file that was edited
    /// *and* moved is reported as a delete plus an add where git would fold
    /// the pair. Copy detection does not exist here.
    #[arg(long = "find-renames", conflicts_with = "no_find_renames")]
    pub find_renames: bool,

    /// Report a moved file as a delete plus an add, without pairing them.
    /// Rename pairing is on by default, and this is the only way off.
    #[arg(long = "no-find-renames")]
    pub no_find_renames: bool,

    /// Maximum files to report, default 500. Line counts are computed only
    /// for the files that fit, so this bounds the reading too. Truncation is
    /// always reported (`truncated: true` and a stderr note), never silent.
    #[arg(short = 'n', long = "limit", value_name = "N", default_value_t = 500)]
    pub limit: usize,

    /// Repository to inspect. Defaults to the current directory; discovery
    /// searches upward, never past the mount root that contains the path.
    #[arg(long = "repo", value_name = "PATH")]
    pub repo: Option<String>,

    #[command(flatten)]
    pub global: GlobalFlags,

    /// Paths after `--`, and nothing before it: `git diff -- src`. Name
    /// revisions with `--from`/`--to` (`git diff --from HEAD~1 --to HEAD`);
    /// a bare `HEAD` would be ambiguous with a file of that name, so it is
    /// refused rather than guessed.
    // Bound so clap accepts the `--`-terminated tail `ToolArgs::to_argv()`
    // always emits. The real operands are read off `args.positional` in
    // `tool.rs` — the kernel's own convention, because `to_argv` inserts a
    // `--` of its own and clap cannot tell it from the caller's. Do not read
    // this field; it cannot distinguish them either.
    #[arg(hide = true)]
    pub operands: Vec<String>,
}

/// Which pair of ends to compare (architecture.md B.4's table).
pub(crate) enum Endpoints {
    /// Bare `git diff`.
    IndexToWorktree,
    /// `git diff --staged`.
    HeadToIndex,
    /// `git diff --from <A>`.
    RevToWorktree { from: String },
    /// `git diff [--from <A>] --to <B>`.
    RevToRev { from: String, to: String },
}

/// Everything `run` needs, decoupled from clap so `verbs::diff::run` stays a
/// plain function testable without a `ToolCtx`.
pub(crate) struct DiffOptions {
    /// The pair of ends to compare.
    pub endpoints: Endpoints,
    /// The `--path` filters, unparsed.
    pub paths: Vec<String>,
    /// Whether to skip line counting entirely.
    pub name_only: bool,
    /// Whether to pair exact renames.
    pub find_renames: bool,
    /// The effective file cap: the smaller of `--limit` and the embedder's
    /// `max_diff_files`.
    pub limit: usize,
    /// The embedder's `max_blob_bytes`, bounding each side that is read.
    pub max_blob_bytes: u64,
}

/// Compose a diff for `repo` under `opts` (architecture.md B.4).
pub(crate) fn run(repo: &ReadRepo, opts: &DiffOptions) -> Result<DiffReport, GitError> {
    const OP: &str = "diff";

    let filter = PathFilter::parse(OP, &opts.paths)?;

    // Both ends, and where each one's content will come from. Resolving the
    // revisions first means a bad `--from` fails before anything is read.
    // A bare repository has no index and no working tree, so three of the
    // five endpoint pairs cannot be answered there at all. Refusing by name
    // beats the alternative: with no index file to read, HEAD↔index would
    // compare a real tree against an empty map and report every tracked file
    // as deleted — a confident wrong answer. Git refuses the same way ("this
    // operation must be run in a work tree").
    if !matches!(opts.endpoints, Endpoints::RevToRev { .. }) && repo.work_dir().is_none() {
        return Err(GitError::NeedsWorktree {
            operation: OP,
            repo: repo.git_dir().to_path_buf(),
        });
    }

    let (from_end, to_end) = describe_endpoints(repo, &opts.endpoints)?;
    let mut unmerged = BTreeSet::new();

    let mut old = match &opts.endpoints {
        Endpoints::IndexToWorktree => {
            let (map, conflicted) = index_side(repo, &filter)?;
            unmerged = conflicted;
            SideMap::objects(map)
        }
        Endpoints::HeadToIndex => SideMap::objects(tree_side(repo, repo.head_tree_id()?, &filter)?),
        Endpoints::RevToWorktree { from } | Endpoints::RevToRev { from, .. } => {
            SideMap::objects(tree_side(repo, Some(resolve_tree(repo, from)?), &filter)?)
        }
    };

    let mut new = match &opts.endpoints {
        Endpoints::HeadToIndex => {
            let (map, conflicted) = index_side(repo, &filter)?;
            unmerged = conflicted;
            SideMap::objects(map)
        }
        Endpoints::RevToRev { to, .. } => {
            SideMap::objects(tree_side(repo, Some(resolve_tree(repo, to)?), &filter)?)
        }
        // The working tree, hashed. Only tracked paths are candidates — an
        // untracked file is not part of any diff, which is why `git diff`
        // says nothing about a file removed from the index and still on disk.
        Endpoints::IndexToWorktree | Endpoints::RevToWorktree { .. } => {
            let (index, conflicted) = index_side(repo, &filter)?;
            unmerged = conflicted;
            worktree_side(repo, opts, &index)?
        }
    };

    // An unmerged path is dropped from **both** sides, not just from the one
    // that lacks a stage 0. Leaving it on the other side would report a
    // conflicted file as deleted, which is a wrong answer wearing a normal
    // status — worse than the omission, which the count and the stderr note
    // declare. Git reports a `U` row here instead; this surface has no
    // unmerged row shape (B.4's file has no `conflicted` field, B.2's entry
    // does), and inventing one is a model change, not a same-PR fix
    // (docs/issues.md, D2).
    for path in &unmerged {
        old.entries.remove(path);
        new.entries.remove(path);
        new.foreign.remove(path);
    }

    let (files, truncated) = compare(repo, opts, &old, &new)?;

    let mut totals = DiffTotals {
        files: files.len(),
        additions: if opts.name_only { None } else { Some(0) },
        deletions: if opts.name_only { None } else { Some(0) },
        lines_capped: 0,
    };
    for file in &files {
        if let (Some(t), Some(a)) = (totals.additions.as_mut(), file.additions) {
            *t += a;
        }
        if let (Some(t), Some(d)) = (totals.deletions.as_mut(), file.deletions) {
            *t += d;
        }
        if file.lines_capped {
            totals.lines_capped += 1;
        }
    }

    Ok(DiffReport {
        from: from_end,
        to: to_end,
        files,
        totals,
        unmerged: unmerged.len(),
        truncated,
    })
}

/// State both ends the way the result will report them — the revision
/// verbatim as the caller spelled it, beside the oid it resolved to.
fn describe_endpoints(
    repo: &ReadRepo,
    endpoints: &Endpoints,
) -> Result<(DiffEndpoint, DiffEndpoint), GitError> {
    let rev = |spec: &String| -> Result<DiffEndpoint, GitError> {
        Ok(DiffEndpoint::Rev {
            rev: spec.clone(),
            oid: repo.resolve_object(spec)?.to_string(),
        })
    };
    Ok(match endpoints {
        Endpoints::IndexToWorktree => (DiffEndpoint::Index, DiffEndpoint::Worktree),
        Endpoints::HeadToIndex => (
            // An unborn HEAD has no commit to name, and that is a first-class
            // state, not an error: every staged file is then an addition
            // against the empty tree. The endpoint still says `HEAD`, with no
            // oid to report.
            match repo.head_tree_id()? {
                Some(_) => rev(&DEFAULT_FROM_REV.to_string())?,
                None => DiffEndpoint::Rev {
                    rev: DEFAULT_FROM_REV.to_string(),
                    oid: String::new(),
                },
            },
            DiffEndpoint::Index,
        ),
        Endpoints::RevToWorktree { from } => (rev(from)?, DiffEndpoint::Worktree),
        Endpoints::RevToRev { from, to } => (rev(from)?, rev(to)?),
    })
}

/// Resolve a `--from` / `--to` revision to the tree it names.
fn resolve_tree(repo: &ReadRepo, spec: &str) -> Result<ObjectId, GitError> {
    let object = repo.resolve_object(spec)?;
    repo.tree_of_object(object, spec)
}

/// One end of the comparison: every path it holds, with the blob oid and
/// class at that path.
struct SideMap {
    entries: PathMap,
    /// Paths present in the working tree as something that is neither a file
    /// nor a symlink — a directory where the index has a blob. Git calls that
    /// a typechange; empty for an object-backed side.
    foreign: BTreeSet<String>,
    /// Whether content for this side comes from the working tree rather than
    /// the object store. Working-tree content has no oid to report and must
    /// be re-read to be counted.
    from_worktree: bool,
}

impl SideMap {
    fn objects(entries: PathMap) -> Self {
        SideMap {
            entries,
            foreign: BTreeSet::new(),
            from_worktree: false,
        }
    }
}

/// A revision's tree, flattened and filtered.
///
/// The filter is applied here rather than at the end so it bounds the *work*,
/// not just the output: a `--path` that names one directory should not cost a
/// full worktree hash of everything else.
fn tree_side(
    repo: &ReadRepo,
    tree: Option<ObjectId>,
    filter: &PathFilter,
) -> Result<PathMap, GitError> {
    let mut out = BTreeMap::new();
    diffcore::flatten_tree(repo, "diff", tree, &mut out)?;
    out.retain(|path, _| filter.matches(path));
    Ok(out)
}

/// The index at stage 0, flattened and filtered, plus the paths that have no
/// stage 0 because they are unmerged.
fn index_side(
    repo: &ReadRepo,
    filter: &PathFilter,
) -> Result<(PathMap, BTreeSet<String>), GitError> {
    const OP: &str = "diff";
    let mut out = BTreeMap::new();
    let mut unmerged = BTreeSet::new();
    let Some(index) = repo.open_index()? else {
        return Ok((out, unmerged));
    };
    let work_dir = repo.work_dir().unwrap_or_else(|| repo.git_dir());
    for entry in index.entries() {
        let path = entry.path(&index).to_string();
        // The same lexical screen `status` applies, for the same reason: a
        // path this build may join onto the working tree must be one git
        // could have written. Non-echoing — the entry is repository content.
        if !crate::worktree::is_repo_relative(&path) {
            return Err(GitError::EscapesMount {
                operation: OP,
                what: crate::worktree::ESCAPING_INDEX_ENTRY,
                repo: work_dir.to_path_buf(),
                ceiling: repo.ceiling().to_path_buf(),
            });
        }
        if !filter.matches(&path) {
            continue;
        }
        let Some(class) = Class::from_index(entry.mode) else {
            // A sparse-directory entry (cone mode). This build does not model
            // sparse indexes; skip it rather than mis-report it.
            continue;
        };
        match entry.stage_raw() {
            0 => {
                out.insert(path, (entry.id, class));
            }
            1..=3 => {
                unmerged.insert(path);
            }
            _ => {}
        }
    }
    // A path can be both: `git checkout --merge` leaves stages behind. Only a
    // path with no stage 0 at all is missing from the comparison.
    unmerged.retain(|path| !out.contains_key(path));
    Ok((out, unmerged))
}

/// The working tree, hashed at every tracked path the filter kept.
///
/// Only paths the index tracks are candidates, which is the rule `git diff`
/// itself follows: an untracked file is not part of any diff, and a path
/// removed from the index but still on disk reads as deleted rather than as
/// content to compare.
fn worktree_side(
    repo: &ReadRepo,
    opts: &DiffOptions,
    index: &PathMap,
) -> Result<SideMap, GitError> {
    const OP: &str = "diff";
    let Some(work_dir) = repo.work_dir().map(std::path::Path::to_path_buf) else {
        return Err(GitError::NeedsWorktree {
            operation: OP,
            repo: repo.git_dir().to_path_buf(),
        });
    };
    let mut paths = WorktreePaths::new(OP, &work_dir, repo.ceiling());
    let mut side = SideMap::objects(BTreeMap::new());
    side.from_worktree = true;

    for (path, (_, index_class)) in index {
        // A submodule's working-tree state is a whole recursive question this
        // build does not answer; the gitlink is compared by what the index
        // and the tree say about it, never by a spurious worktree read.
        if *index_class == Class::Commit {
            if let Some((oid, class)) = index.get(path) {
                side.entries.insert(path.clone(), (*oid, *class));
            }
            continue;
        }
        // No directory chain on disk means no file: the path reads as
        // deleted, which is exactly its absence from this map.
        let Some(full) = paths.leaf(path)? else {
            continue;
        };
        let meta = match std::fs::symlink_metadata(&full) {
            Ok(m) => m,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => continue,
            Err(e) => return Err(GitError::repository(OP, "reading a worktree file", &full, e)),
        };
        let Some(class) = Class::from_metadata(&meta) else {
            side.foreign.insert(path.clone());
            continue;
        };
        let content = read_worktree_blob(OP, path, &full, &meta, opts.max_blob_bytes)?;
        let oid = gix_object::compute_hash(gix_index::hash::Kind::Sha1, gix_object::Kind::Blob, &content)
            .map_err(|e| GitError::repository(OP, "hashing a worktree file", &full, e))?;
        side.entries.insert(path.clone(), (oid, class));
    }
    Ok(side)
}

/// One path that differs, before its line counts are known.
struct Changed {
    path: String,
    old_path: Option<String>,
    status: EntryStatus,
    old: Option<(ObjectId, Class)>,
    new: Option<(ObjectId, Class)>,
}

/// Compare the two sides, pair renames, cap the result, then count lines for
/// what survived.
///
/// The order matters: `--limit` is applied *before* any blob is read, so it
/// bounds the reading and not only the reporting. A cap that only trimmed the
/// output would have already paid for every file it threw away.
fn compare(
    repo: &ReadRepo,
    opts: &DiffOptions,
    old: &SideMap,
    new: &SideMap,
) -> Result<(Vec<DiffFile>, bool), GitError> {
    let mut changed: BTreeMap<String, Changed> = BTreeMap::new();
    let mut added: Vec<(String, ObjectId, Class)> = Vec::new();
    let mut deleted: Vec<(String, ObjectId, Class)> = Vec::new();

    let paths: BTreeSet<&String> = old
        .entries
        .keys()
        .chain(new.entries.keys())
        .chain(new.foreign.iter())
        .collect();
    for path in paths {
        let o = old.entries.get(path).copied();
        let n = new.entries.get(path).copied();
        // Present in the working tree as neither a file nor a symlink where
        // the other side has a blob: git's typechange.
        if new.foreign.contains(path) {
            changed.insert(
                path.clone(),
                Changed {
                    path: path.clone(),
                    old_path: None,
                    status: EntryStatus::Typechange,
                    old: o,
                    new: None,
                },
            );
            continue;
        }
        match (o, n) {
            (None, None) => {}
            (Some((oid, class)), None) => deleted.push((path.clone(), oid, class)),
            (None, Some((oid, class))) => added.push((path.clone(), oid, class)),
            (Some((old_oid, old_class)), Some((new_oid, new_class))) => {
                let status = if new_class.is_typechange_from(old_class) {
                    EntryStatus::Typechange
                } else if old_oid != new_oid || old_class != new_class {
                    // A different blob, or only the executable bit flipped —
                    // git counts a mode-only change as a changed file too.
                    EntryStatus::Modified
                } else {
                    continue;
                };
                changed.insert(
                    path.clone(),
                    Changed {
                        path: path.clone(),
                        old_path: None,
                        status,
                        old: o,
                        new: n,
                    },
                );
            }
        }
    }

    let pairs = if opts.find_renames {
        diffcore::pair_exact_renames(&added, &deleted)
    } else {
        BTreeMap::new()
    };
    let claimed: BTreeSet<usize> = pairs.values().copied().collect();
    for (ai, (path, oid, class)) in added.iter().enumerate() {
        let (status, old_path, old) = match pairs.get(&ai) {
            None => (EntryStatus::Added, None, None),
            Some(di) => {
                let (src, src_oid, src_class) = &deleted[*di];
                (
                    EntryStatus::Renamed,
                    Some(src.clone()),
                    Some((*src_oid, *src_class)),
                )
            }
        };
        changed.insert(
            path.clone(),
            Changed {
                path: path.clone(),
                old_path,
                status,
                old,
                new: Some((*oid, *class)),
            },
        );
    }
    for (di, (path, oid, class)) in deleted.iter().enumerate() {
        if claimed.contains(&di) {
            continue;
        }
        changed.insert(
            path.clone(),
            Changed {
                path: path.clone(),
                old_path: None,
                status: EntryStatus::Deleted,
                old: Some((*oid, *class)),
                new: None,
            },
        );
    }

    // Sorted by path already — `changed` is a BTreeMap keyed by it.
    let mut rows: Vec<Changed> = changed.into_values().collect();
    let truncated = rows.len() > opts.limit;
    rows.truncate(opts.limit);

    let mut files = Vec::with_capacity(rows.len());
    for row in rows {
        files.push(finish(repo, opts, old, new, row)?);
    }
    Ok((files, truncated))
}

/// Turn one comparison result into its reported row, counting lines unless
/// `--name-only` said not to.
fn finish(
    repo: &ReadRepo,
    opts: &DiffOptions,
    old: &SideMap,
    new: &SideMap,
    row: Changed,
) -> Result<DiffFile, GitError> {
    let oid_of = |side: &SideMap, entry: Option<(ObjectId, Class)>| {
        if side.from_worktree {
            None
        } else {
            entry.map(|(oid, _)| oid.to_string())
        }
    };
    let mut file = DiffFile {
        old_mode: row.old.map(|(_, class)| class.mode_str().to_string()),
        new_mode: row.new.map(|(_, class)| class.mode_str().to_string()),
        old_oid: oid_of(old, row.old),
        new_oid: oid_of(new, row.new),
        similarity: match row.status {
            // Exact match, so the two sides are byte-identical: 100 is
            // measured here, not assumed, and it is the number git's own
            // `R100` carries for the same pair.
            EntryStatus::Renamed => Some(100),
            _ => None,
        },
        binary: None,
        additions: None,
        deletions: None,
        lines_capped: false,
        path: row.path,
        old_path: row.old_path,
        status: row.status,
    };
    if opts.name_only {
        return Ok(file);
    }

    // A rename is byte-identical by construction, so there is nothing to
    // count and nothing to read.
    if file.status == EntryStatus::Renamed {
        file.binary = Some(false);
        file.additions = Some(0);
        file.deletions = Some(0);
        return Ok(file);
    }

    let old_bytes = worktree_bytes(repo, opts, old, &file.old_path.clone().unwrap_or_else(|| file.path.clone()), row.old)?;
    let new_bytes = worktree_bytes(repo, opts, new, &file.path, row.new)?;
    let old_side = side_for(row.old, old, old_bytes.as_deref());
    let new_side = side_for(row.new, new, new_bytes.as_deref());

    match diffcore::line_delta(repo, "diff", old_side, new_side, opts.max_blob_bytes)? {
        LineDelta::Counted { added, deleted } => {
            file.binary = Some(false);
            file.additions = Some(added);
            file.deletions = Some(deleted);
        }
        LineDelta::Binary => {
            file.binary = Some(true);
            // Git prints `-` for both columns in `--numstat` here, and leaves
            // binary files out of its shortstat totals. `null` is that `-`.
        }
        LineDelta::OverCap => {
            file.lines_capped = true;
        }
        LineDelta::Gitlink => {
            // A gitlink's patch is one `Subproject commit <oid>` line per
            // side, which is what git counts: `1 1` for a moved pointer,
            // `1 0` for a new submodule. No blob is read to know that.
            file.binary = Some(false);
            file.additions = Some(u64::from(row.new.is_some()));
            file.deletions = Some(u64::from(row.old.is_some()));
        }
    }
    Ok(file)
}

/// Re-read a working-tree file's bytes for the line count.
///
/// Deliberately a second read: the hashing pass that decided *whether* the
/// file changed holds nothing, so a repository with a hundred thousand
/// modified files costs one file's bytes at a time rather than all of them at
/// once. Only the files that survived `--limit` are read again.
fn worktree_bytes(
    repo: &ReadRepo,
    opts: &DiffOptions,
    side: &SideMap,
    path: &str,
    entry: Option<(ObjectId, Class)>,
) -> Result<Option<Vec<u8>>, GitError> {
    const OP: &str = "diff";
    if !side.from_worktree || entry.is_none() {
        return Ok(None);
    }
    let Some((_, class)) = entry else {
        return Ok(None);
    };
    if class == Class::Commit {
        return Ok(None);
    }
    let Some(work_dir) = repo.work_dir().map(std::path::Path::to_path_buf) else {
        return Ok(None);
    };
    let mut paths = WorktreePaths::new(OP, &work_dir, repo.ceiling());
    let Some(full) = paths.leaf(path)? else {
        return Ok(None);
    };
    let meta = match std::fs::symlink_metadata(&full) {
        Ok(m) => m,
        // The file was there when it was hashed and is gone now. Nothing to
        // count, and the row already says what changed.
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(GitError::repository(OP, "reading a worktree file", &full, e)),
    };
    Ok(Some(read_worktree_blob(OP, path, &full, &meta, opts.max_blob_bytes)?))
}

/// Which end of [`diffcore::line_delta`] this side is.
fn side_for<'a>(
    entry: Option<(ObjectId, Class)>,
    side: &SideMap,
    bytes: Option<&'a [u8]>,
) -> Side<'a> {
    match entry {
        None => Side::Absent,
        Some((oid, class)) => {
            if class == Class::Commit {
                // A gitlink's oid is a commit in another repository, so the
                // class names it rather than the object store being asked for
                // a header it cannot answer.
                Side::Gitlink
            } else if side.from_worktree {
                // Working-tree content is not in the object store, so the
                // bytes travel instead of the oid. An absent read (the file
                // vanished between passes) is an empty side, not a lie about
                // an object that would not be there.
                Side::Bytes(bytes.unwrap_or(&[]))
            } else {
                Side::Object(oid)
            }
        }
    }
}
