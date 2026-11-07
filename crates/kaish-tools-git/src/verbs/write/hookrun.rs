//! Calling an embedder's [`GitHooks`] around a ref update.
//!
//! A verb that moves a ref builds one [`Tx`] per ref transaction git would
//! make, calls [`prepared`] for all of them after every refusal it can raise
//! and before the first write (it sends `Preparing` for each, then `Prepared`
//! for each), then [`committed`] for each as it is written, or [`aborted`] for
//! each that was not. `Err` at `Preparing` or `Prepared` refuses.

use std::path::{Path, PathBuf};

use gix_index::hash::ObjectId;

use crate::error::GitError;
use crate::hooks::{GitHooks, HookContext, RefTransactionState, RefUpdate};
use crate::verbs::write::commit::hook_present;
use crate::verbs::write::refmove::MoveKind;
use crate::verbs::write::refusal::{refuse, Override};
use crate::verbs::write::repo::WriteRepo;
use crate::verbs::write::state::hooks_dir;

/// One ref transaction, as git shows it at each state.
pub(crate) struct Tx {
    /// The lines at `Preparing`. Git names a move of the current branch `HEAD`
    /// there.
    pub(crate) preparing: Vec<RefUpdate>,
    /// The lines at `Prepared`, `Committed` and `Aborted` after `Prepared`.
    pub(crate) updates: Vec<RefUpdate>,
}

impl Tx {
    /// A transaction that has the same lines at every state.
    pub(crate) fn new(old: impl Into<String>, new: impl Into<String>, name: impl Into<String>) -> Self {
        let u = RefUpdate::new(old, new, name);
        Self { preparing: vec![u.clone()], updates: vec![u] }
    }

    /// A move of HEAD's branch: `HEAD` at `Preparing`, the branch after.
    /// A detached HEAD is named `HEAD` throughout.
    pub(crate) fn via_head(old: impl Into<String>, new: impl Into<String>, name: impl Into<String>) -> Self {
        let (old, new, name) = (old.into(), new.into(), name.into());
        Self { preparing: vec![RefUpdate::new(old.clone(), new.clone(), "HEAD")], updates: vec![RefUpdate::new(old, new, name)] }
    }
}

/// The hooks directory and the names present in it among `names`.
pub(crate) struct Scan {
    pub(crate) dir: PathBuf,
    pub(crate) present: Vec<String>,
}

impl Scan {
    pub(crate) fn read(repo: &WriteRepo, command: &str, names: &[&str]) -> Result<Self, GitError> {
        let dir = hooks_dir(repo, command)?;
        let present = names.iter().filter(|n| hook_present(&dir, n)).map(|n| (*n).to_string()).collect();
        Ok(Self { dir, present })
    }

    /// The context for a verb that makes no commit.
    pub(crate) fn context<'a>(&'a self, repo: &'a WriteRepo) -> HookContext<'a> {
        context(repo, &self.dir, &self.present, None, false)
    }
}

pub(crate) fn context<'a>(
    repo: &'a WriteRepo,
    dir: &'a Path,
    present: &'a [String],
    tree: Option<&'a str>,
    no_verify: bool,
) -> HookContext<'a> {
    HookContext {
        worktree: repo.read().work_dir(),
        git_dir: repo.git_dir(),
        hooks_dir: dir,
        tree,
        hooks_present: present,
        no_verify,
    }
}

/// The null oid, as 40 zeros, in the form a transaction line carries.
pub(crate) fn null() -> String {
    ObjectId::null(gix_index::hash::Kind::Sha1).to_string()
}

/// `old` as a transaction line shows it: the oid, or zeros when there is none.
pub(crate) fn oid_or_null(old: Option<ObjectId>) -> String {
    old.map_or_else(null, |o| o.to_string())
}

/// Send `Preparing` for each transaction in order, then `Prepared` for each.
/// When a call is refused, send `Aborted` for every transaction that was sent
/// `Preparing` (with the lines of the last state each was sent), and return the
/// refusal.
///
/// The refusal carries no written-state: a caller that has written objects
/// marks it with [`GitError::after_objects`].
pub(crate) fn prepared(
    hooks: &dyn GitHooks,
    ctx: &HookContext<'_>,
    kind: MoveKind,
    command: &str,
    txs: &[Tx],
    err: &mut String,
) -> Result<(), GitError> {
    let mut refused: Option<(usize, RefTransactionState, String)> = None;
    'states: for state in [RefTransactionState::Preparing, RefTransactionState::Prepared] {
        for (i, tx) in txs.iter().enumerate() {
            let lines = if state == RefTransactionState::Preparing { &tx.preparing } else { &tx.updates };
            match hooks.reference_transaction(ctx, state, lines) {
                Ok(out) => note(err, &out.message),
                Err(e) => {
                    refused = Some((i, state, e.message));
                    break 'states;
                }
            }
        }
    }
    let Some((at, state, why)) = refused else { return Ok(()) };
    // A refusal has no stderr to add to; the aborted calls' text is dropped
    // with it, and their `Err` can change nothing.
    let mut dropped = String::new();
    for (i, tx) in txs.iter().enumerate() {
        let was_prepared = state == RefTransactionState::Prepared && i <= at;
        let lines = if was_prepared { &tx.updates } else { &tx.preparing };
        // At `Preparing`, the transactions after the refused one were not sent.
        if state == RefTransactionState::Preparing && i > at {
            continue;
        }
        send_lines(hooks, ctx, RefTransactionState::Aborted, lines, &mut dropped);
    }
    refuse(
        1,
        command,
        format!("The reference-transaction hook refused the {}: {}", kind.noun, why),
        format!("Fix what it names, then {}.", kind.again),
        Override::Embedder,
    )
}

/// Send `Committed` for `tx`. A failure is shown on stderr and is not fatal.
pub(crate) fn committed(hooks: &dyn GitHooks, ctx: &HookContext<'_>, tx: &Tx, err: &mut String) {
    send(hooks, ctx, RefTransactionState::Committed, tx, err);
}

/// Send `Aborted` for `tx`. A failure is shown on stderr and is not fatal.
pub(crate) fn aborted(hooks: &dyn GitHooks, ctx: &HookContext<'_>, tx: &Tx, err: &mut String) {
    send(hooks, ctx, RefTransactionState::Aborted, tx, err);
}

fn send(hooks: &dyn GitHooks, ctx: &HookContext<'_>, state: RefTransactionState, tx: &Tx, err: &mut String) {
    send_lines(hooks, ctx, state, &tx.updates, err);
}

fn send_lines(hooks: &dyn GitHooks, ctx: &HookContext<'_>, state: RefTransactionState, lines: &[RefUpdate], err: &mut String) {
    match hooks.reference_transaction(ctx, state, lines) {
        Ok(out) => note(err, &out.message),
        Err(e) => note(err, &format!("The reference-transaction hook failed at {}: {}", state.as_str(), e.message)),
    }
}

pub(crate) fn note(err: &mut String, text: &str) {
    if text.is_empty() {
        return;
    }
    err.push_str(text);
    if !text.ends_with('\n') {
        err.push('\n');
    }
}
