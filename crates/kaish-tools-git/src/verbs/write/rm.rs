//! `git rm` (git-write-profile.md §4).
//!
//! Accepts `<paths>`, `--cached`, `-r` and `-f`. Without `-f` it keeps git's
//! safety checks (builtin/rm.c `check_local_mod`): a path whose index entry
//! differs from HEAD, or whose file differs from the index, is not removed,
//! and the refusal names `--cached` and `-f`.

use std::collections::BTreeMap;
use std::path::Path;

use clap::Parser;
use gix_index::entry::Mode;
use gix_index::hash::ObjectId;
use kaish_tool_api::GlobalFlags;

use crate::error::GitError;
use crate::verbs::write::add::Tracked;
use crate::verbs::write::head;
use crate::verbs::write::indexfile::{self, IndexTx, Snapshot};
use crate::verbs::write::refusal::{refuse, Override};
use crate::verbs::write::repo::WriteRepo;
use crate::verbs::write::spec::{is_glob, Outcome, SpecSet};
use crate::verbs::write::state::{ensure_writable, StagePolicy};

const OP: &str = "rm";

/// `git rm`'s argv surface.
#[derive(Parser, Debug)]
#[command(
    name = "rm",
    about = "Remove paths from the index and the working tree. Refuses a path with staged or local changes unless -f is given"
)]
pub(crate) struct RmArgs {
    /// Remove from the index only; keep the file in the working tree.
    #[arg(long = "cached")]
    pub cached: bool,

    /// Allow removing a directory: every tracked file below it.
    #[arg(short = 'r')]
    pub recursive: bool,

    /// Remove even when the file differs from the index, or the index differs
    /// from HEAD. The changes are lost.
    #[arg(short = 'f', long = "force")]
    pub force: bool,

    /// Repository to change. Defaults to the current directory; discovery
    /// searches upward, never past the mount root that contains the path.
    #[arg(long = "repo", value_name = "PATH")]
    pub repo: Option<String>,

    #[command(flatten)]
    pub global: GlobalFlags,

    /// Paths to remove, relative to the current directory: tracked files,
    /// directories (with `-r`) or `*`/`?`/`[...]` patterns.
    #[arg(hide = true)]
    pub operands: Vec<String>,
}

pub(crate) struct RmOptions {
    pub specs: Vec<String>,
    pub cached: bool,
    pub recursive: bool,
    pub force: bool,
    pub command: String,
}

struct Plan {
    paths: Vec<String>,
}

pub(crate) fn run(repo: &WriteRepo, opts: &RmOptions) -> Result<Outcome, GitError> {
    if opts.specs.is_empty() {
        return refuse(
            2,
            &opts.command,
            "No path was given.",
            "Name the paths to remove, for example: git rm <path>.",
            Override::Repository,
        );
    }
    let policy = ensure_writable(repo, &opts.command)?;
    let snap = Snapshot::read(repo, &opts.command)?;
    let mut plan = build(repo, &snap.state, snap.mtime, &policy, opts)?;
    let mut tx = IndexTx::lock(repo, &opts.command)?;
    if tx.snap.bytes != snap.bytes {
        plan = build(repo, tx.state(), tx.snap.mtime, &policy, opts)?;
    }

    if !opts.cached {
        for path in &plan.paths {
            let abs = repo.check_worktree_any(Path::new(path))?;
            match std::fs::symlink_metadata(&abs) {
                Ok(m) if m.is_dir() => continue,
                Ok(_) => {
                    std::fs::remove_file(&abs)
                        .map_err(|e| GitError::repository(OP, "removing a working-tree file", &abs, e))?;
                    remove_empty_parents(repo, path);
                }
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
                Err(e) => return Err(GitError::repository(OP, "reading a working-tree path", &abs, e)),
            }
        }
    }
    for path in &plan.paths {
        indexfile::remove(tx.state_mut(), path);
    }
    tx.commit(repo)?;

    let text: String = plan.paths.iter().map(|p| format!("rm '{p}'\n")).collect();
    Ok(Outcome { text, err: String::new(), json: serde_json::json!({ "removed": plan.paths, "cached": opts.cached }) })
}

/// Git removes the directories a removal leaves empty, up to the root.
fn remove_empty_parents(repo: &WriteRepo, path: &str) {
    let Some(work) = repo.read().work_dir() else { return };
    let mut cur = path;
    while let Some(i) = cur.rfind('/') {
        cur = &cur[..i];
        // Fails, and stops, at the first directory that still has content.
        if std::fs::remove_dir(crate::worktree::join_repo_relative(work, cur)).is_err() {
            break;
        }
    }
}

fn build(
    repo: &WriteRepo,
    state: &gix_index::State,
    mtime: Option<filetime::FileTime>,
    policy: &StagePolicy,
    opts: &RmOptions,
) -> Result<Plan, GitError> {
    let mut specs = SpecSet::new(OP, &opts.specs)?;
    let tracked = Tracked::all(state);
    let mut matched: Vec<&Tracked> = Vec::new();
    for t in &tracked {
        let hits = specs.hits(&t.path);
        if hits.is_empty() {
            continue;
        }
        specs.mark(&hits);
        // Matched only as the inside of a directory spec: needs -r.
        if !opts.recursive {
            for s in &opts.specs {
                if !is_glob(s) && !s.is_empty() && *s != t.path && t.path.starts_with(&format!("{s}/")) {
                    return refuse(
                        1,
                        &opts.command,
                        format!("'{s}' is a directory, and removing it would remove every tracked file below it."),
                        format!("To remove the directory, run: git rm -r {s}."),
                        Override::Flag("-r"),
                    );
                }
            }
        }
        if t.mode == Mode::COMMIT {
            return refuse(
                4,
                &opts.command,
                format!("'{}' is a submodule, and removing one means editing .gitmodules and its repository.", t.path),
                "Remove it with real git, outside this shell: git rm <path>.",
                Override::Repository,
            );
        }
        matched.push(t);
    }
    if let Some(m) = specs.unmatched().first() {
        return refuse(
            1,
            &opts.command,
            format!("The pathspec '{m}' did not match any files."),
            "Check the name with: git status.",
            Override::Repository,
        );
    }

    if !opts.force {
        let head_map: BTreeMap<String, (ObjectId, Mode)> = head::flatten(repo, OP)?;
        let mut staged = Vec::new();
        let mut cached = Vec::new();
        let mut local = Vec::new();
        for t in &matched {
            let staged_changes = match head_map.get(&t.path) {
                Some((id, mode)) => *id != t.id || *mode != t.mode,
                None => true,
            };
            let abs = repo.check_worktree_any(Path::new(&t.path))?;
            let local_changes = match std::fs::symlink_metadata(&abs) {
                // Already gone, or a directory where the file was: nothing to lose.
                Err(_) => false,
                Ok(m) if m.is_dir() => false,
                Ok(m) => t.differs(&abs, &m, mtime, policy),
            };
            if local_changes && staged_changes {
                if !opts.cached || head_map.get(&t.path).is_none_or(|(id, _)| *id != t.id) {
                    staged.push(t.path.clone());
                }
            } else if !opts.cached {
                if staged_changes {
                    cached.push(t.path.clone());
                }
                if local_changes {
                    local.push(t.path.clone());
                }
            }
        }
        let list = |v: &[String]| v.join(", ");
        let all = |v: &[String]| v.join(" ");
        if !staged.is_empty() {
            return refuse(
                1,
                &opts.command,
                format!("The following files have staged content different from both the file and the HEAD: {}.", list(&staged)),
                format!("To remove them anyway, run: git rm -f {}.", all(&staged)),
                Override::Flag("-f"),
            );
        }
        if !cached.is_empty() {
            return refuse(
                1,
                &opts.command,
                format!("The following files have changes staged in the index: {}.", list(&cached)),
                format!("To keep the files and remove them from the index only, run: git rm --cached {}. To remove them from both, run: git rm -f {}.", all(&cached), all(&cached)),
                Override::Flag("-f"),
            );
        }
        if !local.is_empty() {
            return refuse(
                1,
                &opts.command,
                format!("The following files have local modifications: {}.", list(&local)),
                format!("To keep the files and remove them from the index only, run: git rm --cached {}. To remove them from both, run: git rm -f {}.", all(&local), all(&local)),
                Override::Flag("-f"),
            );
        }
    }

    // Containment for every deletion, before the first one.
    if !opts.cached {
        for t in &matched {
            repo.check_worktree_any(Path::new(&t.path))?;
        }
    }
    Ok(Plan { paths: matched.iter().map(|t| t.path.clone()).collect() })
}
