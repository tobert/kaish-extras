//! Commit ancestry: the three questions `git branch` and `git tag` ask that
//! cost commits rather than rows (architecture.md B.7).
//!
//! - `--contains <REV>` — is `<REV>` an ancestor of this tip?
//! - `--merged <REV>` — is this tip an ancestor of `<REV>`?
//! - `--ahead-behind` — how far has this branch diverged from its upstream?
//!
//! **Everything here is metered.** A [`Budget`] counts every commit object
//! read and refuses once the count passes [`MAX_ANCESTRY_COMMITS`], because
//! `--limit` bounds *rows* and none of these questions is answered per row:
//! a filter has to be evaluated for every candidate before truncation, or the
//! truncation would cut rows the filter never judged. The count is reported
//! back to the caller (`commits_examined`) rather than kept as a private
//! implementation detail — an embedder budgeting a long-lived server should
//! be able to see the cost it is paying.
//!
//! **A partial answer is a wrong answer**, so exhausting the budget is a
//! refusal, not a truncation. A `--merged` listing that quietly stopped
//! judging branches would report a branch as unmerged because we gave up
//! looking, which is the confidently-wrong shape this crate is organized
//! against.

use std::collections::{HashMap, HashSet};

use gix_index::hash::ObjectId;
use gix_object::FindExt;

use crate::error::GitError;
use crate::repo::ReadRepo;

/// How many commit objects one invocation's ancestry questions may read.
///
/// The same order as `verbs/log.rs`'s `MAX_COMMITS_EXAMINED`, and for the same
/// reason — a walk with no natural stopping point needs one — but a separate
/// constant because it bounds a different thing: `log`'s budget bounds a
/// search for matching commits and reports `truncated: true` when it runs out,
/// while this one bounds an ancestry decision that has no partial form.
pub(crate) const MAX_ANCESTRY_COMMITS: u64 = 100_000;

/// A meter over commit object reads, shared by every walk in one invocation.
pub(crate) struct Budget {
    operation: &'static str,
    examined: u64,
}

impl Budget {
    /// A fresh budget for `operation`.
    pub(crate) fn new(operation: &'static str) -> Self {
        Budget {
            operation,
            examined: 0,
        }
    }

    /// How many commits have been read so far.
    pub(crate) fn examined(&self) -> u64 {
        self.examined
    }

    /// A commit's parent oids, charged against the budget.
    fn parents(&mut self, repo: &ReadRepo, oid: ObjectId) -> Result<Vec<ObjectId>, GitError> {
        self.charge()?;
        let mut buf = Vec::new();
        let commit = repo.objects().find_commit(&oid, &mut buf).map_err(|e| {
            GitError::repository(self.operation, "reading a commit", repo.git_dir(), e)
        })?;
        Ok(commit.parents().collect())
    }

    fn charge(&mut self) -> Result<(), GitError> {
        self.examined += 1;
        if self.examined > MAX_ANCESTRY_COMMITS {
            return Err(GitError::AncestryBudgetExhausted {
                operation: self.operation,
                limit: MAX_ANCESTRY_COMMITS,
            });
        }
        Ok(())
    }
}

/// "Does this commit reach a fixed target?", memoized across every tip asked.
///
/// `--contains <REV>` asks it once per candidate ref, and the answer for a
/// commit is a property of the commit alone, so one table serves them all:
/// the total cost is the union of the histories walked, not the sum of them.
pub(crate) struct Reaches {
    target: ObjectId,
    memo: HashMap<ObjectId, bool>,
}

/// One frame of the explicit-stack walk.
///
/// An explicit stack rather than recursion, matching every other bounded walk
/// in this crate except `status`'s (docs/issues.md G10): history is as deep as
/// the repository chooses, and the call stack is not.
enum Step {
    /// Visit this commit: answer from the memo, or expand its parents.
    Enter(ObjectId),
    /// Both parents have answers now; combine them into this commit's.
    Exit(ObjectId, Vec<ObjectId>),
}

impl Reaches {
    /// A table answering "does X reach `target`?".
    pub(crate) fn to(target: ObjectId) -> Self {
        Reaches {
            target,
            memo: HashMap::new(),
        }
    }

    /// Whether `start` has the target as an ancestor (or *is* it, which is
    /// what git's `--contains` means for the tip itself).
    pub(crate) fn from(
        &mut self,
        repo: &ReadRepo,
        budget: &mut Budget,
        start: ObjectId,
    ) -> Result<bool, GitError> {
        if let Some(known) = self.memo.get(&start) {
            return Ok(*known);
        }
        let mut stack = vec![Step::Enter(start)];
        while let Some(step) = stack.pop() {
            match step {
                Step::Enter(oid) => {
                    if self.memo.contains_key(&oid) {
                        continue;
                    }
                    if oid == self.target {
                        self.memo.insert(oid, true);
                        continue;
                    }
                    let parents = budget.parents(repo, oid)?;
                    stack.push(Step::Exit(oid, parents.clone()));
                    for parent in parents {
                        stack.push(Step::Enter(parent));
                    }
                }
                Step::Exit(oid, parents) => {
                    // A parent with no answer would mean a cycle in the commit
                    // graph, which is not a shape git can produce. Reading it
                    // as `false` keeps a corrupt repository from hanging here
                    // rather than inventing a `true`.
                    let reached = parents
                        .iter()
                        .any(|p| self.memo.get(p).copied().unwrap_or(false));
                    self.memo.insert(oid, reached);
                }
            }
        }
        Ok(self.memo.get(&start).copied().unwrap_or(false))
    }
}

/// Every commit reachable from `tip`, itself included.
///
/// `--merged <REV>` is a membership test against this set, so it is built once
/// per invocation however many branches are listed. It is the whole history
/// behind `<REV>`, which is the honest cost of the question: "is this branch
/// already in that one" cannot be answered from the tips alone.
pub(crate) fn ancestors(
    repo: &ReadRepo,
    budget: &mut Budget,
    tip: ObjectId,
) -> Result<HashSet<ObjectId>, GitError> {
    let mut seen = HashSet::new();
    let mut stack = vec![tip];
    seen.insert(tip);
    while let Some(oid) = stack.pop() {
        for parent in budget.parents(repo, oid)? {
            if seen.insert(parent) {
                stack.push(parent);
            }
        }
    }
    Ok(seen)
}

/// How many commits each side has that the other does not — git's
/// `rev-list --left-right --count <local>...<upstream>`.
///
/// Paints both histories with which side reaches each commit, then counts the
/// commits only one side reached. **Nothing here depends on the order commits
/// come out in**, and that is deliberate: an earlier draft popped commits
/// newest-first and stopped as soon as everything queued was common history,
/// which is only sound if committer time increases from parent to child. It
/// does not. A repository whose commits share one instant — a scripted import,
/// a fixture, a fast rebase — made that walk stop one commit early and report
/// `behind 2` where git reports `behind 1`, and a clock that ran backwards did
/// the same. Both are pinned in `tests/branch.rs`.
///
/// **What it costs.** Both histories, to their roots — this reads the same
/// commits `git rev-list <local> <upstream>` would. The early stop is what a
/// cheaper version would buy, and it is not available without an assumption
/// about clocks that this crate is not willing to make. `--limit` bounds how
/// many branches pay it, [`MAX_ANCESTRY_COMMITS`] refuses when one invocation's
/// total passes 100,000 commits, and `commits_examined` reports what it spent.
/// Recorded as docs/issues.md B1.
pub(crate) fn ahead_behind(
    repo: &ReadRepo,
    budget: &mut Budget,
    local: ObjectId,
    upstream: ObjectId,
) -> Result<(u64, u64), GitError> {
    /// Reachable from the branch being measured.
    const LEFT: u8 = 1;
    /// Reachable from its upstream.
    const RIGHT: u8 = 2;
    if local == upstream {
        return Ok((0, 0));
    }

    let mut flags: HashMap<ObjectId, u8> = HashMap::new();
    // A commit is queued when it is first marked and again if its flags
    // widen, so it is expanded at most twice — once per side. The worklist is
    // a plain stack because the order genuinely does not matter.
    let mut work: Vec<(ObjectId, u8)> = vec![(local, LEFT), (upstream, RIGHT)];

    while let Some((oid, side)) = work.pop() {
        let entry = flags.entry(oid).or_insert(0);
        let old = *entry;
        let new = old | side;
        if new == old && old != 0 {
            continue;
        }
        *entry = new;
        for parent in budget.parents(repo, oid)? {
            work.push((parent, new));
        }
    }

    let mut ahead = 0;
    let mut behind = 0;
    for side in flags.values() {
        match *side {
            LEFT => ahead += 1,
            RIGHT => behind += 1,
            // `LEFT | RIGHT` — common history, counted for neither side.
            _ => {}
        }
    }
    Ok((ahead, behind))
}

/// Refuse an ancestry question on a repository whose history is truncated.
///
/// A shallow clone's boundary commits have parents the object store does not
/// hold, so a walk stops at a wall rather than at a root. Answering "this ref
/// does not contain that commit" out of a history that was cut off is the
/// confidently-wrong shape E.5 forbids, and the failure would otherwise wear
/// the shape of a missing object rather than of a shallow repository.
pub(crate) fn refuse_shallow(
    repo: &ReadRepo,
    operation: &'static str,
    flag: &'static str,
) -> Result<(), GitError> {
    if repo.is_shallow()? {
        return Err(GitError::Usage {
            operation,
            message: format!(
                "{flag} needs full history, and this repository is shallow — \
                 the commits it would search are not here. Ask without {flag} \
                 for the listing itself, which reads no history"
            ),
        });
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A budget that has not been spent refuses nothing, and one that has is
    /// a refusal rather than a smaller answer.
    #[test]
    fn the_budget_refuses_rather_than_truncating() {
        let mut budget = Budget::new("branch");
        assert_eq!(budget.examined(), 0);
        for _ in 0..MAX_ANCESTRY_COMMITS {
            budget.charge().expect("within the budget");
        }
        let err = budget.charge().expect_err("one past the budget");
        assert_eq!(
            err.exit_code(),
            1,
            "a repository too large for the question is a git-level no"
        );
        assert!(
            err.to_string().contains(&MAX_ANCESTRY_COMMITS.to_string()),
            "the refusal names the limit: {err}"
        );
    }
}
