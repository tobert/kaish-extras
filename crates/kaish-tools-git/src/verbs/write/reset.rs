//! `git reset --soft|--mixed <rev>` and `git reset <rev> -- <paths>`
//! (git-write-profile.md §3, §4).
//!
//! `--soft` moves the branch and nothing else. `--mixed` (the default) moves
//! the branch and puts the whole index back to the target's tree. Neither
//! writes the working tree. Both are a branch move, so both go through
//! [`rewrite::check_move`] before anything is written. Like git, both write
//! `ORIG_HEAD` (the old tip) and the reflog line `reset: moving to <rev as
//! typed>` in the branch's log and HEAD's, even when the tip does not change.
//!
//! The order is a commit's: plan and lock the index, move the ref with a
//! compare-and-swap, then write the index. A failure before the move leaves
//! the index and the branch as they were. `ORIG_HEAD` is written first, as git
//! does, so a reset that fails at the move leaves only that pointer behind.
//!
//! `reset <rev> -- <paths>` copies those index entries from `<rev>`'s tree. It
//! moves no branch, so no policy applies and nothing is written but the index.

use gix_index::hash::ObjectId;
use gix_ref::transaction::{Change, LogChange, PreviousValue, RefEdit, RefLog};
use gix_ref::Target;

use crate::config::{Identity, RewritePolicy};
use crate::error::GitError;
use crate::hooks::GitHooks;
use crate::verbs::write::commit::hook_present;
use crate::verbs::write::containment::Leaf;
use crate::verbs::write::head;
use crate::verbs::write::ident::{self, KernelVars, Signature};
use crate::verbs::write::refmove::{self, MoveKind};
use crate::verbs::write::refusal::{refuse, Override};
use crate::verbs::write::repo::WriteRepo;
use crate::verbs::write::rewrite::{self, MoveCheck, Mover, NewTip};
use crate::verbs::write::spec::Outcome;
use crate::verbs::write::state::{ensure_reflog_on, ensure_writable, ensure_writable_with, hooks_dir};
use crate::verbs::write::unstage::{self, UnstageOptions};

const OP: &str = "reset";

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum Mode {
    Soft,
    Mixed,
}

impl Mode {
    fn text(self) -> &'static str {
        match self {
            Mode::Soft => "soft",
            Mode::Mixed => "mixed",
        }
    }
}

/// What the embedder installed that a reset consults.
pub(crate) struct ResetEnv {
    pub(crate) hooks: Option<std::sync::Arc<dyn GitHooks>>,
    pub(crate) identity: Option<(Identity, Identity)>,
    pub(crate) policy: RewritePolicy,
}

pub(crate) struct ResetOptions {
    /// The revision as typed: it goes into the reflog message unchanged.
    pub(crate) rev: String,
    pub(crate) mode: Mode,
    pub(crate) vars: KernelVars,
    pub(crate) command: String,
}

pub(crate) fn run(repo: &WriteRepo, env: &ResetEnv, opts: &ResetOptions) -> Result<Outcome, GitError> {
    let cmd = opts.command.as_str();
    match opts.mode {
        Mode::Mixed => {
            ensure_writable(repo, cmd)?;
        }
        Mode::Soft => {
            ensure_writable_with(repo, cmd, false)?;
        }
    }
    ensure_reflog_on(repo, cmd)?;
    if env.hooks.is_none() {
        let dir = hooks_dir(repo, cmd)?;
        if hook_present(&dir, "reference-transaction") {
            return refuse(
                4,
                cmd,
                "The repository has a reference-transaction hook, and this shell runs no hooks.",
                "Use real git for this reset, outside this shell, or ask the embedder for a hook provider.",
                Override::Embedder,
            );
        }
    }

    let head_state = repo.read().head()?;
    let old = head_state
        .oid
        .as_deref()
        .map(|hex| ObjectId::from_hex(hex.as_bytes()).map_err(|e| GitError::repository(OP, "reading HEAD", repo.git_dir(), e)))
        .transpose()?;
    let target = refmove::head_target(repo, OP)?;
    let new = repo.read().resolve_commit(&opts.rev)?;

    rewrite::check_move(
        repo,
        &MoveCheck { command: cmd, mover: Mover::Reset { rev: &opts.rev }, policy: env.policy },
        old,
        &NewTip::Existing(new),
    )?;
    refmove::precheck(repo, cmd, &target)?;
    precheck_orig_head(repo, cmd)?;
    let committer = ident::resolve_committer(cmd, env.identity.as_ref(), &opts.vars.clone(), ident::now())?;

    let prepared = match opts.mode {
        Mode::Soft => None,
        Mode::Mixed => {
            let map = head::flatten_commit(repo, OP, new)?;
            Some(unstage::prepare(
                repo,
                &UnstageOptions { specs: Vec::new(), operation: OP, command: opts.command.clone() },
                &map,
            )?)
        }
    };

    if let Some(old) = old {
        write_orig_head(repo, old, &committer)?;
    }
    let message = format!("reset: moving to {}", opts.rev);
    refmove::move_head_as(repo, MoveKind::RESET, cmd, &target, old, new, &message, &committer)?;

    let unstaged = prepared.as_ref().map(unstage::Prepared::paths).unwrap_or_default();
    if let Some(p) = prepared {
        // The branch has moved; a failure here leaves the index as it was.
        p.finish(repo).map_err(|e| GitError::Usage {
            operation: OP,
            message: format!("the branch now names {new}, but the index could not be written afterward ({e}). Run git status; the index still holds the state from before the reset"),
        })?;
    }
    let detached = matches!(target, refmove::HeadTarget::Detached);
    let mut err = String::new();
    if detached {
        err.push_str(&format!(
            "git reset: HEAD is detached and now names {new}. To keep work on a branch: git switch -c <name>.\n"
        ));
    }
    Ok(Outcome {
        text: String::new(),
        err,
        json: serde_json::json!({
            "head": new.to_string(),
            "previous": old.map(|o| o.to_string()),
            "mode": opts.mode.text(),
            "branch": head_state.branch,
            "unstaged": unstaged,
        }),
    })
}

/// `git reset <rev> -- <paths>`: those index entries from `<rev>`'s tree.
pub(crate) fn run_paths(repo: &WriteRepo, rev: &str, specs: Vec<String>, command: String) -> Result<Outcome, GitError> {
    ensure_writable(repo, &command)?;
    let commit = repo.read().resolve_commit(rev)?;
    let map = head::flatten_commit(repo, OP, commit)?;
    unstage::prepare(repo, &UnstageOptions { specs, operation: OP, command }, &map)?.finish(repo)
}

fn precheck_orig_head(repo: &WriteRepo, command: &str) -> Result<(), GitError> {
    let checked = repo.check_ref_write("ORIG_HEAD", false)?;
    for p in checked.iter().filter(|p| p.extension().is_some_and(|e| e == "lock")) {
        if std::fs::symlink_metadata(p).is_ok() && p.ends_with("ORIG_HEAD.lock") {
            return refuse(
                4,
                command,
                "The lock file 'ORIG_HEAD.lock' already exists in the git directory, so another git process may be running. This shell does not remove a lock it did not take.",
                "Wait and run the same command again. If no other git process is running, remove the lock with real git's environment: rm .git/ORIG_HEAD.lock.",
                Override::Repository,
            );
        }
    }
    Ok(())
}

/// Point `ORIG_HEAD` at the tip a reset leaves, as git does. Its reflog is
/// appended to only when one exists, which gix-ref decides as git does.
fn write_orig_head(repo: &WriteRepo, old: ObjectId, committer: &Signature) -> Result<(), GitError> {
    repo.check(std::path::Path::new("ORIG_HEAD"), Leaf::File)?;
    let edit = RefEdit {
        change: Change::Update {
            log: LogChange { mode: RefLog::AndReference, force_create_reflog: false, message: "updating ORIG_HEAD".into() },
            expected: PreviousValue::Any,
            new: Target::Object(old),
        },
        name: "ORIG_HEAD"
            .try_into()
            .map_err(|e| GitError::repository(OP, "naming ORIG_HEAD", repo.git_dir(), std::io::Error::other(format!("{e}"))))?,
        deref: false,
    };
    let time = committer.time_text();
    let sig = gix_actor::SignatureRef {
        name: committer.name.as_str().into(),
        email: committer.email.as_str().into(),
        time: &time,
    };
    repo.refs()
        .transaction()
        .prepare([edit], gix_lock::acquire::Fail::Immediately, gix_lock::acquire::Fail::Immediately)
        .map_err(|e| GitError::repository(OP, "locking ORIG_HEAD", repo.git_dir(), e))?
        .commit(Some(sig))
        .map_err(|e| GitError::repository(OP, "writing ORIG_HEAD", repo.git_dir(), e))?;
    Ok(())
}
