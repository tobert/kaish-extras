//! Moving the checked-out branch (or a detached HEAD) to a new commit
//! (git-write-profile.md §6).
//!
//! One `gix-ref` transaction with a compare-and-swap on the oid the caller
//! read: `MustExistAndMatch` for a branch that has a commit, `MustNotExist`
//! for an unborn one. A branch that moved in between fails the transaction,
//! and the caller gets a refusal naming both oids; there is no retry loop.
//!
//! Every path the transaction touches goes through
//! [`WriteRepo::check_ref_write`] first, so a planted link at the ref, its
//! lock, its reflog, `logs/HEAD` or `packed-refs` refuses the move before a
//! byte is written.

use gix_index::hash::ObjectId;
use gix_ref::transaction::{Change, LogChange, PreviousValue, RefEdit, RefLog};
use gix_ref::Target;

use crate::error::GitError;
use crate::verbs::write::ident::Signature;
use crate::verbs::write::refusal::{refuse, Override};
use crate::verbs::write::reflog::validate_message;
use crate::verbs::write::repo::WriteRepo;

/// What HEAD names right now.
pub(crate) enum HeadTarget {
    /// HEAD is a symbolic ref to this full branch name.
    Branch(String),
    /// HEAD holds a commit id.
    Detached,
}

/// Read what HEAD points at, from the store that will write.
pub(crate) fn head_target(repo: &WriteRepo, op: &'static str) -> Result<HeadTarget, GitError> {
    let head = repo
        .refs()
        .find("HEAD")
        .map_err(|e| GitError::repository(op, "reading HEAD", repo.git_dir(), e))?;
    Ok(match head.target {
        Target::Symbolic(name) => HeadTarget::Branch(name.as_bstr().to_string()),
        Target::Object(_) => HeadTarget::Detached,
    })
}

/// Move HEAD's branch (or HEAD itself, detached) from `old` to `new`, with
/// `message` in the reflogs of the branch and of HEAD.
pub(crate) fn move_head(
    repo: &WriteRepo,
    command: &str,
    target: &HeadTarget,
    old: Option<ObjectId>,
    new: ObjectId,
    message: &str,
    committer: &Signature,
) -> Result<(), GitError> {
    const OP: &str = "commit";
    let message = validate_message(OP, message)?;
    precheck(repo, command, target)?;
    let (name, deref, refname) = match target {
        HeadTarget::Branch(full) => ("HEAD", true, full.as_str()),
        HeadTarget::Detached => ("HEAD", false, "HEAD"),
    };
    let expected = match old {
        Some(oid) => PreviousValue::MustExistAndMatch(Target::Object(oid)),
        None => {
            // `MustNotExist` passes for a ref that exists at the new value and
            // reads a corrupt loose ref as absent (feasibility §4), so look
            // first. An unborn branch is the only caller that gets here.
            if let HeadTarget::Branch(full) = target {
                let found = repo
                    .refs()
                    .try_find(full.as_str())
                    .map_err(|e| GitError::repository(OP, format!("reading ref '{full}'"), repo.common_dir(), e))?;
                if let Some(existing) = found {
                    return cas_failure(command, refname, None, Some(&existing.target));
                }
            }
            PreviousValue::MustNotExist
        }
    };
    let edit = RefEdit {
        change: Change::Update {
            log: LogChange { mode: RefLog::AndReference, force_create_reflog: false, message: message.into() },
            expected,
            new: Target::Object(new),
        },
        name: name.try_into().map_err(|e| GitError::repository(OP, "naming HEAD", repo.git_dir(), std::io::Error::other(format!("{e}"))))?,
        deref,
    };
    let time = committer.time_text();
    let sig = gix_actor::SignatureRef {
        name: committer.name.as_str().into(),
        email: committer.email.as_str().into(),
        time: &time,
    };
    let prepared = repo
        .refs()
        .transaction()
        .prepare([edit], gix_lock::acquire::Fail::Immediately, gix_lock::acquire::Fail::Immediately)
        .map_err(|e| prepare_error(repo, command, refname, old, e))?;
    prepared
        .commit(Some(sig))
        .map_err(|e| GitError::repository(OP, format!("moving '{refname}'"), repo.common_dir(), e))?;
    Ok(())
}

/// Everything that can refuse a ref move before anything is written: the
/// containment walk over the ref's write set, and the lock files.
pub(crate) fn precheck(repo: &WriteRepo, command: &str, target: &HeadTarget) -> Result<(), GitError> {
    match target {
        HeadTarget::Branch(full) => {
            let checked = repo.check_ref_write(full, true)?;
            refuse_if_locked(repo, command, &checked, full)
        }
        HeadTarget::Detached => {
            let checked = repo.check_ref_write("HEAD", false)?;
            refuse_if_locked(repo, command, &checked, "HEAD")
        }
    }
}

/// Refuse when a lock file for the ref, for HEAD or for `packed-refs` exists.
///
/// gix-ref 0.66.0 cannot be left to report this itself: when a symbolic HEAD
/// is split into HEAD and its branch and the *branch's* lock exists, the
/// error path in `Transaction::prepare_inner` (`while let Some(parent_idx) =
/// cursor`, store/file/transaction/prepare.rs:401) never advances its cursor
/// and spins forever. So the locks are looked at first, with `lstat`. A lock
/// that appears between this look and the transaction still reaches that loop;
/// docs/issues.md W13 records it.
fn refuse_if_locked(
    repo: &WriteRepo,
    command: &str,
    checked: &[std::path::PathBuf],
    refname: &str,
) -> Result<(), GitError> {
    let mut candidates: Vec<std::path::PathBuf> = checked
        .iter()
        .filter(|p| p.extension().is_some_and(|e| e == "lock"))
        .cloned()
        .collect();
    let head_lock = repo.check(std::path::Path::new("HEAD.lock"), crate::verbs::write::containment::Leaf::File)?;
    if !candidates.contains(&head_lock) {
        candidates.push(head_lock);
    }
    for lock in candidates {
        if std::fs::symlink_metadata(&lock).is_ok() {
            let shown = lock
                .strip_prefix(repo.common_dir())
                .or_else(|_| lock.strip_prefix(repo.git_dir()))
                .map_or_else(|_| "a lock file".to_string(), |p| p.display().to_string());
            return refuse(
                4,
                command,
                format!("The lock file '{shown}' already exists in the git directory (moving '{refname}' needs it), so another git process may be running. This shell does not remove a lock it did not take."),
                "Wait and run the same command again. If no other git process is running, remove the lock with real git's environment, for example: rm .git/refs/heads/<branch>.lock.",
                Override::Repository,
            );
        }
    }
    Ok(())
}

fn prepare_error(
    repo: &WriteRepo,
    command: &str,
    refname: &str,
    old: Option<ObjectId>,
    e: gix_ref::file::transaction::prepare::Error,
) -> GitError {
    use gix_ref::file::transaction::prepare::Error as E;
    match e {
        E::ReferenceOutOfDate { actual, .. } => cas_failure(command, refname, old, Some(&actual)).unwrap_err(),
        E::MustNotExist { actual, .. } => cas_failure(command, refname, None, Some(&actual)).unwrap_err(),
        E::MustExist { .. } => cas_failure(command, refname, old, None).unwrap_err(),
        E::LockAcquire { source, .. } | E::PackedTransactionAcquire(source) => {
            let permanent = matches!(source, gix_lock::acquire::Error::PermanentlyLocked { .. });
            if permanent {
                refuse::<()>(
                    4,
                    command,
                    format!("A lock file for '{refname}' already exists in the git directory, so another git process may be running. This shell does not remove a lock it did not take."),
                    "Wait and run the same command again. If no other git process is running, remove the lock with real git's environment, for example: rm .git/refs/heads/<branch>.lock.",
                    Override::Repository,
                )
                .unwrap_err()
            } else {
                GitError::repository("commit", format!("locking '{refname}'"), repo.common_dir(), source)
            }
        }
        other => GitError::repository("commit", format!("moving '{refname}'"), repo.common_dir(), other),
    }
}

fn target_text(t: Option<&Target>) -> String {
    match t {
        Some(Target::Object(id)) => id.to_string(),
        Some(Target::Symbolic(name)) => format!("a symbolic ref to {}", name.as_bstr()),
        None => "absent".to_string(),
    }
}

/// The refusal for a ref that was not what this commit was built on.
fn cas_failure(
    command: &str,
    refname: &str,
    expected: Option<ObjectId>,
    actual: Option<&Target>,
) -> Result<(), GitError> {
    let expected = expected.map_or_else(|| "absent".to_string(), |o| o.to_string());
    refuse(
        1,
        command,
        format!(
            "'{refname}' moved while this commit was being made: it was {expected} when the commit started and is now {}. The commit object is on disk and no ref names it.",
            target_text(actual)
        ),
        "Run git status to see where the branch is now, then make the commit again.",
        Override::Never,
    )
}
