//! The hook boundary of the commit profile (docs/design/git-write-profile.md
//! §5.1).
//!
//! This build cannot spawn a process, so it cannot run a repository's hook
//! scripts. Without a provider, a write refuses when the repository has a hook
//! git would run for it. An embedder that can honor hooks installs a
//! [`GitHooks`] through `WritableGitConfig::with_hooks`: it may spawn the real
//! scripts, run its own checks, or decline.
//!
//! `commit` calls them, in git's order: `pre_commit` once the tree is written
//! and before the commit object is, `commit_msg` on the message, then
//! `reference_transaction` around the branch move, `post_commit` after it, and
//! for `--amend` `post_rewrite`. `branch`, `tag`, `reset`, `switch -c` and
//! `checkout -b` call `reference_transaction` around each ref they write;
//! the two switches then call `post_checkout`.
//!
//! Every method is required. A provider that has nothing to do for a hook
//! says so by returning `Ok`, and no default does that on its behalf.
//! `prepare-commit-msg` and `post-index-change` have no method. For `commit`,
//! a repository that has one lists its name in [`HookContext::hooks_present`]
//! and the provider decides what to do about it. The index verbs (`add`, `rm`,
//! `mv`, `restore --staged` and `reset` without `--soft`) refuse a repository
//! that has a `post-index-change` hook whether or not a provider is installed,
//! and every verb refuses a `core.hooksPath` that points outside the working
//! tree (docs/issues.md, W21, W50 and W51).

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
    /// The tree the commit will have, as 40 lowercase hex digits, for a verb
    /// that makes a commit (`commit`, `commit --amend`). `None` for `reset`,
    /// `branch`, `tag`, `switch -c` and `checkout -b`, which make none. A
    /// string, like every oid in this crate's public types, so an embedder
    /// needs no gix crate to read it.
    pub tree: Option<&'a str>,
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
///
/// The verb knows which hook it called, so the error carries no hook name.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("hook refused: {message}")]
#[non_exhaustive]
pub struct HookError {
    /// Why it refused, in words a caller can act on.
    pub message: String,
}

impl HookError {
    /// A refusal with `message`.
    pub fn new(message: impl Into<String>) -> Self {
        Self { message: message.into() }
    }
}

/// Where a ref transaction is, as git's `reference-transaction` hook is told.
///
/// Git runs the hook in these four states, in this order, for each
/// transaction. A provider that forwards to the repository's hook script can
/// run it for every one of them. `Preparing` and `Prepared` are the two where
/// `Err` refuses.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum RefTransactionState {
    /// The transaction is about to start and no ref is locked. A move of the
    /// current branch is named `HEAD` here, as git names it (`Prepared` and
    /// later name the branch). `Err` refuses the command exactly as at
    /// `Prepared`: it stops, leaves `.git` as it was (apart from objects
    /// already written) and says so. [`Aborted`](Self::Aborted) follows.
    Preparing,
    /// Every check this crate makes has passed and nothing is written yet.
    /// `Err` refuses the command as at `Preparing`. This is sent before any ref
    /// lock is taken, where git sends `prepared` after it has locked the refs:
    /// a lock that another process holds can still fail the write after this
    /// state, and [`Aborted`](Self::Aborted) follows (docs/issues.md W48).
    Prepared,
    /// The refs in the transaction were written. `Err` here is shown on
    /// stderr and changes nothing, as git ignores the hook's exit status.
    Committed,
    /// A transaction that was sent as [`Preparing`](Self::Preparing) did not
    /// happen: a provider refused it (this one, or the call for a sibling
    /// transaction of the same command), or the write failed afterward, for
    /// example on a lock another process took. None of its refs were
    /// written. Its updates are those of the last state sent for the
    /// transaction. It is never sent for a command refused before `Preparing`.
    /// A transaction whose refs were written is sent
    /// [`Committed`](Self::Committed) even when the command then fails, for
    /// example when the `logs/HEAD` line cannot be appended after the branch
    /// moved.
    /// `Err` here is shown on stderr and changes nothing.
    Aborted,
}

impl RefTransactionState {
    /// The word git passes as the hook's argument: `preparing`, `prepared`,
    /// `committed` or `aborted`.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Preparing => "preparing",
            Self::Prepared => "prepared",
            Self::Committed => "committed",
            Self::Aborted => "aborted",
        }
    }
}

/// One ref in a transaction, in the form of a line on the hook's standard
/// input: `<old> <new> <ref>`.
///
/// `old` and `new` are 40 hex digits, with 40 zeros for a ref that does not
/// exist yet. `new` is `ref:<full name>` for a `HEAD` that is made to name a
/// branch, as git writes it. `name` is a full ref name (`refs/heads/main`), or
/// `HEAD` for a detached HEAD and for the move of HEAD itself. Built by this
/// crate; an embedder reads it.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct RefUpdate {
    /// The value before the update.
    pub old: String,
    /// The value after it.
    pub new: String,
    /// The ref.
    pub name: String,
}

impl RefUpdate {
    pub(crate) fn new(old: impl Into<String>, new: impl Into<String>, name: impl Into<String>) -> Self {
        Self { old: old.into(), new: new.into(), name: name.into() }
    }

    /// The line as git feeds it to the hook, without the newline.
    pub fn line(&self) -> String {
        format!("{} {} {}", self.old, self.new, self.name)
    }
}

/// A commit that a rewrite replaced: a line on `post-rewrite`'s standard
/// input is `<old> <new>`.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct RewrittenCommit {
    /// The commit that was replaced, as 40 hex digits.
    pub old: String,
    /// The commit that replaced it, as 40 hex digits.
    pub new: String,
}

impl RewrittenCommit {
    pub(crate) fn new(old: impl Into<String>, new: impl Into<String>) -> Self {
        Self { old: old.into(), new: new.into() }
    }

    /// The line as git feeds it to the hook, without the newline.
    pub fn line(&self) -> String {
        format!("{} {}", self.old, self.new)
    }
}

/// An embedder's implementation of git's hooks.
///
/// Every method is required: a provider decides each one. The write verbs
/// call them whether or not the repository has a hook file of that name.
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

    /// Git's `reference-transaction` hook, once per state of each ref
    /// transaction a verb makes. `updates` are the lines of the hook's
    /// standard input for that transaction in that state.
    ///
    /// A command that makes more than one transaction sends
    /// [`RefTransactionState::Preparing`] for every one of them, in git's
    /// order, then `Prepared` for every one, all before it writes anything,
    /// and then sends `Committed` for each as it is written. Git instead
    /// runs all states of the first transaction before it starts the second;
    /// see docs/issues.md W21 for the order per verb. `reset` makes two
    /// transactions when HEAD has a commit: `ORIG_HEAD`, then the branch (or
    /// `HEAD` when detached). `switch -c` and `checkout -b` make two when HEAD
    /// has a commit: the new branch, then `HEAD`. `commit`, `commit --amend`,
    /// `branch` and `tag` make one. Git's transaction on the `AUTO_MERGE`
    /// ref, which changes nothing, is not sent.
    ///
    /// `Err` at `Preparing` or `Prepared` refuses the command: the provider is
    /// then sent `Aborted` for every transaction it was sent `Preparing` for,
    /// and the command leaves `.git` as it was apart from objects already
    /// written. At `Committed` and `Aborted`, `Err` is shown on stderr and
    /// changes nothing.
    fn reference_transaction(
        &self,
        ctx: &HookContext<'_>,
        state: RefTransactionState,
        updates: &[RefUpdate],
    ) -> Result<HookOutput, HookError>;

    /// Git's `post-checkout` hook, after `switch -c` or `checkout -b` moved
    /// HEAD to a new branch. `previous` and `new` are the commit HEAD was on
    /// and the commit it is on now (the same one, since these verbs write no
    /// files), and `branch_checkout` is git's third argument: true for a
    /// branch checkout. It is not called when HEAD has no commit yet. Errors
    /// are reported, not fatal, as in git.
    fn post_checkout(&self, ctx: &HookContext<'_>, previous: &str, new: &str, branch_checkout: bool) -> HookOutput;

    /// Git's `post-rewrite` hook, after a command replaced commits.
    /// `command` is git's argument (`amend` for `commit --amend`) and
    /// `rewritten` the lines of its standard input. `commit --amend` is the
    /// only verb here that calls it: `reset` does not. Called after
    /// `post_commit`, as git does. Errors are reported, not fatal.
    fn post_rewrite(&self, ctx: &HookContext<'_>, command: &str, rewritten: &[RewrittenCommit]) -> HookOutput;
}
