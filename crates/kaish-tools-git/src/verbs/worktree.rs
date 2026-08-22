//! `git worktree list` — every working tree this repository has
//! (architecture.md B.9).
//!
//! The read half of `worktree`, and the only half in the read profile: every
//! answer here comes out of the registrations under `<common>/worktrees/`,
//! which this build reads and never writes. Create, remove, lock and prune
//! wait on the ledger (B.11).
//!
//! **What is read, and where from.** A linked worktree's registration is a
//! directory under the common dir holding `gitdir` (the path of the
//! worktree's own `.git` file), `HEAD`, and optionally `locked`. All three are
//! inside the mount, because the common dir is, and each is opened through
//! `ReadRepo::contained_leaf` so a symlinked one is refused rather than
//! followed.
//!
//! **What is not read.** The working tree itself. `gitdir`'s *content* is a
//! path the repository chose and can name anywhere on the host, so it is
//! reported exactly as recorded and probed only when it is lexically inside
//! the mount. Stat-ing an out-of-mount path to decide `prunable` would answer
//! one bit about an arbitrary host path per registration, which is the
//! existence oracle `repo.rs`'s `contain` exists to refuse. Such a row carries
//! `path_vfs: null` and `prunable: null`: named, not examined.

use std::path::{Component, Path, PathBuf};

use clap::Parser;

use gix_index::hash as gix_hash;

use kaish_tool_api::GlobalFlags;

use crate::error::GitError;
use crate::model::{WorktreeReport, WorktreeRow};
use crate::repo::ReadRepo;

const OP: &str = "worktree list";

/// `git worktree list`'s argv surface (architecture.md B.9).
#[derive(Parser, Debug)]
#[command(name = "list", about = "List every working tree of this repository")]
pub(crate) struct WorktreeListArgs {
    /// Maximum working trees to report. Truncation is always reported
    /// (`truncated: true` and a stderr note), never silent.
    #[arg(short = 'n', long = "limit", value_name = "N", default_value_t = 1000)]
    pub limit: usize,

    /// Repository to inspect. Defaults to the current directory; discovery
    /// searches upward, never past the mount root that contains the path.
    /// Listing from inside a linked working tree reports them all.
    #[arg(long = "repo", value_name = "PATH")]
    pub repo: Option<String>,

    #[command(flatten)]
    pub global: GlobalFlags,

    /// Takes no operands: `git worktree list` reports every working tree, and
    /// a repository other than the current directory is named with `--repo`.
    // Bound so clap accepts the `--`-terminated tail `ToolArgs::to_argv()`
    // always emits. The real operands are read off `args.positional` in
    // `tool.rs`, which refuses them by name.
    #[arg(hide = true)]
    pub operands: Vec<String>,
}

/// Everything `run` needs, decoupled from clap.
pub(crate) struct WorktreeListOptions {
    /// The effective row cap: the smaller of `--limit` and the embedder's cap.
    pub limit: usize,
    /// The real root of the mount the caller reached this repository through,
    /// for mapping each working tree back into the VFS.
    pub mount_real: PathBuf,
    /// That mount's VFS path.
    pub mount_vfs: PathBuf,
}

/// Compose the worktree listing for `repo` (architecture.md B.9).
pub(crate) fn run(
    repo: &ReadRepo,
    opts: &WorktreeListOptions,
) -> Result<WorktreeReport, GitError> {
    let mut rows = Vec::new();

    // The main working tree first, which is git's own order.
    //
    // "Has a main working tree" is read off the common dir being named `.git`,
    // gitoxide's own `DOT_GIT_DIR` convention and the same rule
    // `ReadRepo::worktree_count` applies. A repository whose git directory is
    // named something else is listed without its main working tree; that shape
    // needs the `core.worktree` handling this build does not have.
    if let Some(main) = main_worktree_path(repo) {
        let (head_oid, branch) = head_of(repo, repo.common_dir())?;
        rows.push(row(
            None,
            &main,
            head_oid,
            branch,
            false,
            None,
            opts,
            repo,
        )?);
    }

    // Where the linked worktrees start. Zero for a bare repository, which has
    // no main working tree to put above them.
    let linked_start = rows.len();

    if let Some(dir) = repo.contained_leaf("worktrees directory", repo.common_dir(), "worktrees")? {
        // `read_dir` order is the filesystem's, so the names are collected
        // first and the rows sorted afterwards — by **path**, which is what
        // real git orders by. Registration name and path can disagree: a
        // `git worktree move` keeps the name and changes the path, and a
        // worktree nested under another sorts by where it is rather than by
        // what it is called. Confirmed against `git worktree list --porcelain`
        // rather than assumed.
        let mut names: Vec<String> = Vec::new();
        for entry in std::fs::read_dir(&dir).map_err(|e| {
            GitError::repository(OP, "listing linked worktrees", &dir, e)
        })? {
            let entry = entry.map_err(|e| {
                GitError::repository(OP, "listing linked worktrees", &dir, e)
            })?;
            // A registration is a real directory carrying a `gitdir` file. A
            // symlinked entry is not something git writes here, and following
            // one would step outside the mount to read it, so it is skipped —
            // `symlink_metadata` does not follow it. An unreadable entry is
            // treated as not a registration rather than swallowed silently:
            // there is nothing else it could be, and the alternative is
            // failing the whole listing over one stray name.
            let path = entry.path();
            let is_symlink = std::fs::symlink_metadata(&path)
                .map(|m| m.file_type().is_symlink())
                .unwrap_or(true);
            if is_symlink || !path.join("gitdir").is_file() {
                continue;
            }
            if let Some(name) = entry.file_name().to_str() {
                names.push(name.to_string());
            }
        }
        names.sort();

        for name in names {
            let Some(private) = repo.contained_leaf("worktree registration", &dir, &name)? else {
                continue;
            };
            let Some(gitdir_file) = repo.contained_leaf("worktree gitdir file", &private, "gitdir")?
            else {
                continue;
            };
            let recorded = std::fs::read(&gitdir_file).map_err(|e| {
                GitError::repository(OP, "reading a worktree's gitdir file", &gitdir_file, e)
            })?;
            let Some(work_path) = worktree_path_from_gitdir(&recorded) else {
                // A `gitdir` file that does not name a `.git` under a
                // directory is a registration we cannot interpret. Reporting
                // it with an invented path would be worse than leaving it out,
                // and refusing the listing would make one bad registration
                // hide every good one.
                continue;
            };

            let (head_oid, branch) = head_of(repo, &private)?;
            let (locked, lock_reason) = lock_of(repo, &private)?;
            rows.push(row(
                Some(name),
                &work_path,
                head_oid,
                branch,
                locked,
                lock_reason,
                opts,
                repo,
            )?);
        }
    }

    // The main working tree stays first — git's own order — and the linked
    // ones are sorted by path beneath it.
    rows[linked_start..].sort_by(|a, b| a.path_real.cmp(&b.path_real));

    let truncated = rows.len() > opts.limit;
    rows.truncate(opts.limit);
    Ok(WorktreeReport {
        worktrees: rows,
        truncated,
    })
}

/// Build one row, deciding reachability and prunability together because they
/// are the same question asked twice.
#[allow(clippy::too_many_arguments)]
fn row(
    name: Option<String>,
    work_path: &Path,
    head_oid: Option<String>,
    branch: Option<String>,
    locked: bool,
    lock_reason: Option<String>,
    opts: &WorktreeListOptions,
    repo: &ReadRepo,
) -> Result<WorktreeRow, GitError> {
    let path_vfs = crate::tool::to_vfs_path(work_path, &opts.mount_real, &opts.mount_vfs);
    let (prunable, prunable_reason) = if path_vfs.is_some() {
        prunability(repo, work_path)?
    } else {
        // Outside the mount: named, not examined. See the module doc.
        (None, None)
    };
    Ok(WorktreeRow {
        name,
        path_real: work_path.display().to_string(),
        path_vfs,
        head_oid,
        branch,
        locked,
        lock_reason,
        prunable,
        prunable_reason,
    })
}

/// The main working tree's root, or `None` for a repository whose git
/// directory is not a `.git` beside one.
fn main_worktree_path(repo: &ReadRepo) -> Option<PathBuf> {
    if repo.common_dir().file_name() != Some(std::ffi::OsStr::new(".git")) {
        return None;
    }
    repo.common_dir().parent().map(Path::to_path_buf)
}

/// Where a working tree's HEAD points: the commit and, when it is not
/// detached, the short branch name.
///
/// Read from the given git directory's own `HEAD`, because a linked
/// worktree's HEAD is private to it. A HEAD naming a branch that does not
/// exist yet is an unborn worktree, which is a state, not an error.
fn head_of(repo: &ReadRepo, git_dir: &Path) -> Result<(Option<String>, Option<String>), GitError> {
    let Some(head_file) = repo.contained_leaf("HEAD", git_dir, "HEAD")? else {
        return Ok((None, None));
    };
    let bytes = std::fs::read(&head_file)
        .map_err(|e| GitError::repository(OP, "reading a worktree's HEAD", &head_file, e))?;
    let text = String::from_utf8_lossy(&bytes);
    let text = text.trim();

    if let Some(refname) = text.strip_prefix("ref:") {
        let refname = refname.trim();
        let branch = refname
            .strip_prefix("refs/heads/")
            .unwrap_or(refname)
            .to_string();
        let found = repo
            .refs()
            .try_find(refname)
            .map_err(|e| GitError::repository(OP, "resolving a worktree's HEAD", git_dir, e))?;
        let oid = match found {
            Some(reference) => repo.ref_object(&reference)?.map(|o| o.to_string()),
            None => None,
        };
        return Ok((oid, Some(branch)));
    }

    // A detached HEAD names an object directly.
    match gix_hash::ObjectId::from_hex(text.as_bytes()) {
        Ok(oid) => Ok((Some(oid.to_string()), None)),
        // Neither `ref:` nor an oid. Not something git writes; reporting no
        // HEAD is the honest answer, and the row still names the worktree.
        Err(_) => Ok((None, None)),
    }
}

/// Whether a registration is locked, and the reason recorded with it.
fn lock_of(repo: &ReadRepo, private: &Path) -> Result<(bool, Option<String>), GitError> {
    let Some(path) = repo.contained_leaf("worktree lock file", private, "locked")? else {
        return Ok((false, None));
    };
    let bytes = std::fs::read(&path)
        .map_err(|e| GitError::repository(OP, "reading a worktree lock file", &path, e))?;
    let reason = String::from_utf8_lossy(&bytes).trim().to_string();
    Ok((true, (!reason.is_empty()).then_some(reason)))
}

/// Whether the registration outlived what it points at.
///
/// Only ever called for a path already known to be inside the mount, so the
/// probe answers about the caller's own sandbox rather than about the host.
/// The chain is walked a component at a time from the mount root, the same
/// rule `worktree.rs`'s `WorktreePaths::walk_chain` applies to index entries:
/// `symlink_metadata` classifies each component before it is traversed, and a
/// symlink that leaves the mount ends the walk rather than being followed.
fn prunability(repo: &ReadRepo, work_path: &Path) -> Result<(Option<bool>, Option<String>), GitError> {
    let ceiling = repo.ceiling();
    let Ok(rest) = work_path.strip_prefix(ceiling) else {
        return Ok((None, None));
    };
    let mut cur = ceiling.to_path_buf();
    for component in rest.components() {
        let Component::Normal(name) = component else {
            // `work_path` is absolute and lexically under the ceiling, so a
            // `.` or `..` here means the recorded path was not normalized. Not
            // ours to resolve — a `..` would walk back out of the sandbox.
            return Ok((None, None));
        };
        let next = cur.join(name);
        let meta = match std::fs::symlink_metadata(&next) {
            Ok(m) => m,
            Err(e)
                if matches!(
                    e.kind(),
                    std::io::ErrorKind::NotFound | std::io::ErrorKind::NotADirectory
                ) =>
            {
                return Ok((
                    Some(true),
                    Some("the working tree directory no longer exists".to_string()),
                ))
            }
            Err(e) => {
                return Err(GitError::repository(
                    OP,
                    "examining a working tree directory",
                    &next,
                    e,
                ))
            }
        };
        if meta.file_type().is_symlink() {
            match std::fs::canonicalize(&next) {
                Ok(real) if real.starts_with(ceiling) => cur = real,
                // Escaping or dangling, made indistinguishable on purpose:
                // the same rule `repo.rs`'s `contain` follows.
                _ => return Ok((None, None)),
            }
        } else {
            cur = next;
        }
    }

    // The directory is there. Git also calls a registration prunable when the
    // directory survives but its `.git` file is gone, which is what a manual
    // move leaves behind.
    if !cur.join(".git").exists() {
        return Ok((
            Some(true),
            Some("the working tree's .git file is missing".to_string()),
        ));
    }
    Ok((Some(false), None))
}

/// The working tree root a `gitdir` file's contents name.
///
/// git writes the path of the worktree's own `.git` *file*, so the working
/// tree is its parent. Only an absolute path is accepted: a relative one is
/// resolved against a base this build does not have (git's own
/// `relativeworktrees` extension, which `repo.rs` refuses to open a repository
/// under), and guessing the base would name a directory nobody chose.
fn worktree_path_from_gitdir(bytes: &[u8]) -> Option<PathBuf> {
    let text = String::from_utf8_lossy(bytes);
    let text = text.trim();
    if text.is_empty() {
        return None;
    }
    let path = Path::new(text);
    if !path.is_absolute() {
        return None;
    }
    if path.file_name() != Some(std::ffi::OsStr::new(".git")) {
        return None;
    }
    path.parent().map(Path::to_path_buf)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_gitdir_file_names_the_working_tree_above_its_dot_git() {
        assert_eq!(
            worktree_path_from_gitdir(b"/srv/wt-a/.git\n"),
            Some(PathBuf::from("/srv/wt-a"))
        );
        // Trailing whitespace is git's own newline, not part of the path.
        assert_eq!(
            worktree_path_from_gitdir(b"  /srv/wt-a/.git  "),
            Some(PathBuf::from("/srv/wt-a"))
        );
    }

    /// The shapes that are not a registration this build can interpret. Each
    /// would otherwise become a row naming a directory nobody chose.
    #[test]
    fn an_uninterpretable_gitdir_file_names_nothing() {
        for bad in [
            &b""[..],
            b"   \n",
            b"../wt-a/.git",
            b"relative/.git",
            b"/srv/wt-a",
            b"/srv/wt-a/notgit",
        ] {
            assert_eq!(
                worktree_path_from_gitdir(bad),
                None,
                "must not interpret {:?}",
                String::from_utf8_lossy(bad)
            );
        }
    }

}
