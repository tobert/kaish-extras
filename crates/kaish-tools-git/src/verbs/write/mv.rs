//! `git mv` (git-write-profile.md §4).
//!
//! Accepts `<src> <dst>` and `<src>... <dir>`, and `-f`. The only file this
//! verb touches in the working tree is a rename, so no content is written and
//! no filter or checkout path is involved. Refuses what git refuses: an
//! untracked or missing source, an existing destination without `-f`, a
//! directory moved into itself, a destination whose parent directory does not
//! exist. Containment applies to both paths.

use std::collections::BTreeSet;
use std::path::Path;

use clap::Parser;
use gix_index::entry::Mode;
use kaish_tool_api::GlobalFlags;

use crate::error::GitError;
use crate::verbs::write::add::Tracked;
use crate::verbs::write::indexfile::{self, IndexTx, Snapshot, Upsert};
use crate::verbs::write::refusal::{refuse, Override};
use crate::verbs::write::repo::WriteRepo;
use crate::verbs::write::spec::Outcome;
use crate::verbs::write::state::ensure_writable;

const OP: &str = "mv";

/// `git mv`'s argv surface.
#[derive(Parser, Debug)]
#[command(
    name = "mv",
    about = "Rename a tracked file or directory in the working tree and the index. Moves into a directory when the last path is one"
)]
pub(crate) struct MvArgs {
    /// Replace an existing destination file.
    #[arg(short = 'f', long = "force")]
    pub force: bool,

    /// Repository to change. Defaults to the current directory; discovery
    /// searches upward, never past the mount root that contains the path.
    #[arg(long = "repo", value_name = "PATH")]
    pub repo: Option<String>,

    #[command(flatten)]
    pub global: GlobalFlags,

    /// `<src> <dst>`, or `<src>... <dir>`: tracked paths, relative to the
    /// current directory. The last path is the destination.
    #[arg(hide = true)]
    pub operands: Vec<String>,
}

pub(crate) struct MvOptions {
    /// Repo-relative; at least one source and the destination last.
    pub paths: Vec<String>,
    pub force: bool,
    pub command: String,
}

struct Move {
    src: String,
    dst: String,
    /// Index entries carried along: (old path, new path).
    entries: Vec<(String, String)>,
}

pub(crate) fn run(repo: &WriteRepo, opts: &MvOptions) -> Result<Outcome, GitError> {
    if opts.paths.len() < 2 {
        return refuse(
            2,
            &opts.command,
            "A source and a destination are both required.",
            "Name both, for example: git mv <src> <dst>.",
            Override::Repository,
        );
    }
    ensure_writable(repo, &opts.command)?;
    let snap = Snapshot::read(repo, &opts.command)?;
    let mut moves = build(repo, &snap.state, opts)?;
    let mut tx = IndexTx::lock(repo, &opts.command)?;
    if tx.snap.bytes != snap.bytes {
        moves = build(repo, tx.state(), opts)?;
    }

    // Rename in the working tree first; undo on the first failure.
    let mut done: Vec<(std::path::PathBuf, std::path::PathBuf)> = Vec::new();
    for m in &moves {
        let from = any_leaf(repo, &m.src)?;
        let to = any_leaf(repo, &m.dst)?;
        if let Err(e) = std::fs::rename(&from, &to) {
            for (f, t) in done.iter().rev() {
                // Best effort: the original error is the one to report.
                let _ = std::fs::rename(t, f);
            }
            return Err(GitError::repository(OP, format!("renaming '{}'", m.src), &from, e));
        }
        done.push((from, to));
    }

    let mut upserts = Vec::new();
    {
        let state = tx.state_mut();
        let moved: BTreeSet<&str> = moves.iter().flat_map(|m| m.entries.iter().map(|(o, _)| o.as_str())).collect();
        for t in Tracked::all(state) {
            if let Some((_, new)) = moves.iter().flat_map(|m| m.entries.iter()).find(|(o, _)| *o == t.path) {
                upserts.push(Upsert { path: new.clone(), stat: t.stat, id: t.id, mode: t.mode });
            }
        }
        for old in moved {
            indexfile::remove(state, old);
        }
        indexfile::upsert_all(state, upserts);
    }
    tx.commit(repo)?;

    let pairs: Vec<[&str; 2]> = moves.iter().map(|m| [m.src.as_str(), m.dst.as_str()]).collect();
    Ok(Outcome { text: String::new(), err: String::new(), json: serde_json::json!({ "moved": pairs }) })
}

fn any_leaf(repo: &WriteRepo, rel: &str) -> Result<std::path::PathBuf, GitError> {
    repo.check_worktree_any(Path::new(rel))
}

fn is_real_dir(repo: &WriteRepo, rel: &str) -> Result<bool, GitError> {
    if rel.is_empty() {
        return Ok(true);
    }
    let abs = any_leaf(repo, rel)?;
    Ok(std::fs::symlink_metadata(abs).is_ok_and(|m| m.is_dir()))
}

fn build(repo: &WriteRepo, state: &gix_index::State, opts: &MvOptions) -> Result<Vec<Move>, GitError> {
    let (sources, dest) = opts.paths.split_at(opts.paths.len() - 1);
    let dest = &dest[0];
    let dest_is_dir = is_real_dir(repo, dest)?;
    if sources.len() > 1 && !dest_is_dir {
        return refuse(
            1,
            &opts.command,
            format!("The destination '{dest}' is not a directory, and several sources were given."),
            "Give one source, or make the last path an existing directory.",
            Override::Repository,
        );
    }
    let tracked = Tracked::all(state);
    let tracked_paths: BTreeSet<&str> = tracked.iter().map(|t| t.path.as_str()).collect();
    let mut moves = Vec::new();
    let mut dsts = BTreeSet::new();
    for src in sources {
        if src.is_empty() {
            return refuse(1, &opts.command, "The repository root cannot be moved.", "Name a file or directory inside it.", Override::Repository);
        }
        let base = src.rsplit('/').next().unwrap_or(src);
        let dst = if dest_is_dir {
            if dest.is_empty() { base.to_string() } else { format!("{dest}/{base}") }
        } else {
            dest.clone()
        };
        let abs_src = any_leaf(repo, src)?;
        let src_meta = std::fs::symlink_metadata(&abs_src);
        let below = format!("{src}/");
        let entries: Vec<&Tracked> = tracked.iter().filter(|t| t.path == *src || t.path.starts_with(&below)).collect();
        let bad = |why: &str| {
            refuse::<Vec<Move>>(
                1,
                &opts.command,
                format!("The source '{src}' {why}."),
                "Check the names with: git status.",
                Override::Repository,
            )
        };
        if src_meta.is_err() {
            bad("does not exist")?;
        }
        if entries.is_empty() {
            bad("is not tracked: it is not under version control")?;
        }
        if entries.iter().any(|t| t.mode == Mode::COMMIT) {
            return refuse(
                4,
                &opts.command,
                format!("'{src}' is or holds a submodule, and moving one means editing .gitmodules and its repository."),
                "Move it with real git, outside this shell: git mv <src> <dst>.",
                Override::Repository,
            );
        }
        if dst == *src || dst.starts_with(&below) {
            return refuse(
                1,
                &opts.command,
                format!("'{src}' cannot be moved into itself ('{dst}')."),
                "Choose a destination outside the source.",
                Override::Repository,
            );
        }
        if !dsts.insert(dst.clone()) {
            return refuse(1, &opts.command, format!("Two sources would both become '{dst}'."), "Move them one at a time.", Override::Repository);
        }
        // The destination.
        let abs_dst = any_leaf(repo, &dst)?;
        let dst_below = format!("{dst}/");
        let exists_on_disk = std::fs::symlink_metadata(&abs_dst);
        let in_index = tracked_paths.contains(dst.as_str()) || tracked.iter().any(|t| t.path.starts_with(&dst_below));
        if exists_on_disk.is_ok() || in_index {
            let dir_there = exists_on_disk.as_ref().is_ok_and(|m| m.is_dir()) || tracked.iter().any(|t| t.path.starts_with(&dst_below));
            if !opts.force || dir_there {
                return refuse(
                    1,
                    &opts.command,
                    if dir_there { format!("The destination '{dst}' exists and is a directory.") } else { format!("The destination '{dst}' exists.") },
                    if dir_there { "Choose a name that does not exist.".to_string() } else { format!("To replace it, run: git mv -f {src} {dst}.") },
                    if dir_there { Override::Repository } else { Override::Flag("-f") },
                );
            }
        }
        // The parent must exist: this verb creates no directories.
        if let Some((parent, _)) = dst.rsplit_once('/') {
            if !is_real_dir(repo, parent)? {
                return refuse(
                    1,
                    &opts.command,
                    format!("Renaming '{src}' to '{dst}' would fail: the directory '{parent}' does not exist."),
                    format!("Create it first, for example with: mkdir -p {parent}."),
                    Override::Repository,
                );
            }
        }
        let entries = entries
            .iter()
            .map(|t| (t.path.clone(), format!("{dst}{}", &t.path[src.len()..])))
            .collect();
        moves.push(Move { src: src.clone(), dst, entries });
    }
    Ok(moves)
}
