//! `git branch <name> [<start>]` (git-write-profile.md §4, §6): create only.
//!
//! The listing forms stay in `verbs/branch.rs`. This file adds the argv a
//! tool with the commit profile accepts beyond them, and the creation.
//!
//! The ref is written with a compare-and-swap on "does not exist", after our
//! own look at the name: gix-ref passes that swap for a ref that exists at the
//! same oid and reads a corrupt loose ref as absent (feasibility §4).
//!
//! The reflog line is git's: `branch: Created from <start as typed>`. With no
//! start git writes the current branch's name, or `HEAD` when HEAD is
//! detached, which differs from the design's first draft (`Created from
//! HEAD`); `tests/write_refs.rs` compares the log bytes with git's.

use std::sync::Arc;

use clap::Parser;

use crate::config::Identity;
use crate::error::GitError;
use crate::hooks::GitHooks;
use crate::verbs::branch::BranchArgs;
use crate::verbs::write::hookrun::{self, Scan, Tx};
use crate::verbs::write::ident::{self, KernelVars};
use crate::verbs::write::refmove::{self, MoveKind};
use crate::hooks::RefUpdate;
use crate::verbs::write::refname::{self, RefKind};
use crate::verbs::write::refusal::{refuse, Override};
use crate::verbs::write::repo::WriteRepo;
use crate::verbs::write::spec::Outcome;
use crate::verbs::write::state::{config_last, ensure_reflog_on};

const OP: &str = "branch";

/// What the embedder installed that a ref write consults.
pub(crate) struct RefsEnv {
    pub(crate) hooks: Option<Arc<dyn GitHooks>>,
    pub(crate) identity: Option<(Identity, Identity)>,
}

/// `git branch` under the commit profile: the listing flags, plus the
/// creation flag and the spellings that are refused with a next step.
#[derive(Parser, Debug)]
#[command(
    name = "branch",
    about = "List the repository's branches, or create one with git branch <name> [<start>]. Creates only: it never deletes, renames or copies a branch, never moves one, and never writes branch.* configuration"
)]
pub(crate) struct BranchWriteArgs {
    #[command(flatten)]
    pub read: BranchArgs,

    #[command(flatten)]
    pub extra: BranchExtra,
}

/// The flags beyond the listing ones.
#[derive(clap::Args, Debug)]
pub(crate) struct BranchExtra {
    /// Create the branch without upstream tracking. This is the only form this
    /// shell makes: a start point that is a remote-tracking branch would make
    /// git write tracking configuration, which this shell never writes.
    #[arg(long = "no-track")]
    pub no_track: bool,

    /// Refused: this shell never deletes a branch.
    #[arg(short = 'd', long = "delete")]
    pub delete: bool,

    /// Refused: this shell never deletes a branch.
    #[arg(short = 'D')]
    pub delete_force: bool,

    /// Refused: this shell never renames a branch.
    #[arg(short = 'm', long = "move")]
    pub rename: bool,

    /// Refused: this shell never renames a branch.
    #[arg(short = 'M')]
    pub rename_force: bool,

    /// Refused: this shell never copies a branch.
    #[arg(short = 'c', long = "copy")]
    pub copy: bool,

    /// Refused: this shell never copies a branch.
    #[arg(short = 'C')]
    pub copy_force: bool,

    /// Refused: this shell never moves an existing branch.
    #[arg(short = 'f', long = "force")]
    pub force: bool,

    /// Refused: this shell never writes branch configuration.
    #[arg(short = 'u', long = "set-upstream-to", value_name = "UPSTREAM")]
    pub set_upstream: Option<String>,

    /// Refused: this shell never writes branch configuration.
    #[arg(long = "unset-upstream")]
    pub unset_upstream: bool,

    /// Refused: this shell never writes branch configuration.
    #[arg(short = 't', long = "track", num_args = 0..=1, require_equals = true, value_name = "MODE")]
    pub track: Option<Option<String>>,
}

impl BranchExtra {
    /// The refusal for a flag this shell will not act on, naming the next
    /// step, or `None`.
    pub(crate) fn refusal(&self, command: &str) -> Option<GitError> {
        let (why, next): (&str, String) = if self.delete || self.delete_force {
            (
                "git branch -d deletes a branch, and this shell creates branches only.",
                "List the branches with git branch. To delete one, use real git, outside this shell: git branch -d <name>.".to_string(),
            )
        } else if self.rename || self.rename_force {
            (
                "git branch -m renames a branch, and this shell creates branches only.",
                "Create a branch under the new name: git branch <new-name> <old-name>. Removing the old one takes real git, outside this shell.".to_string(),
            )
        } else if self.copy || self.copy_force {
            (
                "git branch -c copies a branch with its reflog, and this shell creates branches only.",
                "Create a branch at the same commit: git branch <new-name> <old-name>.".to_string(),
            )
        } else if self.force {
            (
                "git branch -f moves a branch that exists, which can drop commits, and this shell creates branches only.",
                "Create a branch under a new name: git branch <new-name> <start>.".to_string(),
            )
        } else if self.set_upstream.is_some() || self.unset_upstream || self.track.is_some() {
            (
                "Upstream tracking is written into the repository configuration, and this shell never writes configuration.",
                "Create the branch without tracking: git branch --no-track <name> <start>.".to_string(),
            )
        } else {
            return None;
        };
        Some(refuse::<()>(2, command, why, next, Override::Never).unwrap_err())
    }
}

pub(crate) struct BranchCreate {
    pub(crate) name: String,
    pub(crate) start: Option<String>,
    pub(crate) no_track: bool,
    pub(crate) vars: KernelVars,
    pub(crate) command: String,
}

/// Read which of the hooks `names` the repository has. Without a provider, a
/// hook file git would run for this write refuses it: `what` completes "Use
/// real git for this ...". With one, the provider is called and decides.
pub(crate) fn scan_ref_hooks(repo: &WriteRepo, provider: bool, command: &str, names: &[&str], what: &str) -> Result<Scan, GitError> {
    let scan = Scan::read(repo, command, names)?;
    if !provider {
        if let Some(name) = scan.present.first() {
            return refuse(
                4,
                command,
                format!("The repository has a {name} hook, and this shell runs no hooks."),
                format!("Use real git for this {what}, outside this shell, or ask the embedder for a hook provider."),
                Override::Embedder,
            );
        }
    }
    Ok(scan)
}

/// Whether git would write tracking configuration for a branch started at
/// `start` (`branch.autoSetupMerge`): by default when `start` is a
/// remote-tracking branch, with `always` and `inherit` more often.
pub(crate) fn tracking_implied(repo: &WriteRepo, start: &str) -> Result<bool, GitError> {
    let setting = config_last(repo, "branch", "autosetupmerge")?;
    match setting.as_deref().map(str::to_ascii_lowercase).as_deref() {
        Some("false" | "no" | "off" | "0") => return Ok(false),
        Some("always" | "inherit") => return Ok(true),
        _ => {}
    }
    let full = if start.starts_with("refs/remotes/") { start.to_string() } else { format!("refs/remotes/{start}") };
    // A start like `HEAD~1` is no ref name, so it is no remote-tracking branch.
    if gix_ref::FullName::try_from(full.as_str()).is_err() {
        return Ok(false);
    }
    Ok(refname::exists(repo, &full)?.is_some())
}

/// The refusal for a start point that would set up tracking.
pub(crate) fn tracking_refusal(command: &str, start: &str, name: &str) -> GitError {
    refuse::<()>(
        4,
        command,
        format!("'{start}' is a remote-tracking branch (or branch.autoSetupMerge asks for it), so git would write tracking configuration for '{name}', and this shell never writes configuration."),
        format!("Create the branch without tracking: git branch --no-track {name} {start}."),
        Override::Repository,
    )
    .unwrap_err()
}

/// The refusal for a branch name that is already taken.
pub(crate) fn exists_refusal(command: &str, name: &str) -> GitError {
    refuse::<()>(
        1,
        command,
        format!("A branch named '{name}' already exists."),
        format!("Choose another name, for example: git branch {name}-2. To see what '{name}' names: git log -n 1 {name}."),
        Override::Never,
    )
    .unwrap_err()
}

/// The refusal for a name whose ref cannot be made beside another ref.
pub(crate) fn blocked_refusal(command: &str, word: &str, name: &str, why: &str) -> GitError {
    refuse::<()>(
        1,
        command,
        format!("The {word} '{name}' cannot be created: {why}. Git stores a {word} as a file, so a name cannot also be a directory of {word}s."),
        format!("Choose a name that does not start with an existing {word} and is not the start of one."),
        Override::Never,
    )
    .unwrap_err()
}

/// The refusal for a HEAD with no commit to start from.
pub(crate) fn unborn_refusal(command: &str, branch: Option<&str>) -> GitError {
    let on = branch.map_or_else(|| "HEAD".to_string(), |b| format!("branch '{b}'"));
    refuse::<()>(
        1,
        command,
        format!("{on} has no commits yet, so there is no commit to start from."),
        "Make the first commit, then create the branch: git commit -m '<message>'.",
        Override::Never,
    )
    .unwrap_err()
}

pub(crate) fn run(repo: &WriteRepo, env: &RefsEnv, opts: &BranchCreate) -> Result<Outcome, GitError> {
    let cmd = opts.command.as_str();
    let full = refname::full_name(RefKind::Branch, cmd, &opts.name)?;
    ensure_reflog_on(repo, cmd)?;
    let scan = scan_ref_hooks(repo, env.hooks.is_some(), cmd, &["reference-transaction"], "branch")?;

    let head = repo.read().head()?;
    let head_oid = head
        .oid
        .as_deref()
        .map(|hex| {
            gix_index::hash::ObjectId::from_hex(hex.as_bytes())
                .map_err(|e| GitError::repository(OP, "reading HEAD", repo.git_dir(), e))
        })
        .transpose()?;
    let (start_oid, shown) = match &opts.start {
        Some(start) => {
            if !opts.no_track && tracking_implied(repo, start)? {
                return Err(tracking_refusal(cmd, start, &opts.name));
            }
            (repo.read().resolve_commit(start)?, start.clone())
        }
        None => match head_oid {
            Some(oid) => (oid, head.branch.clone().unwrap_or_else(|| "HEAD".to_string())),
            None => return Err(unborn_refusal(cmd, head.branch.as_deref())),
        },
    };

    if refname::exists(repo, &full)?.is_some() {
        return Err(exists_refusal(cmd, &opts.name));
    }
    if let Some(why) = refname::blocked_by(repo, &full)? {
        return Err(blocked_refusal(cmd, "branch", &opts.name, &why));
    }
    let committer = ident::resolve_committer(cmd, env.identity.as_ref(), &opts.vars, ident::now())?;
    let message = format!("branch: Created from {shown}");
    let mut err = String::new();
    let tx: Tx = vec![RefUpdate::new(hookrun::null(), start_oid.to_string(), full.clone())];
    let ctx = scan.context(repo);
    if let Some(h) = env.hooks.as_ref() {
        hookrun::prepared(h.as_ref(), &ctx, MoveKind::BRANCH, cmd, std::slice::from_ref(&tx), &mut err)?;
    }
    if let Err(e) = refmove::create_ref(repo, MoveKind::BRANCH, cmd, &full, start_oid, Some((&message, &committer))) {
        if let Some(h) = env.hooks.as_ref() {
            hookrun::aborted(h.as_ref(), &ctx, &tx, &mut err);
        }
        return Err(e);
    }
    if let Some(h) = env.hooks.as_ref() {
        hookrun::committed(h.as_ref(), &ctx, &tx, &mut err);
    }
    Ok(Outcome {
        text: String::new(),
        err,
        json: serde_json::json!({ "branch": opts.name, "ref": full, "commit": start_oid.to_string(), "start": shown }),
    })
}
