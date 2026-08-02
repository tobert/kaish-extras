//! `git info` — the honesty verb (architecture.md B.1).
//!
//! What am I looking at, and what am I allowed to do. It is the verb an agent
//! should reach for first, and the one that catches an unreadable repository
//! (a reftable backend, an unknown extension) before any other verb has a
//! chance to answer from data it did not understand.

use clap::Parser;

use kaish_tool_api::GlobalFlags;

use crate::error::GitError;
use crate::model::{Capabilities, RepoInfo};
use crate::repo::ReadRepo;

/// `git info`'s argv surface.
#[derive(Parser, Debug)]
#[command(
    name = "info",
    about = "Report what repository this is and what this build will let you ask of it"
)]
pub(crate) struct InfoArgs {
    /// Repository to inspect. Defaults to the current directory; discovery
    /// searches upward, never past the mount root that contains the path.
    #[arg(long = "repo", value_name = "PATH")]
    pub repo: Option<String>,

    #[command(flatten)]
    pub global: GlobalFlags,

    /// Validation-only sink: `ToolArgs::to_argv()` always emits `--` before
    /// positionals, and `info` takes none. Read nothing off this field.
    #[arg(hide = true)]
    pub operands: Vec<String>,
}

/// Gather everything `git info` reports.
///
/// Runs entirely inside the caller's blocking closure and returns an owned
/// model: no gix value escapes, so E.3's "no gix value crosses an `.await`"
/// holds by construction rather than by review.
pub(crate) fn run(
    repo: &ReadRepo,
    repo_root_vfs: Option<String>,
    capabilities: Capabilities,
) -> Result<RepoInfo, GitError> {
    Ok(RepoInfo {
        repo_root_vfs,
        repo_root_real: repo.root().display().to_string(),
        git_dir: repo.git_dir().display().to_string(),
        bare: repo.is_bare(),
        shallow: repo.is_shallow(),
        ref_backend: repo.ref_backend(),
        head: repo.head()?,
        worktrees: repo.worktree_count()?,
        submodules: repo.submodule_count()?,
        gix_pins: crate::gix_pins(),
        capabilities,
    })
}
