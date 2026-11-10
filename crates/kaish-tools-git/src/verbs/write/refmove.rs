//! Moving the checked-out branch (or a detached HEAD) to a new commit
//! (git-write-profile.md §6).
//!
//! One `gix-ref` transaction with a compare-and-swap on the oid the caller
//! read: `MustExistAndMatch` for a branch that has a commit, `MustNotExist`
//! for an unborn one. A branch that moved in between fails the transaction,
//! and the caller gets a refusal naming both oids; there is no retry loop.
//!
//! The transaction never gets a symbolic `HEAD` edit. gix-ref splits one
//! (`deref: true`) into a `HEAD` edit and a child edit for the branch, and in
//! 0.66.0 a failed lock on the child sends it into a loop that never ends
//! (docs/issues.md W13). So for a branch the one edit names the branch
//! itself, and `HEAD` is handled the way git does it: take `HEAD.lock`, check
//! that `HEAD` still names the branch, move the branch, append the same line
//! to `logs/HEAD`, release the lock without changing `HEAD`.
//!
//! Every path the transaction touches goes through
//! [`WriteRepo::check_ref_write`] first, so a planted link at the ref, its
//! lock, its reflog, `logs/HEAD` or `packed-refs` refuses the move before a
//! byte is written.

use gix_index::hash::ObjectId;
use gix_ref::transaction::{Change, LogChange, PreviousValue, RefEdit, RefLog};
use gix_ref::Target;

use crate::error::{GitError, Wrote};
use crate::verbs::write::ident::Signature;
use crate::verbs::write::refusal::{refuse, Override};
use crate::verbs::write::reflog::copy_message;
use crate::verbs::write::repo::WriteRepo;
use crate::verbs::write::state::reflog_turned_off;

/// What HEAD names right now.
pub(crate) enum HeadTarget {
    /// HEAD is a symbolic ref to this full branch name.
    Branch(String),
    /// HEAD holds a commit id.
    Detached,
}

/// What a ref move is for, so its errors and refusals name the right
/// operation. A reset says "reset", not "commit".
#[derive(Clone, Copy)]
pub(crate) struct MoveKind {
    /// The operation name in repository errors.
    pub(crate) op: &'static str,
    /// Completes "moved while ...": "this commit was being made".
    pub(crate) during: &'static str,
    /// The next step after a lost race.
    pub(crate) again: &'static str,
    /// Says the move happened, for a failure after the branch moved.
    pub(crate) done: &'static str,
    /// The command as a noun, for "the hook refused the ...".
    pub(crate) noun: &'static str,
}

impl MoveKind {
    pub(crate) const COMMIT: MoveKind = MoveKind {
        op: "commit",
        during: "this commit was being made",
        again: "make the commit again",
        done: "the commit was made",
        noun: "commit",
    };
    pub(crate) const RESET: MoveKind = MoveKind {
        op: "reset",
        during: "this reset was running",
        again: "run the reset again",
        done: "the reset was made",
        noun: "reset",
    };
    pub(crate) const BRANCH: MoveKind = MoveKind {
        op: "branch",
        during: "this branch was being created",
        again: "create the branch again",
        done: "the branch was created",
        noun: "branch",
    };
    pub(crate) const TAG: MoveKind = MoveKind {
        op: "tag",
        during: "this tag was being created",
        again: "create the tag again",
        done: "the tag was created",
        noun: "tag",
    };
    pub(crate) const SWITCH: MoveKind = MoveKind {
        op: "switch",
        during: "this switch was running",
        again: "run the switch again",
        done: "the branch was created",
        noun: "switch",
    };
    /// `checkout -b`: the same move as [`Self::SWITCH`], named as typed.
    pub(crate) const CHECKOUT: MoveKind = MoveKind {
        op: "checkout",
        during: "this checkout was running",
        again: "run the checkout again",
        done: "the branch was created",
        noun: "checkout",
    };
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
    move_head_as(repo, MoveKind::COMMIT, command, target, old, new, message, committer)
}

/// [`move_head`] for an operation other than a commit.
#[allow(clippy::too_many_arguments)]
pub(crate) fn move_head_as(
    repo: &WriteRepo,
    kind: MoveKind,
    command: &str,
    target: &HeadTarget,
    old: Option<ObjectId>,
    new: ObjectId,
    message: &str,
    committer: &Signature,
) -> Result<(), GitError> {
    let message = copy_message(message);
    precheck(repo, command, target)?;
    apply_move(repo, kind, command, target, old, new, &message, committer)
}

/// The move itself, after [`precheck`]. Separate so a test can reach the
/// transaction with a lock file the precheck would have refused.
#[allow(clippy::too_many_arguments)]
fn apply_move(
    repo: &WriteRepo,
    kind: MoveKind,
    command: &str,
    target: &HeadTarget,
    old: Option<ObjectId>,
    new: ObjectId,
    message: &str,
    committer: &Signature,
) -> Result<(), GitError> {
    let op = kind.op;
    let refname = match target {
        HeadTarget::Branch(full) => full.as_str(),
        HeadTarget::Detached => "HEAD",
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
                    .map_err(|e| GitError::repository(op, format!("reading ref '{full}'"), repo.common_dir(), e))?;
                if let Some(existing) = found {
                    return cas_failure(kind, command, refname, None, Some(&existing.target));
                }
            }
            PreviousValue::MustNotExist
        }
    };
    // Never `deref: true`: see the module doc.
    let edit = RefEdit {
        change: Change::Update {
            log: LogChange { mode: RefLog::AndReference, force_create_reflog: false, message: message.into() },
            expected,
            new: Target::Object(new),
        },
        name: refname.try_into().map_err(|e| {
            GitError::repository(op, format!("naming '{refname}'"), repo.git_dir(), std::io::Error::other(format!("{e}")))
        })?,
        deref: false,
    };
    let time = committer.time_text();
    let sig = gix_actor::SignatureRef {
        name: committer.name.as_str().into(),
        email: committer.email.as_str().into(),
        time: &time,
    };

    // For a branch, hold HEAD.lock across the move. Dropped without a commit,
    // the Marker removes the lock and leaves HEAD as it was.
    let head_lock = match target {
        HeadTarget::Branch(full) => Some(hold_head(repo, kind, command, full)?),
        HeadTarget::Detached => None,
    };
    let prepared = repo
        .refs()
        .transaction()
        .prepare([edit], gix_lock::acquire::Fail::Immediately, gix_lock::acquire::Fail::Immediately)
        .map_err(|e| prepare_error(kind, command, refname, old, repo, e))?;
    prepared
        .commit(Some(sig))
        .map_err(|e| GitError::repository(op, format!("moving '{refname}'"), repo.common_dir(), e))?;
    if head_lock.is_some() {
        // The branch has already moved. The error must say so, or an agent
        // reads a failure and makes the same commit a second time.
        append_head_reflog(repo, kind, old, new, &sig, message).map_err(|e| match e {
            GitError::Repository { operation, what, path, source, .. } => GitError::Repository {
                operation,
                what: format!(
                    "{} and '{refname}' now points at {new}, so do not run the command again; \
                     only {what} failed (logs/HEAD lacks this entry)",
                    kind.done
                ),
                path,
                source,
                wrote: Wrote::Described,
            },
            other => other,
        })?;
    }
    drop(head_lock);
    Ok(())
}

/// Take `HEAD.lock` and confirm `HEAD` still names `full`.
fn hold_head(repo: &WriteRepo, kind: MoveKind, command: &str, full: &str) -> Result<gix_lock::Marker, GitError> {
    use crate::verbs::write::containment::Leaf;
    let head = repo.check(std::path::Path::new("HEAD"), Leaf::File)?;
    let _ = repo.check(std::path::Path::new("HEAD.lock"), Leaf::File)?;
    let marker = gix_lock::Marker::acquire_to_hold_resource(&head, gix_lock::acquire::Fail::Immediately, None)
        .map_err(|e| lock_error(kind, command, "HEAD.lock", full, "moving", repo, e))?;
    let now = repo
        .refs()
        .find("HEAD")
        .map_err(|e| GitError::repository(kind.op, "reading HEAD", repo.git_dir(), e))?;
    match &now.target {
        Target::Symbolic(name) if name.as_bstr() == full => Ok(marker),
        other => {
            let now = match other {
                Target::Symbolic(name) => format!("a symbolic ref to {}", name.as_bstr()),
                Target::Object(id) => format!("detached at {id}"),
            };
            refuse(
                1,
                command,
                format!("HEAD moved while {}: it named '{full}' when it started and is now {now}.", kind.during),
                format!("Run git status to see where HEAD is now, then {}.", kind.again),
                Override::Never,
            )
        }
    }
}

/// Whether the reflog of `full` is a file already. Git appends to one whatever
/// `core.logAllRefUpdates` says, and creates one only for the refs that
/// setting names. A directory there is not a log (git leaves it alone for a
/// tag); a link or a file with other hard links refuses, as every write does.
pub(crate) fn log_exists(repo: &WriteRepo, full: &str) -> Result<bool, GitError> {
    use crate::verbs::write::containment::Leaf;
    let rel = std::path::PathBuf::from(format!("logs/{full}"));
    let parent = rel.parent().map(|p| repo.check(p, Leaf::Dir)).transpose()?;
    let (Some(parent), Some(name)) = (parent, rel.file_name()) else { return Ok(false) };
    match std::fs::symlink_metadata(parent.join(name)) {
        Ok(m) if m.is_dir() => Ok(false),
        Ok(_) => repo.check(&rel, Leaf::File).map(|_| true),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(e) => Err(GitError::repository(repo.read().operation(), format!("looking at logs/{full}"), repo.common_dir(), e)),
    }
}

/// Create the ref `full` at `new`, which must not exist (git-write-profile.md
/// §6). The caller has already refused a name that exists, with git's wording;
/// the compare-and-swap here closes the race after that look.
///
/// `log` is the reflog line for the ref and the committer to write it as. A
/// branch gets one (gix-ref's autocreate rule is git's: heads, remotes, notes
/// and `HEAD`); a tag passes `None`, and gix-ref writes no log for it, unless
/// the caller found a log file already there (see [`log_exists`]). With
/// `core.logAllRefUpdates` off, git writes the line only to a log file that
/// exists, so `log` is dropped when there is none.
pub(crate) fn create_ref(
    repo: &WriteRepo,
    kind: MoveKind,
    command: &str,
    full: &str,
    new: ObjectId,
    log: Option<(&str, &Signature)>,
) -> Result<(), GitError> {
    let log = if log.is_some() && reflog_turned_off(repo, command)? && !log_exists(repo, full)? { None } else { log };
    let checked = if log.is_some() { repo.check_ref_write(full, false)? } else { repo.check_ref_write_unlogged(full)? };
    refuse_if_ref_locked(repo, command, &checked, full)?;
    let message = log.map(|(m, _)| copy_message(m)).unwrap_or_default();
    let edit = RefEdit {
        change: Change::Update {
            log: LogChange { mode: RefLog::AndReference, force_create_reflog: false, message: message.into() },
            expected: PreviousValue::MustNotExist,
            new: Target::Object(new),
        },
        name: full.try_into().map_err(|e| {
            GitError::repository(kind.op, format!("naming '{full}'"), repo.git_dir(), std::io::Error::other(format!("{e}")))
        })?,
        deref: false,
    };
    let time = log.map(|(_, c)| c.time_text());
    let sig = log.zip(time.as_deref()).map(|((_, c), time)| gix_actor::SignatureRef {
        name: c.name.as_str().into(),
        email: c.email.as_str().into(),
        time,
    });
    let store = if log.is_some() { repo.refs() } else { repo.refs_unlogged() };
    let prepared = store
        .transaction()
        .prepare([edit], gix_lock::acquire::Fail::Immediately, gix_lock::acquire::Fail::Immediately)
        .map_err(|e| prepare_error(kind, command, full, None, repo, e))?;
    prepared
        .commit(sig)
        .map_err(|e| GitError::repository(kind.op, format!("creating '{full}'"), repo.common_dir(), e))?;
    Ok(())
}

/// What `switch -c` does to HEAD (git-write-profile.md §4): after the new
/// branch exists, HEAD names it and `logs/HEAD` records the move.
pub(crate) struct HeadSwitch<'a> {
    pub(crate) kind: MoveKind,
    pub(crate) command: &'a str,
    /// What HEAD named when the caller read it.
    pub(crate) was: &'a HeadTarget,
    /// The commit HEAD was on, or `None` for an unborn branch.
    pub(crate) head_oid: Option<ObjectId>,
    /// The full name of the new branch.
    pub(crate) full: &'a str,
    /// The branch's reflog line, and the committer, when the branch is made.
    /// An unborn HEAD makes no branch: HEAD just names the new one.
    pub(crate) create: Option<(&'a str, &'a Signature)>,
    /// Set to true once the branch exists, so a caller that failed later can
    /// tell the branch transaction from the HEAD one.
    pub(crate) created: Option<&'a std::cell::Cell<bool>>,
}

/// Create the branch (unless HEAD is unborn) and point HEAD at it, holding
/// `HEAD.lock` throughout. Git's order: the branch first, then HEAD, then the
/// `logs/HEAD` line `checkout: moving from <old> to <new>`.
pub(crate) fn switch_head(repo: &WriteRepo, s: &HeadSwitch<'_>) -> Result<(), GitError> {
    use crate::verbs::write::containment::Leaf;
    use std::io::Write;
    let kind = s.kind;
    let checked = repo.check_ref_write(s.full, true)?;
    refuse_if_locked(repo, s.command, &checked, s.full)?;
    let head = repo.check(std::path::Path::new("HEAD"), Leaf::File)?;
    let mut lock = gix_lock::File::acquire_to_update_resource(&head, gix_lock::acquire::Fail::Immediately, None)
        .map_err(|e| lock_error(kind, s.command, "HEAD.lock", s.full, "moving", repo, e))?;
    // HEAD must still be what the caller read: the lock is held, so nothing
    // else changes it from here on.
    let now = repo
        .refs()
        .find("HEAD")
        .map_err(|e| GitError::repository(kind.op, "reading HEAD", repo.git_dir(), e))?;
    let unchanged = match (&now.target, s.was) {
        (Target::Symbolic(n), HeadTarget::Branch(b)) => n.as_bstr() == b.as_str(),
        (Target::Object(o), HeadTarget::Detached) => Some(*o) == s.head_oid,
        _ => false,
    };
    if !unchanged {
        return refuse(
            1,
            s.command,
            format!("HEAD moved while {}: it is now {}.", kind.during, target_text(Some(&now.target))),
            format!("Run git status to see where HEAD is now, then {}.", kind.again),
            Override::Never,
        );
    }
    // With reflogs off, git appends to `logs/HEAD` only when it exists. Asked
    // before the first write: a refusal here must leave the repository as it was.
    let head_log = !reflog_turned_off(repo, s.command)? || log_exists(repo, "HEAD")?;
    if let (Some(oid), Some((message, committer))) = (s.head_oid, s.create) {
        create_ref(repo, kind, s.command, s.full, oid, Some((message, committer)))?;
        if let Some(flag) = s.created {
            flag.set(true);
        }
    }
    let created = s.create.is_some();
    let after_branch = |what: String, source: std::io::Error| -> GitError {
        let what = if created {
            format!("{} and '{}' exists, but {what}", kind.done, s.full)
        } else {
            what
        };
        GitError::repository(kind.op, what, repo.git_dir(), source)
    };
    lock.write_all(format!("ref: {}\n", s.full).as_bytes())
        .map_err(|e| after_branch("HEAD could not be written".to_string(), e))?;
    lock.commit().map_err(|e| after_branch("HEAD could not be replaced".to_string(), e.error))?;
    if let (true, Some(oid), Some((_, committer))) = (head_log, s.head_oid, s.create) {
        let from = match s.was {
            HeadTarget::Branch(b) => b.strip_prefix("refs/heads/").unwrap_or(b).to_string(),
            HeadTarget::Detached => oid.to_string(),
        };
        let to = s.full.strip_prefix("refs/heads/").unwrap_or(s.full);
        let time = committer.time_text();
        let sig = gix_actor::SignatureRef {
            name: committer.name.as_str().into(),
            email: committer.email.as_str().into(),
            time: &time,
        };
        append_head_reflog(repo, kind, Some(oid), oid, &sig, &copy_message(&format!("checkout: moving from {from} to {to}")))
            .map_err(|e| match e {
                GitError::Repository { operation, what, path, source, .. } => GitError::Repository {
                    operation,
                    what: format!(
                        "{} and HEAD now names '{}', so do not run the command again; only {what} failed (logs/HEAD lacks this entry)",
                        kind.done, s.full
                    ),
                    path,
                    source,
                    wrote: Wrote::Described,
                },
                other => other,
            })?;
    }
    Ok(())
}

/// The early check for creating `full`: the containment walk over its write
/// set and a look for its lock files, before any object is written. For a ref
/// that gets no reflog (a tag): its log path is not part of the walk.
pub(crate) fn precheck_ref(repo: &WriteRepo, command: &str, full: &str) -> Result<(), GitError> {
    let checked = repo.check_ref_write_unlogged(full)?;
    refuse_if_ref_locked(repo, command, &checked, full)
}

/// Refuse when a lock file in a ref's write set exists. Unlike
/// [`refuse_if_locked`] it does not look at `HEAD.lock`, which a ref creation
/// does not take.
fn refuse_if_ref_locked(repo: &WriteRepo, command: &str, checked: &[std::path::PathBuf], refname: &str) -> Result<(), GitError> {
    for lock in checked.iter().filter(|p| p.extension().is_some_and(|e| e == "lock")) {
        if std::fs::symlink_metadata(lock).is_ok() {
            let shown = lock
                .strip_prefix(repo.common_dir())
                .or_else(|_| lock.strip_prefix(repo.git_dir()))
                .map_or_else(|_| "a lock file".to_string(), |p| p.display().to_string());
            return Err(lock_refusal(command, &shown, refname, "creating"));
        }
    }
    Ok(())
}

/// Append the line git writes to `logs/HEAD` when HEAD's branch moves:
/// `<old> <new> <committer>\t<message>`, with the null oid as `old` for an
/// unborn branch (the same line gix-ref writes to the branch's own log).
fn append_head_reflog(
    repo: &WriteRepo,
    kind: MoveKind,
    old: Option<ObjectId>,
    new: ObjectId,
    committer: &gix_actor::SignatureRef<'_>,
    message: &str,
) -> Result<(), GitError> {
    use crate::verbs::write::containment::Leaf;
    use std::io::Write;
    let path = repo.check(std::path::Path::new("logs/HEAD"), Leaf::File)?;
    let mut line = format!("{} {} ", old.unwrap_or_else(|| new.kind().null()), new).into_bytes();
    committer
        .trim()
        .write_to(&mut line)
        .map_err(|e| GitError::repository(kind.op, "writing the HEAD reflog line", repo.git_dir(), e))?;
    line.push(b'\t');
    line.extend_from_slice(message.as_bytes());
    line.push(b'\n');
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .map_err(|e| GitError::repository(kind.op, "creating logs/", repo.git_dir(), e))?;
    }
    // One write call on an O_APPEND handle, so a concurrent appender's line
    // cannot land inside ours.
    std::fs::OpenOptions::new()
        .append(true)
        .create(true)
        .open(&path)
        .and_then(|mut f| f.write_all(&line))
        .map_err(|e| GitError::repository(kind.op, "appending to logs/HEAD", repo.git_dir(), e))
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
/// This is the early check, before any object is written, so the refusal
/// comes before blobs and trees do. It does not close the race: a lock taken
/// after this `lstat` is not seen here. That case is closed by [`apply_move`],
/// which hands gix-ref no symbolic `HEAD` edit (so a failed lock returns an
/// error instead of reaching the gix-ref 0.66.0 loop in
/// `Transaction::prepare_inner`, store/file/transaction/prepare.rs:401) and
/// takes `HEAD.lock` itself with a fail-immediately acquire.
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
            return Err(lock_refusal(command, &shown, refname, "moving"));
        }
    }
    Ok(())
}

/// The exit-4 refusal for a lock file that exists. `shown` is relative to
/// the git directory. `action` is `moving` or `creating`.
fn lock_refusal(command: &str, shown: &str, refname: &str, action: &str) -> GitError {
    refuse::<()>(
        4,
        command,
        format!("The lock file '{shown}' already exists in the git directory ({action} '{refname}' needs it), so another git process may be running. This shell does not remove a lock it did not take."),
        "Wait and run the same command again. If no other git process is running, remove the lock with real git's environment, for example: rm .git/refs/heads/<branch>.lock.",
        Override::Repository,
    )
    .unwrap_err()
}

/// Map a failed lock acquire: a lock that exists is the exit-4 refusal naming
/// `shown`; anything else is a repository error.
fn lock_error(kind: MoveKind, command: &str, shown: &str, refname: &str, action: &str, repo: &WriteRepo, source: gix_lock::acquire::Error) -> GitError {
    if matches!(source, gix_lock::acquire::Error::PermanentlyLocked { .. }) {
        lock_refusal(command, shown, refname, action)
    } else {
        GitError::repository(kind.op, format!("locking '{shown}'"), repo.common_dir(), source)
    }
}

fn prepare_error(
    kind: MoveKind,
    command: &str,
    refname: &str,
    old: Option<ObjectId>,
    repo: &WriteRepo,
    e: gix_ref::file::transaction::prepare::Error,
) -> GitError {
    use gix_ref::file::transaction::prepare::Error as E;
    // A ref with no old value is being created.
    let action = if old.is_some() { "moving" } else { "creating" };
    match e {
        E::ReferenceOutOfDate { actual, .. } => cas_failure(kind, command, refname, old, Some(&actual)).unwrap_err(),
        E::MustNotExist { actual, .. } => cas_failure(kind, command, refname, None, Some(&actual)).unwrap_err(),
        E::MustExist { .. } => cas_failure(kind, command, refname, old, None).unwrap_err(),
        E::LockAcquire { source, .. } => lock_error(kind, command, &format!("{refname}.lock"), refname, action, repo, source),
        E::PackedTransactionAcquire(source) => lock_error(kind, command, "packed-refs.lock", refname, action, repo, source),
        other => GitError::repository(kind.op, format!("moving '{refname}'"), repo.common_dir(), other),
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
    kind: MoveKind,
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
            "'{refname}' moved while {}: it was {expected} when it started and is now {}.",
            kind.during,
            target_text(actual)
        ),
        format!("Run git status to see where the branch is now, then {}.", kind.again),
        Override::Never,
    )
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use crate::verbs::write::fixtures::{git_stdout, Fixture};
    use std::sync::mpsc;
    use std::time::Duration;

    fn committer() -> Signature {
        Signature { name: "T".into(), email: "t@example.com".into(), seconds: 1_800_000_000, offset: 0 }
    }

    /// Run `apply_move` on `main` in a thread, with a deadline: a transaction
    /// that spins (gix-ref 0.66.0, W13) must fail this test, not hang the suite.
    fn move_main_with_deadline(fx: &Fixture) -> Result<(), GitError> {
        let head = git_stdout(&fx.repo, &["rev-parse", "HEAD"]);
        let old = ObjectId::from_hex(head.as_bytes()).expect("oid");
        let new = git_stdout(&fx.repo, &["-c", "user.name=T", "-c", "user.email=t@example.com", "commit-tree", "HEAD^{tree}", "-p", "HEAD", "-m", "next"]);
        let new = ObjectId::from_hex(new.as_bytes()).expect("oid");
        let at = fx.repo.clone();
        let mount = fx.mount.clone();
        let (tx, rx) = mpsc::channel();
        std::thread::spawn(move || {
            let ceiling = mount.canonicalize().expect("mount");
            let read = crate::repo::ReadRepo::discover("commit", &at, &ceiling).expect("discover");
            let repo = WriteRepo::open(read, crate::verbs::write::repo::WriteToken::test()).expect("open");
            let target = HeadTarget::Branch("refs/heads/main".to_string());
            let _ = tx.send(apply_move(&repo, MoveKind::COMMIT, "git commit", &target, Some(old), new, "commit: t", &committer()));
        });
        rx.recv_timeout(Duration::from_secs(10))
            .expect("the transaction did not return within 10s: gix-ref spins on a branch lock under a symbolic HEAD (W13)")
    }

    fn assert_lock_refusal(r: Result<(), GitError>, lock: &str) {
        let err = r.expect_err("a held lock must refuse");
        assert_eq!(err.exit_code(), 4, "{err}");
        assert!(err.to_string().contains("lock"), "{err}");
        assert!(err.to_string().contains(lock), "the refusal names {lock}: {err}");
    }

    #[test]
    fn a_branch_lock_taken_after_the_precheck_refuses_with_exit_4_and_does_not_hang() {
        let fx = Fixture::new();
        std::fs::write(fx.common().join("refs/heads/main.lock"), "").expect("lock");
        assert_lock_refusal(move_main_with_deadline(&fx), "refs/heads/main.lock");
        assert!(fx.common().join("refs/heads/main.lock").exists(), "a lock this shell did not take is left alone");
    }

    #[test]
    fn a_head_lock_taken_after_the_precheck_refuses_with_exit_4_and_does_not_hang() {
        let fx = Fixture::new();
        std::fs::write(fx.common().join("HEAD.lock"), "").expect("lock");
        assert_lock_refusal(move_main_with_deadline(&fx), "HEAD.lock");
        assert!(fx.common().join("HEAD.lock").exists());
    }

    #[test]
    fn a_move_leaves_no_lock_and_head_still_names_the_branch() {
        let fx = Fixture::new();
        move_main_with_deadline(&fx).expect("moves");
        assert!(!fx.common().join("HEAD.lock").exists());
        assert!(!fx.common().join("refs/heads/main.lock").exists());
        let head = std::fs::read_to_string(fx.common().join("HEAD")).expect("HEAD");
        assert_eq!(head, "ref: refs/heads/main\n");
        let log = std::fs::read_to_string(fx.common().join("logs/HEAD")).expect("log");
        assert!(log.lines().last().expect("line").ends_with("\tcommit: t"), "{log}");
        let blog = std::fs::read_to_string(fx.common().join("logs/refs/heads/main")).expect("log");
        assert!(blog.lines().last().expect("line").ends_with("\tcommit: t"), "{blog}");
    }

    #[test]
    fn a_failed_head_reflog_append_says_the_commit_was_made() {
        use std::os::unix::fs::PermissionsExt;
        let fx = Fixture::new();
        let log = fx.common().join("logs/HEAD");
        let before = std::fs::read(&log).expect("log");
        std::fs::set_permissions(&log, std::fs::Permissions::from_mode(0o444)).expect("chmod");
        if std::fs::OpenOptions::new().append(true).open(&log).is_ok() {
            // Running as root: permissions do not stop the append.
            return;
        }
        let err = move_main_with_deadline(&fx).expect_err("logs/HEAD is read-only");
        let text = err.to_string();
        assert!(text.contains("the commit was made"), "{text}");
        assert!(text.contains("do not run the command again"), "{text}");
        assert!(!text.to_lowercase().contains("nothing was"), "the branch moved: {text}");
        // The branch did move, and the lock is released.
        let main = std::fs::read_to_string(fx.common().join("refs/heads/main")).expect("main");
        assert!(text.contains(main.trim()), "names the new oid: {text}");
        assert!(!fx.common().join("HEAD.lock").exists());
        assert_eq!(std::fs::read(&log).expect("log"), before);
    }

    #[test]
    fn a_head_that_names_another_branch_refuses_and_releases_its_lock() {
        let fx = Fixture::new();
        let repo = fx.write_repo(false);
        let err = hold_head(&repo, MoveKind::COMMIT, "git commit", "refs/heads/side").map(|_| ()).expect_err("HEAD names main, not side");
        assert_eq!(err.exit_code(), 1, "{err}");
        assert!(err.to_string().contains("refs/heads/main"), "{err}");
        assert!(!fx.common().join("HEAD.lock").exists(), "the lock is released on the refusal path");
        assert_eq!(std::fs::read_to_string(fx.common().join("HEAD")).expect("HEAD"), "ref: refs/heads/main\n");
    }

    /// A reset that loses the race says "reset", not "commit": the wording
    /// names the operation that ran, and leaves out the commit object a
    /// reset never writes.
    #[test]
    fn a_lost_race_in_a_reset_is_worded_for_a_reset() {
        let fx = Fixture::new();
        let stale = ObjectId::from_hex(b"1111111111111111111111111111111111111111").expect("oid");
        let new = ObjectId::from_hex(git_stdout(&fx.repo, &["rev-parse", "HEAD"]).as_bytes()).expect("oid");
        let repo = fx.write_repo(false);
        let target = HeadTarget::Branch("refs/heads/main".to_string());
        let err = apply_move(&repo, MoveKind::RESET, "git reset --soft x", &target, Some(stale), new, "reset: moving to x", &committer())
            .expect_err("the branch is not at the stale oid");
        let text = err.to_string();
        assert_eq!(err.exit_code(), 1, "{text}");
        assert!(text.contains("moved while this reset was running"), "{text}");
        assert!(text.contains("run the reset again"), "{text}");
        assert!(!text.to_lowercase().contains("commit object"), "{text}");
        assert!(!text.contains("make the commit again"), "{text}");

        let err = hold_head(&repo, MoveKind::RESET, "git reset --soft x", "refs/heads/side").map(|_| ()).expect_err("HEAD names main");
        let text = err.to_string();
        assert!(text.contains("HEAD moved while this reset was running"), "{text}");
        assert!(text.contains("run the reset again"), "{text}");
        assert!(!text.contains("commit object"), "{text}");
    }

    fn head_oid(fx: &Fixture) -> ObjectId {
        ObjectId::from_hex(git_stdout(&fx.repo, &["rev-parse", "HEAD"]).as_bytes()).expect("oid")
    }

    /// `create_ref` ends in a compare-and-swap on "does not exist", so a ref
    /// that appeared after the caller looked fails the transaction and names
    /// both values.
    #[test]
    fn a_ref_that_appeared_before_the_create_fails_the_swap_and_is_left_as_it_is() {
        let fx = Fixture::new();
        git_stdout(&fx.repo, &["branch", "taken", "HEAD"]);
        let repo = fx.write_repo(false);
        let other = ObjectId::from_hex(git_stdout(&fx.repo, &["-c", "user.name=T", "-c", "user.email=t@example.com", "commit-tree", "HEAD^{tree}", "-m", "other"]).as_bytes()).expect("oid");
        let before = std::fs::read_to_string(fx.common().join("refs/heads/taken")).expect("ref");
        let err = create_ref(&repo, MoveKind::BRANCH, "git branch taken", "refs/heads/taken", other, Some(("branch: Created from HEAD", &committer())))
            .expect_err("the ref exists at another oid");
        let text = err.to_string();
        assert_eq!(err.exit_code(), 1, "{text}");
        assert!(text.contains("moved while this branch was being created"), "{text}");
        assert!(text.contains("is now ") && text.contains(&head_oid(&fx).to_string()), "names the oid it found: {text}");
        assert!(text.contains("create the branch again"), "{text}");
        assert_eq!(std::fs::read_to_string(fx.common().join("refs/heads/taken")).expect("ref"), before);
        assert!(!fx.common().join("refs/heads/taken.lock").exists());
    }

    #[test]
    fn a_ref_lock_refuses_a_create_with_exit_4_and_is_left_alone() {
        let fx = Fixture::new();
        let repo = fx.write_repo(false);
        std::fs::write(fx.common().join("refs/heads/new.lock"), "").expect("lock");
        let err = create_ref(&repo, MoveKind::BRANCH, "git branch new", "refs/heads/new", head_oid(&fx), None).expect_err("locked");
        assert_eq!(err.exit_code(), 4, "{err}");
        assert!(err.to_string().contains("refs/heads/new.lock"), "{err}");
        assert!(fx.common().join("refs/heads/new.lock").exists());
        assert!(!fx.common().join("refs/heads/new").exists());
    }

    fn switch(repo: &WriteRepo, full: &str, was: &HeadTarget, oid: ObjectId) -> Result<(), GitError> {
        switch_head(
            repo,
            &HeadSwitch {
                kind: MoveKind::SWITCH,
                command: "git switch -c new",
                was,
                head_oid: Some(oid),
                full,
                create: Some(("branch: Created from HEAD", &committer())),
                created: None,
            },
        )
    }

    #[test]
    fn a_held_head_lock_stops_a_switch_before_the_branch_exists() {
        let fx = Fixture::new();
        let repo = fx.write_repo(false);
        std::fs::write(fx.common().join("HEAD.lock"), "").expect("lock");
        let err = switch(&repo, "refs/heads/new", &HeadTarget::Branch("refs/heads/main".into()), head_oid(&fx)).expect_err("locked");
        assert_eq!(err.exit_code(), 4, "{err}");
        assert!(err.to_string().contains("HEAD.lock"), "{err}");
        assert!(!fx.common().join("refs/heads/new").exists(), "no branch without HEAD");
        assert_eq!(std::fs::read_to_string(fx.common().join("HEAD")).expect("HEAD"), "ref: refs/heads/main\n");
    }

    #[test]
    fn a_head_that_changed_since_it_was_read_refuses_a_switch_and_makes_no_branch() {
        let fx = Fixture::new();
        let repo = fx.write_repo(false);
        // The caller read "main"; HEAD names "side" by now.
        let err = switch(&repo, "refs/heads/new", &HeadTarget::Branch("refs/heads/side".into()), head_oid(&fx)).expect_err("HEAD moved");
        assert_eq!(err.exit_code(), 1, "{err}");
        assert!(err.to_string().contains("HEAD moved while this switch was running"), "{err}");
        assert!(!fx.common().join("refs/heads/new").exists());
        assert!(!fx.common().join("HEAD.lock").exists(), "the lock is released");
        assert_eq!(std::fs::read_to_string(fx.common().join("HEAD")).expect("HEAD"), "ref: refs/heads/main\n");
    }

    #[test]
    fn a_switch_moves_head_logs_it_and_releases_the_lock() {
        let fx = Fixture::new();
        let repo = fx.write_repo(false);
        switch(&repo, "refs/heads/new", &HeadTarget::Branch("refs/heads/main".into()), head_oid(&fx)).expect("switches");
        assert_eq!(std::fs::read_to_string(fx.common().join("HEAD")).expect("HEAD"), "ref: refs/heads/new\n");
        assert!(!fx.common().join("HEAD.lock").exists());
        let log = std::fs::read_to_string(fx.common().join("logs/HEAD")).expect("log");
        assert!(log.lines().last().expect("line").ends_with("\tcheckout: moving from main to new"), "{log}");
        let blog = std::fs::read_to_string(fx.common().join("logs/refs/heads/new")).expect("log");
        assert!(blog.ends_with("\tbranch: Created from HEAD\n"), "{blog}");
    }

    /// The branch and HEAD both moved before the log append failed, so the
    /// error must say the command ran and must not be run again.
    #[test]
    fn a_failed_head_log_append_in_a_switch_says_the_branch_was_made_and_head_moved() {
        use std::os::unix::fs::PermissionsExt;
        let fx = Fixture::new();
        let repo = fx.write_repo(false);
        let log = fx.common().join("logs/HEAD");
        std::fs::set_permissions(&log, std::fs::Permissions::from_mode(0o444)).expect("chmod");
        if std::fs::OpenOptions::new().append(true).open(&log).is_ok() {
            return; // running as root: permissions do not stop the append
        }
        let err = switch(&repo, "refs/heads/new", &HeadTarget::Branch("refs/heads/main".into()), head_oid(&fx)).expect_err("logs/HEAD is read-only");
        let text = err.to_string();
        assert!(text.contains("the branch was created") && text.contains("HEAD now names 'refs/heads/new'"), "{text}");
        assert!(text.contains("do not run the command again"), "{text}");
        assert!(!text.to_lowercase().contains("nothing was"), "{text}");
        assert_eq!(std::fs::read_to_string(fx.common().join("HEAD")).expect("HEAD"), "ref: refs/heads/new\n");
        assert!(!fx.common().join("HEAD.lock").exists());
    }
}
