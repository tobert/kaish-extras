//! The hook boundary of the commit profile (docs/design/git-write-profile.md
//! §5.1).
//!
//! This build cannot spawn a process, so it cannot run a repository's hook
//! scripts. Without a provider, a write refuses when the repository has a hook
//! git would run for it. An embedder that can honor hooks installs a
//! [`GitHooks`] through `WritableGitConfig::with_hooks`: it may spawn the real
//! scripts, run its own checks, or decline.
//!
//! Phase 1a defines the types only. No verb calls them yet.

use std::path::Path;

/// What a hook is told about the write in progress.
///
/// Carries the repository's real paths, so only an embedder that already
/// holds host access should spawn from them. Built by this crate; an
/// embedder reads it.
#[derive(Debug, Clone, Copy)]
#[non_exhaustive]
pub struct HookContext<'a> {
    /// The working tree root, or `None` for a bare repository.
    pub worktree: Option<&'a Path>,
    /// The git directory (per-worktree in a linked worktree).
    pub git_dir: &'a Path,
    /// The directory hooks were resolved from: `core.hooksPath` when the
    /// repository sets it, else `<common dir>/hooks`.
    pub hooks_dir: &'a Path,
    /// The tree the commit will have, as 40 lowercase hex digits. A string,
    /// like every oid in this crate's public types, so an embedder needs no
    /// gix crate to read it.
    pub tree: &'a str,
    /// The names of the hook files present that git would run for this
    /// write, for example `pre-commit`. Never a `*.sample` file.
    pub hooks_present: &'a [String],
    /// Whether the caller passed `--no-verify`. A provider decides what that
    /// means; this crate does not skip a hook it cannot run.
    pub no_verify: bool,
}

/// Text a hook produced, relayed to the caller.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
#[non_exhaustive]
pub struct HookOutput {
    /// Shown to the caller on stderr.
    pub message: String,
}

impl HookOutput {
    /// Output with no text.
    pub fn none() -> Self {
        Self::default()
    }

    /// Output carrying `message`.
    pub fn with_message(message: impl Into<String>) -> Self {
        Self { message: message.into() }
    }
}

/// A hook that refused the write. Its text is the refusal the caller sees.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("hook {hook} refused: {message}")]
#[non_exhaustive]
pub struct HookError {
    /// The hook's name, for example `pre-commit`.
    pub hook: String,
    /// Why it refused, in words a caller can act on.
    pub message: String,
}

impl HookError {
    /// A refusal by `hook` with `message`.
    pub fn new(hook: impl Into<String>, message: impl Into<String>) -> Self {
        Self { hook: hook.into(), message: message.into() }
    }
}

/// An embedder's implementation of git's commit hooks.
pub trait GitHooks: Send + Sync {
    /// Called before the commit object is written. `Err` aborts the commit
    /// and its text is the refusal.
    fn pre_commit(&self, ctx: &HookContext<'_>) -> Result<HookOutput, HookError>;

    /// May rewrite the message, as git's `commit-msg` hook may.
    fn commit_msg(
        &self,
        ctx: &HookContext<'_>,
        message: &mut String,
    ) -> Result<HookOutput, HookError>;

    /// Called after the ref moved. Errors are reported, not fatal, as in git.
    fn post_commit(&self, ctx: &HookContext<'_>, commit: &str) -> HookOutput;
}
