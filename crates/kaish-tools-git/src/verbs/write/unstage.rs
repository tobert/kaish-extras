//! `git restore --staged` and `git reset` with no revision
//! (git-write-profile.md §4, W6).
//!
//! Both put the index entries for the named paths back to HEAD's. Only
//! entries that differ are touched, and a restored entry gets an all-zero
//! stat, as git writes it. A path in the index and not in HEAD is removed
//! from the index (the file stays, untracked). Nothing is written to the
//! working tree. `restore` without `--staged` and `reset <rev>` refuse.

use clap::Parser;
use gix_index::entry::{Mode, Stat};
use kaish_tool_api::GlobalFlags;

use crate::error::GitError;
use crate::verbs::write::add::Tracked;
use crate::verbs::write::head;
use crate::verbs::write::indexfile::{self, IndexTx, Snapshot, Upsert};
use crate::verbs::write::refusal::{refuse, Override};
use crate::verbs::write::repo::WriteRepo;
use crate::verbs::write::spec::{Outcome, SpecSet};
use crate::verbs::write::state::ensure_writable;

/// `git restore`'s argv surface. Only `--staged` is supported.
#[derive(Parser, Debug)]
#[command(
    name = "restore",
    about = "Unstage paths with --staged: put their index entries back to HEAD's. Never writes the working tree; restore without --staged is refused"
)]
pub(crate) struct RestoreArgs {
    /// Restore the index entries from HEAD. Required: restoring working-tree
    /// content is refused.
    #[arg(short = 'S', long = "staged")]
    pub staged: bool,

    /// Refused: restoring working-tree files overwrites file content, which
    /// this shell does not do.
    #[arg(short = 'W', long = "worktree")]
    pub worktree: bool,

    /// Refused: choosing hunks needs a terminal this shell does not have.
    #[arg(short = 'p', long = "patch")]
    pub patch: bool,

    /// Repository to change. Defaults to the current directory; discovery
    /// searches upward, never past the mount root that contains the path.
    #[arg(long = "repo", value_name = "PATH")]
    pub repo: Option<String>,

    #[command(flatten)]
    pub global: GlobalFlags,

    /// Paths to unstage, relative to the current directory. `.` is the
    /// current directory.
    #[arg(hide = true)]
    pub operands: Vec<String>,
}

/// `git reset`'s argv surface. Only the forms that move no ref are supported.
#[derive(Parser, Debug)]
#[command(
    name = "reset",
    about = "Unstage paths, or move the branch to a revision with --soft or --mixed (subject to the rewrite policy). Never writes the working tree; --hard, --merge and --keep are refused"
)]
pub(crate) struct ResetArgs {
    /// Move the branch to the revision and put the whole index back to its
    /// tree. The default. The working tree is not touched. Refused by the
    /// rewrite policy when the move drops commits the policy protects.
    #[arg(long = "mixed")]
    pub mixed: bool,

    /// Move the branch to the revision and change nothing else: the index and
    /// the working tree stay. Refused by the rewrite policy when the move drops
    /// commits the policy protects.
    #[arg(long = "soft")]
    pub soft: bool,

    /// Refused: it discards working-tree changes, which this shell never does.
    #[arg(long = "hard")]
    pub hard: bool,

    /// Refused: it writes working-tree files, which this shell never does.
    #[arg(long = "merge")]
    pub merge: bool,

    /// Refused: it writes working-tree files, which this shell never does.
    #[arg(long = "keep")]
    pub keep: bool,

    /// Refused: choosing hunks needs a terminal this shell does not have.
    #[arg(short = 'p', long = "patch")]
    pub patch: bool,

    /// Repository to change. Defaults to the current directory; discovery
    /// searches upward, never past the mount root that contains the path.
    #[arg(long = "repo", value_name = "PATH")]
    pub repo: Option<String>,

    #[command(flatten)]
    pub global: GlobalFlags,

    /// Paths to unstage, relative to the current directory, after an optional
    /// `HEAD` and an optional `--`. With none, the whole index is reset.
    #[arg(hide = true)]
    pub operands: Vec<String>,
}

pub(crate) struct UnstageOptions {
    /// Repo-relative; empty means every path (reset only).
    pub specs: Vec<String>,
    pub operation: &'static str,
    pub command: String,
}

pub(crate) fn refuse_restore_worktree<T>(command: &str) -> Result<T, GitError> {
    refuse(
        2,
        command,
        "Restoring working-tree files overwrites file content, which this shell does not do.",
        "To unstage a file, run: git restore --staged <path>. To discard edits, use real git outside this shell: git restore <path>.",
        Override::Embedder,
    )
}

pub(crate) fn refuse_reset_hard<T>(command: &str) -> Result<T, GitError> {
    refuse(
        2,
        command,
        "git reset --hard discards working-tree changes, which this shell never does.",
        "To unstage files, run: git reset -- <path>. To move the branch and keep the files as they are, run: git reset --soft <rev>.",
        Override::Never,
    )
}

pub(crate) fn refuse_reset_worktree_mode<T>(command: &str, flag: &str) -> Result<T, GitError> {
    refuse(
        2,
        command,
        format!("git reset {flag} writes working-tree files, which this shell never does."),
        "To move the branch and keep the files as they are, run: git reset --soft <rev>. To move it and unstage everything, run: git reset <rev>.",
        Override::Never,
    )
}

struct Plan {
    restore: Vec<(String, gix_index::hash::ObjectId, Mode)>,
    remove: Vec<String>,
}

type TreeMap = std::collections::BTreeMap<String, (gix_index::hash::ObjectId, Mode)>;

/// An index change planned and, when there is one, locked and applied in
/// memory. [`Prepared::finish`] writes it.
pub(crate) struct Prepared {
    tx: Option<IndexTx>,
    plan: Plan,
}

pub(crate) fn run(repo: &WriteRepo, opts: &UnstageOptions) -> Result<Outcome, GitError> {
    let op = opts.operation;
    ensure_writable(repo, &opts.command)?;
    // `git reset` treats an unborn HEAD as the empty tree; `git restore
    // --staged` has no commit to restore from and fails.
    if op == "restore" && repo.read().head_tree_id()?.is_none() {
        return refuse(
            1,
            &opts.command,
            "HEAD does not name a commit yet, so there is nothing to restore the index from.",
            "To remove paths from the index, run: git rm --cached <path>.",
            Override::Repository,
        );
    }
    let head_map = head::flatten(repo, op)?;
    prepare(repo, opts, &head_map)?.finish(repo)
}

/// Plan the change that puts the index entries for `opts.specs` (all of them
/// when empty) back to `tree`'s, and lock the index when there is a change.
/// The caller has already run `ensure_writable`.
pub(crate) fn prepare(repo: &WriteRepo, opts: &UnstageOptions, tree: &TreeMap) -> Result<Prepared, GitError> {
    let snap = Snapshot::read(repo, &opts.command)?;
    let mut plan = build(&snap.state, tree, opts)?;
    if plan.restore.is_empty() && plan.remove.is_empty() {
        return Ok(Prepared { tx: None, plan });
    }
    let mut tx = IndexTx::lock(repo, &opts.command)?;
    if tx.snap.bytes != snap.bytes {
        plan = build(tx.state(), tree, opts)?;
    }
    for p in &plan.remove {
        indexfile::remove(tx.state_mut(), p);
    }
    let upserts = plan
        .restore
        .iter()
        .map(|(path, id, mode)| Upsert { path: path.clone(), stat: Stat::default(), id: *id, mode: *mode })
        .collect();
    indexfile::upsert_all(tx.state_mut(), upserts);
    Ok(Prepared { tx: Some(tx), plan })
}

impl Prepared {
    pub(crate) fn finish(self, repo: &WriteRepo) -> Result<Outcome, GitError> {
        let out = report(&self.plan);
        if let Some(tx) = self.tx {
            tx.commit(repo)?;
        }
        Ok(out)
    }

    pub(crate) fn paths(&self) -> Vec<String> {
        let mut v: Vec<String> = self.plan.restore.iter().map(|(p, _, _)| p.clone()).collect();
        v.extend(self.plan.remove.iter().cloned());
        v.sort();
        v
    }
}

fn report(plan: &Plan) -> Outcome {
    let mut unstaged: Vec<&str> = plan.restore.iter().map(|(p, _, _)| p.as_str()).collect();
    unstaged.extend(plan.remove.iter().map(String::as_str));
    unstaged.sort();
    Outcome { text: String::new(), err: String::new(), json: serde_json::json!({ "unstaged": unstaged }) }
}

fn build(
    state: &gix_index::State,
    head_map: &TreeMap,
    opts: &UnstageOptions,
) -> Result<Plan, GitError> {
    let mut specs = SpecSet::new(opts.operation, &opts.specs)?;
    let tracked = Tracked::all(state);
    let index: std::collections::BTreeMap<&str, &Tracked> = tracked.iter().map(|t| (t.path.as_str(), t)).collect();
    let mut plan = Plan { restore: Vec::new(), remove: Vec::new() };
    let paths: std::collections::BTreeSet<&str> =
        index.keys().copied().chain(head_map.keys().map(String::as_str)).collect();
    for path in paths {
        if !specs.matches(path) {
            continue;
        }
        match (index.get(path), head_map.get(path)) {
            (Some(t), Some((id, mode))) => {
                if t.id != *id || t.mode != *mode {
                    plan.restore.push((path.to_string(), *id, *mode));
                }
            }
            (None, Some((id, mode))) => plan.restore.push((path.to_string(), *id, *mode)),
            (Some(_), None) => plan.remove.push(path.to_string()),
            (None, None) => {}
        }
    }
    if let Some(m) = specs.unmatched().first() {
        return refuse(
            1,
            &opts.command,
            format!("The pathspec '{m}' did not match any file known to git."),
            "Check the name with: git status.",
            Override::Repository,
        );
    }
    Ok(plan)
}
