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
    about = "Unstage paths: put their index entries back to HEAD's. Takes no revision other than HEAD, moves no branch, and never writes the working tree"
)]
pub(crate) struct ResetArgs {
    /// Reset the index only. This is the default and the only mode that runs.
    #[arg(long = "mixed")]
    pub mixed: bool,

    /// Refused: moving the branch is a history rewrite this shell does not do.
    #[arg(long = "soft")]
    pub soft: bool,

    /// Refused: it discards working-tree changes, which this shell never does.
    #[arg(long = "hard")]
    pub hard: bool,

    /// Refused: moving the branch is a history rewrite this shell does not do.
    #[arg(long = "merge")]
    pub merge: bool,

    /// Refused: moving the branch is a history rewrite this shell does not do.
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

pub(crate) fn refuse_reset_revision<T>(command: &str, what: &str) -> Result<T, GitError> {
    refuse(
        2,
        command,
        format!("{what} moves the branch, which rewrites history, and this shell does not do that."),
        "To unstage files without moving anything, run: git reset -- <path>.",
        Override::Embedder,
    )
}

pub(crate) fn refuse_reset_hard<T>(command: &str) -> Result<T, GitError> {
    refuse(
        2,
        command,
        "git reset --hard discards working-tree changes, which this shell never does.",
        "To unstage files, run: git reset -- <path>.",
        Override::Never,
    )
}

struct Plan {
    restore: Vec<(String, gix_index::hash::ObjectId, Mode)>,
    remove: Vec<String>,
}

pub(crate) fn run(repo: &WriteRepo, opts: &UnstageOptions) -> Result<Outcome, GitError> {
    let op = opts.operation;
    ensure_writable(repo, &opts.command)?;
    let head_map = head::flatten(repo, op)?;
    let snap = Snapshot::read(repo, &opts.command)?;
    let mut plan = build(&snap.state, &head_map, opts)?;
    if plan.restore.is_empty() && plan.remove.is_empty() {
        return Ok(report(&plan));
    }
    let mut tx = IndexTx::lock(repo, &opts.command)?;
    if tx.snap.bytes != snap.bytes {
        plan = build(tx.state(), &head_map, opts)?;
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
    tx.commit(repo)?;
    Ok(report(&plan))
}

fn report(plan: &Plan) -> Outcome {
    let mut unstaged: Vec<&str> = plan.restore.iter().map(|(p, _, _)| p.as_str()).collect();
    unstaged.extend(plan.remove.iter().map(String::as_str));
    unstaged.sort();
    Outcome { text: String::new(), json: serde_json::json!({ "unstaged": unstaged }) }
}

fn build(
    state: &gix_index::State,
    head_map: &std::collections::BTreeMap<String, (gix_index::hash::ObjectId, Mode)>,
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
