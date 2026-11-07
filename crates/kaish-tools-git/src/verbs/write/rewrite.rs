//! The rewrite check: the one function every branch move goes through
//! (git-write-profile.md §3).
//!
//! A branch move from the old tip O to a new tip N *drops* the commits
//! reachable from O and not from N. The move is a **rewrite** when that set is
//! not empty, that is, when O is not an ancestor of (or equal to) N. A plain
//! commit and a forward `reset` drop nothing and always pass.
//!
//! - `Allow` passes everything.
//! - `Refuse` passes only a move that drops nothing.
//! - `Unpublished` passes a move when **no dropped commit is reachable from a
//!   remote-tracking ref** (`refs/remotes/*`). The design's first wording asked
//!   only whether O itself is published, which is enough for `--amend` (it
//!   drops exactly O) and wrong for `reset --soft HEAD~3`: with `origin/main`
//!   at `HEAD~1`, that reset drops `HEAD~1` and `HEAD~2`, which are published,
//!   while O is not.
//!
//! # The walk, and why its stop rule is sound
//!
//! "Is any dropped commit published" is a painting walk with three sides: from
//! O, from N and from every remote tip, each commit carrying the sides that
//! reach it. A dropped, published commit is one painted `O` and `R` and not
//! `N` once the painting is complete. This is not [`crate::reach::Reaches`],
//! which is single-sided and would read the whole history behind every remote
//! tip to answer "no".
//!
//! **No stop rule here reads a commit date.** Committer dates are not ordered
//! along parent edges (clock skew, imports, rebases), and git's own date-ordered
//! walk is unsound under skew. The two stop rules below depend only on the
//! graph:
//!
//! * *Exhaustion.* The walk ends when nothing is pending. Every commit that
//!   any side reaches has then been painted by all the sides that reach it, so
//!   the verdict is exact.
//! * *S1, all pending carry `N`.* Everything not yet visited is an ancestor of
//!   a pending commit, so it will be painted `N` and cannot be dropped. Used
//!   only when no visited commit is already `O` and `R` without `N`: such a
//!   commit may still be reached by a pending `N` later, so the walk drains
//!   instead.
//! * *S2, generation numbers.* When `objects/info/commit-graph` exists, its
//!   generation number (a topological level: a commit is strictly above all its
//!   parents) orders the walk, highest first. A commit popped in that order has
//!   already received everything its descendants in the walk can give it, so
//!   its painting is final at pop. The walk stops when no pending commit
//!   carries `O` without `N` and the highest pending generation is below the
//!   lowest generation of any visited `O`-without-`N` commit: a pending commit
//!   can only paint its ancestors, which have lower generations, so none can
//!   reach a commit at or above that line.
//!
//! Commits the graph lacks have no generation and sort above every commit
//! that has one, which is sound because a commit in the graph never has a
//! parent outside it. The ordering assumption is **checked as the walk runs**:
//! an expanded commit whose parent has an equal or higher generation (or none),
//! or a commit that gains a side after it was expanded, means the file lies or
//! is inconsistent, and the walk restarts without it. Without a graph the
//! walk simply runs to exhaustion, bounded by the [`Budget`]; the cost is the
//! commits behind the remote tips that the dropped commits do not reach, and a
//! repository too large for that refuses (exit 1) and names `git
//! commit-graph write`.
//!
//! What is not covered: a commit-graph that is consistently wrong (every
//! generation shifted so that order is preserved but a parent edge is
//! missing) is trusted; the file is part of the repository, and a repository
//! that forges its own object store has other ways to mislead (docs/issues.md
//! W24).

use std::collections::{BinaryHeap, HashMap, HashSet};

use gix_index::hash::ObjectId;

use crate::config::RewritePolicy;
use crate::error::GitError;
use crate::reach::Budget;
use crate::verbs::write::refusal::{refuse, Override};
use crate::verbs::write::repo::WriteRepo;

/// What is moving the branch, for the refusal's words.
#[derive(Clone, Copy)]
pub(crate) enum Mover<'a> {
    Amend,
    Reset { rev: &'a str },
}

/// Where the branch is going.
pub(crate) enum NewTip {
    /// A new commit whose parent is the old tip. Drops nothing.
    Child,
    /// A new commit that replaces the old tip: it has these parents (the old
    /// tip's own, for `--amend`), so the old tip is dropped by construction.
    Replacement(Vec<ObjectId>),
    /// An existing commit.
    Existing(ObjectId),
}

pub(crate) struct MoveCheck<'a> {
    pub(crate) command: &'a str,
    pub(crate) mover: Mover<'a>,
    pub(crate) policy: RewritePolicy,
}

const OP: &str = "commit";

/// Refuse when the policy forbids this move. Writes nothing.
pub(crate) fn check_move(
    repo: &WriteRepo,
    chk: &MoveCheck<'_>,
    old: Option<ObjectId>,
    new: &NewTip,
) -> Result<(), GitError> {
    check_move_with(repo, chk, old, new, Budget::new(OP))
}

fn check_move_with(
    repo: &WriteRepo,
    chk: &MoveCheck<'_>,
    old: Option<ObjectId>,
    new: &NewTip,
    mut budget: Budget,
) -> Result<(), GitError> {
    // Nothing to drop on an unborn branch.
    let Some(old) = old else { return Ok(()) };
    if chk.policy == RewritePolicy::Allow {
        return Ok(());
    }
    let seeds: Vec<ObjectId> = match new {
        NewTip::Child => return Ok(()),
        NewTip::Existing(n) if *n == old => return Ok(()),
        NewTip::Existing(n) => vec![*n],
        NewTip::Replacement(parents) => parents.clone(),
    };
    let replaces = matches!(new, NewTip::Replacement(_));

    if chk.policy == RewritePolicy::Refuse {
        if !replaces {
            // A shallow repository cannot show that old is an ancestor.
            refuse_shallow(repo, chk)?;
            let graph = Graph::load(repo);
            if budget_ok(reaches(repo, graph.as_ref(), &mut budget, &seeds, old), chk)? {
                return Ok(());
            }
        }
        return Err(policy_refusal(chk, old, None));
    }

    // Unpublished.
    refuse_shallow(repo, chk)?;
    let tips = remote_tips(repo)?;
    if tips.is_empty() {
        return Ok(());
    }
    let graph = Graph::load(repo);
    let verdict = match paint(repo, graph.as_ref(), &mut budget, old, &seeds, &tips) {
        Err(Trouble::Graph) => {
            // The graph disagrees with itself. Start over without it, on a
            // fresh meter: the first attempt's reads were wasted, not owed.
            let mut fresh = Budget::new(OP);
            paint(repo, None, &mut fresh, old, &seeds, &tips)
        }
        other => other,
    };
    let verdict = match verdict {
        Ok(v) => v,
        Err(Trouble::Git(e)) => return Err(budget_refusal(e, chk)),
        Err(Trouble::Graph) => unreachable!("a walk without a graph has no graph to distrust"),
    };
    match verdict {
        Verdict::Unpublished => Ok(()),
        Verdict::Published(commit) => {
            let name = name_remote(repo, graph.as_ref(), &tips, commit);
            Err(policy_refusal(chk, commit, Some(&name)))
        }
    }
}

fn short(oid: ObjectId) -> String {
    oid.to_string()[..7].to_string()
}

/// A walk that answers "is old an ancestor of the seeds": `true`, `false`, or
/// the budget's error as a refusal.
fn budget_ok(r: Result<bool, GitError>, chk: &MoveCheck<'_>) -> Result<bool, GitError> {
    r.map_err(|e| budget_refusal(e, chk))
}

/// The refusal for a policy, naming what the move drops.
fn policy_refusal(chk: &MoveCheck<'_>, commit: ObjectId, published_on: Option<&str>) -> GitError {
    let c = short(commit);
    let reason = match (published_on, chk.mover) {
        (None, Mover::Amend) => format!(
            "This shell never replaces a commit, and --amend would replace HEAD {c}."
        ),
        (None, Mover::Reset { .. }) => {
            "This shell never replaces a commit, and this reset would drop commits from the branch.".to_string()
        }
        (Some(r), Mover::Amend) => format!(
            "HEAD {c} is on {r}, and this shell does not rewrite published commits. \
             Remote-tracking refs show the last fetch, not the remote itself."
        ),
        (Some(r), Mover::Reset { .. }) => format!(
            "This reset would drop commit {c}, which is on {r}, and this shell does not rewrite published commits. \
             Remote-tracking refs show the last fetch, not the remote itself."
        ),
    };
    refuse::<()>(
        2,
        chk.command,
        reason,
        "Make a new commit instead: git commit -m '<message>'.",
        Override::Embedder,
    )
    .unwrap_err()
}

fn refuse_shallow(repo: &WriteRepo, chk: &MoveCheck<'_>) -> Result<(), GitError> {
    if repo.read().is_shallow()? {
        return refuse(
            4,
            chk.command,
            "This repository is shallow, so its history is incomplete and this shell cannot tell what this move drops or whether that is published.",
            "Make a new commit instead: git commit -m '<message>'.",
            Override::Repository,
        );
    }
    Ok(())
}

/// A walk that ran out of budget is a refusal; any other error stays one.
fn budget_refusal(e: GitError, chk: &MoveCheck<'_>) -> GitError {
    match e {
        GitError::AncestryBudgetExhausted { limit, .. } => refuse::<()>(
            1,
            chk.command,
            format!(
                "This shell read {limit} commits and could not tell what this move drops or whether that is published, so it refuses. \
                 A commit-graph file makes the check short; real git writes one with git commit-graph write --reachable."
            ),
            "Make a new commit instead: git commit -m '<message>'.",
            Override::Repository,
        )
        .unwrap_err(),
        other => other,
    }
}

// ── the commit-graph ────────────────────────────────────────────────────────

/// The generation numbers of `objects/info/commit-graph`, when it exists and
/// no link stands on the way to it.
struct Graph(gix_commitgraph::Graph);

impl Graph {
    fn load(repo: &WriteRepo) -> Option<Graph> {
        let info = repo.common_dir().join("objects").join("info");
        // A link in this path could aim the mapping outside the mount. Not
        // reading the file only costs speed: the walk is exact without it.
        for p in [info.clone(), info.join("commit-graph"), info.join("commit-graphs")] {
            match std::fs::symlink_metadata(&p) {
                Ok(m) if m.file_type().is_symlink() => return None,
                _ => {}
            }
        }
        let has = |name: &str| info.join(name).exists();
        if !has("commit-graph") && !has("commit-graphs") {
            return None;
        }
        // An unreadable graph is a missing one, for the same reason.
        gix_commitgraph::Graph::from_info_dir(&info).ok().map(Graph)
    }

    /// The generation, or `None` for a commit the graph lacks.
    fn generation(&self, oid: &ObjectId) -> Option<u32> {
        self.0.commit_by_id(oid).map(|c| c.generation()).filter(|g| *g > 0)
    }
}

/// The heap key: the generation, and above all of them for an unknown commit.
fn key(graph: Option<&Graph>, oid: &ObjectId) -> u32 {
    graph.and_then(|g| g.generation(oid)).unwrap_or(u32::MAX)
}

// ── is `target` an ancestor of (or equal to) any of `from`? ────────────────

fn reaches(
    repo: &WriteRepo,
    graph: Option<&Graph>,
    budget: &mut Budget,
    from: &[ObjectId],
    target: ObjectId,
) -> Result<bool, GitError> {
    let floor = key(graph, &target);
    let mut seen: HashSet<ObjectId> = HashSet::new();
    let mut stack: Vec<ObjectId> = from.to_vec();
    while let Some(oid) = stack.pop() {
        if oid == target {
            return Ok(true);
        }
        if !seen.insert(oid) {
            continue;
        }
        // An ancestor has a strictly lower generation than its descendant.
        let k = key(graph, &oid);
        if floor != u32::MAX && k != u32::MAX && k < floor {
            continue;
        }
        stack.extend(budget.parents(repo.read(), oid)?);
    }
    Ok(false)
}

// ── remote tips ─────────────────────────────────────────────────────────────

fn remote_tips(repo: &WriteRepo) -> Result<Vec<(String, ObjectId)>, GitError> {
    let read = repo.read();
    let platform = read
        .refs()
        .iter()
        .map_err(|e| GitError::repository(OP, "opening packed-refs", repo.git_dir(), e))?;
    let all = platform
        .all()
        .map_err(|e| GitError::repository(OP, "listing refs", repo.git_dir(), e))?;
    let mut tips: Vec<(String, ObjectId)> = Vec::new();
    for reference in all {
        let reference = reference.map_err(|e| GitError::repository(OP, "reading a ref", repo.git_dir(), e))?;
        let full = String::from_utf8_lossy(reference.name.as_bstr()).into_owned();
        let Some(short_name) = full.strip_prefix("refs/remotes/") else { continue };
        // A symbolic ref whose chain ends nowhere names no commit.
        let Some(oid) = read.ref_commit(&reference)? else { continue };
        tips.push((short_name.to_string(), oid));
    }
    tips.sort();
    // One seed per commit: the first name in sorted order stands for it.
    let mut seen = HashSet::new();
    tips.retain(|(_, oid)| seen.insert(*oid));
    Ok(tips)
}

/// The remote-tracking ref that reaches `commit`, for the refusal's words.
fn name_remote(repo: &WriteRepo, graph: Option<&Graph>, tips: &[(String, ObjectId)], commit: ObjectId) -> String {
    for (name, tip) in tips {
        let mut budget = Budget::new(OP);
        if matches!(reaches(repo, graph, &mut budget, &[*tip], commit), Ok(true)) {
            return name.clone();
        }
    }
    "a remote-tracking ref".to_string()
}

// ── the painting walk ──────────────────────────────────────────────────────

const O: u8 = 1;
const N: u8 = 2;
const R: u8 = 4;

enum Verdict {
    Unpublished,
    /// A commit the move drops that a remote-tracking ref reaches.
    Published(ObjectId),
}

enum Trouble {
    /// The graph contradicted itself.
    Graph,
    Git(GitError),
}

impl From<GitError> for Trouble {
    fn from(e: GitError) -> Self {
        Trouble::Git(e)
    }
}

struct Painter<'a> {
    repo: &'a WriteRepo,
    graph: Option<&'a Graph>,
    budget: &'a mut Budget,
    flags: HashMap<ObjectId, u8>,
    parents: HashMap<ObjectId, Vec<ObjectId>>,
    /// The flags a commit had when it was last expanded.
    expanded: HashMap<ObjectId, u8>,
    heap: BinaryHeap<(u32, ObjectId)>,
    pending: HashSet<ObjectId>,
    /// Pending commits without `N`.
    pending_non_n: usize,
    /// Pending commits with `O` and without `N`.
    pending_o_not_n: usize,
    /// The lowest key of any commit that was ever `O` without `N`.
    low_o_key: u32,
}

fn paint(
    repo: &WriteRepo,
    graph: Option<&Graph>,
    budget: &mut Budget,
    old: ObjectId,
    new_seeds: &[ObjectId],
    tips: &[(String, ObjectId)],
) -> Result<Verdict, Trouble> {
    let mut p = Painter {
        repo,
        graph,
        budget,
        flags: HashMap::new(),
        parents: HashMap::new(),
        expanded: HashMap::new(),
        heap: BinaryHeap::new(),
        pending: HashSet::new(),
        pending_non_n: 0,
        pending_o_not_n: 0,
        low_o_key: u32::MAX,
    };
    p.add(old, O)?;
    for n in new_seeds {
        p.add(*n, N)?;
    }
    for (_, t) in tips {
        p.add(*t, R)?;
    }
    p.run()
}

impl Painter<'_> {
    fn contributions(f: u8) -> (bool, bool) {
        (f & N == 0, f & O != 0 && f & N == 0)
    }

    /// Give `oid` more sides, and queue it when they are new to it.
    fn add(&mut self, oid: ObjectId, bits: u8) -> Result<(), Trouble> {
        let old = self.flags.get(&oid).copied().unwrap_or(0);
        let new = old | bits;
        if new == old {
            return Ok(());
        }
        let k = key(self.graph, &oid);
        if self.graph.is_some() && k != u32::MAX && self.expanded.contains_key(&oid) {
            // A commit in generation order is final when popped.
            return Err(Trouble::Graph);
        }
        self.flags.insert(oid, new);
        if self.pending.contains(&oid) {
            let (a, b) = Self::contributions(old);
            self.pending_non_n -= usize::from(a);
            self.pending_o_not_n -= usize::from(b);
        } else {
            self.pending.insert(oid);
            self.heap.push((k, oid));
        }
        let (a, b) = Self::contributions(new);
        self.pending_non_n += usize::from(a);
        self.pending_o_not_n += usize::from(b);
        if b {
            self.low_o_key = self.low_o_key.min(k);
        }
        Ok(())
    }

    /// A commit painted `O` and `R` and not `N`, among those visited.
    fn find_dropped_published(&self) -> Option<ObjectId> {
        let mut hit: Vec<ObjectId> = self
            .flags
            .iter()
            .filter(|(_, f)| **f & (O | R | N) == O | R)
            .map(|(o, _)| *o)
            .collect();
        hit.sort();
        hit.first().copied()
    }

    fn parents_of(&mut self, oid: ObjectId) -> Result<Vec<ObjectId>, GitError> {
        if let Some(p) = self.parents.get(&oid) {
            return Ok(p.clone());
        }
        let p = self.budget.parents(self.repo.read(), oid)?;
        self.parents.insert(oid, p.clone());
        Ok(p)
    }

    fn run(&mut self) -> Result<Verdict, Trouble> {
        let mut drain = false;
        loop {
            if self.pending.is_empty() {
                return Ok(self.verdict());
            }
            // S1.
            if self.pending_non_n == 0 && !drain {
                match self.find_dropped_published() {
                    None => return Ok(Verdict::Unpublished),
                    Some(_) => drain = true,
                }
            }
            // S2.
            if self.graph.is_some() && self.pending_o_not_n == 0 {
                if let Some(&(top, _)) = self.heap.peek() {
                    if top < self.low_o_key && top != u32::MAX {
                        return Ok(self.verdict());
                    }
                }
            }
            let Some((k, oid)) = self.heap.pop() else { return Ok(self.verdict()) };
            if !self.pending.remove(&oid) {
                continue;
            }
            let f = self.flags[&oid];
            let (a, b) = Self::contributions(f);
            self.pending_non_n -= usize::from(a);
            self.pending_o_not_n -= usize::from(b);
            if self.graph.is_some() && k != u32::MAX && f & (O | R | N) == O | R {
                return Ok(Verdict::Published(oid));
            }
            let parents = self.parents_of(oid)?;
            if self.graph.is_some() && k != u32::MAX {
                for p in &parents {
                    let pk = key(self.graph, p);
                    if pk >= k {
                        return Err(Trouble::Graph);
                    }
                }
            }
            self.expanded.insert(oid, f);
            for p in parents {
                self.add(p, f)?;
            }
        }
    }

    fn verdict(&self) -> Verdict {
        match self.find_dropped_published() {
            Some(c) => Verdict::Published(c),
            None => Verdict::Unpublished,
        }
    }
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use std::path::Path;
    use crate::verbs::write::fixtures::{git_stdout, Fixture};

    fn oid(repo: &Path, rev: &str) -> ObjectId {
        ObjectId::from_hex(git_stdout(repo, &["rev-parse", rev]).as_bytes()).expect("oid")
    }

    fn chk(policy: RewritePolicy) -> MoveCheck<'static> {
        MoveCheck { command: "git reset --soft HEAD~3", mover: Mover::Reset { rev: "HEAD~3" }, policy }
    }

    fn commits(repo: &Path, n: usize) {
        for i in 0..n {
            std::fs::write(repo.join(format!("f{i}")), format!("{i}\n")).expect("file");
            git_stdout(repo, &["add", "."]);
            git_stdout(repo, &["-c", "user.name=T", "-c", "user.email=t@x", "commit", "-q", "-m", &format!("c{i}")]);
        }
    }

    fn publish(repo: &Path, name: &str, rev: &str) {
        git_stdout(repo, &["update-ref", &format!("refs/remotes/{name}"), rev]);
    }

    /// The design's first wording asked only whether HEAD is published. With
    /// origin/main at HEAD~1, HEAD is not, and `reset --soft HEAD~3` still
    /// drops HEAD~1 and HEAD~2, which are.
    #[test]
    fn a_reset_that_drops_a_published_ancestor_of_an_unpublished_head_refuses() {
        let fx = Fixture::new();
        commits(&fx.repo, 4);
        publish(&fx.repo, "origin/main", "HEAD~1");
        let repo = fx.write_repo(false);
        let (head, target) = (oid(&fx.repo, "HEAD"), oid(&fx.repo, "HEAD~3"));
        let err = check_move(&repo, &chk(RewritePolicy::Unpublished), Some(head), &NewTip::Existing(target))
            .expect_err("HEAD~1 is published");
        let text = err.to_string();
        assert_eq!(err.exit_code(), 2, "{text}");
        // Both HEAD~1 and HEAD~2 are dropped and published; either may be named.
        assert!(
            text.contains(&short(oid(&fx.repo, "HEAD~1"))) || text.contains(&short(oid(&fx.repo, "HEAD~2"))),
            "names a dropped published commit: {text}"
        );
        assert!(!text.contains(&short(oid(&fx.repo, "HEAD"))), "HEAD itself is not published: {text}");
        assert!(text.contains("origin/main"), "{text}");
        // And only HEAD is dropped when the reset stops above the published commit.
        check_move(&repo, &chk(RewritePolicy::Unpublished), Some(head), &NewTip::Existing(oid(&fx.repo, "HEAD~1")))
            .expect("dropping HEAD alone is fine: it is not published");
    }

    #[test]
    fn a_forward_move_passes_every_policy_and_a_rewrite_passes_only_where_allowed() {
        let fx = Fixture::new();
        commits(&fx.repo, 3);
        publish(&fx.repo, "origin/main", "HEAD");
        let repo = fx.write_repo(false);
        let (head, back) = (oid(&fx.repo, "HEAD"), oid(&fx.repo, "HEAD~2"));
        for policy in [RewritePolicy::Refuse, RewritePolicy::Unpublished, RewritePolicy::Allow] {
            check_move(&repo, &chk(policy), Some(back), &NewTip::Existing(head)).expect("fast-forward");
            check_move(&repo, &chk(policy), Some(head), &NewTip::Child).expect("a commit");
        }
        check_move(&repo, &chk(RewritePolicy::Allow), Some(head), &NewTip::Existing(back)).expect("allow");
        assert!(check_move(&repo, &chk(RewritePolicy::Refuse), Some(head), &NewTip::Existing(back)).is_err());
        assert!(check_move(&repo, &chk(RewritePolicy::Unpublished), Some(head), &NewTip::Existing(back)).is_err());
    }

    #[test]
    fn no_remote_refs_means_nothing_is_published() {
        let fx = Fixture::new();
        commits(&fx.repo, 3);
        let repo = fx.write_repo(false);
        let (head, back) = (oid(&fx.repo, "HEAD"), oid(&fx.repo, "HEAD~2"));
        check_move(&repo, &chk(RewritePolicy::Unpublished), Some(head), &NewTip::Existing(back)).expect("nothing published");
    }

    #[test]
    fn a_walk_past_its_budget_refuses_instead_of_guessing() {
        let fx = Fixture::new();
        commits(&fx.repo, 12);
        // The remote is far from HEAD on a side line, so the walk has to read.
        publish(&fx.repo, "origin/side", "HEAD~11");
        let repo = fx.write_repo(false);
        let (head, back) = (oid(&fx.repo, "HEAD"), oid(&fx.repo, "HEAD~2"));
        let err = check_move_with(
            &repo,
            &chk(RewritePolicy::Unpublished),
            Some(head),
            &NewTip::Existing(back),
            Budget::with_limit(OP, 3),
        )
        .expect_err("three reads cannot decide");
        let text = err.to_string();
        assert_eq!(err.exit_code(), 1, "{text}");
        assert!(text.contains("could not tell"), "{text}");
        assert!(text.contains("Nothing was changed."), "{text}");
        // The same move with room to read is decided.
        check_move(&repo, &chk(RewritePolicy::Unpublished), Some(head), &NewTip::Existing(back))
            .expect("HEAD~11 is far below what HEAD~3.. drops");
    }

    #[test]
    fn a_shallow_repository_refuses_under_unpublished() {
        let fx = Fixture::new();
        commits(&fx.repo, 3);
        publish(&fx.repo, "origin/main", "HEAD~2");
        std::fs::write(fx.repo.join(".git/shallow"), format!("{}\n", oid(&fx.repo, "HEAD~2"))).expect("shallow");
        let repo = fx.write_repo(false);
        let (head, back) = (oid(&fx.repo, "HEAD"), oid(&fx.repo, "HEAD~1"));
        let err = check_move(&repo, &chk(RewritePolicy::Unpublished), Some(head), &NewTip::Existing(back)).expect_err("shallow");
        assert_eq!(err.exit_code(), 4, "{err}");
        assert!(err.to_string().contains("shallow"), "{err}");
    }

    /// Committer dates that run backwards along parent edges. A walk that
    /// stops when the newest pending commit is common history would stop
    /// early here and call a published commit unpublished.
    #[test]
    fn skewed_committer_dates_do_not_change_the_answer() {
        let fx = Fixture::new();
        let r = &fx.repo;
        let at = |i: usize, date: &str| {
            std::fs::write(r.join(format!("s{i}")), format!("{i}\n")).expect("file");
            git_stdout(r, &["add", "."]);
            let out = std::process::Command::new("git")
                .current_dir(r)
                .args(["-c", "user.name=T", "-c", "user.email=t@x", "commit", "-q", "-m", &format!("s{i}")])
                .env("GIT_COMMITTER_DATE", date)
                .env("GIT_AUTHOR_DATE", date)
                .env("GIT_CONFIG_NOSYSTEM", "1")
                .env("GIT_CONFIG_GLOBAL", "/dev/null")
                .output()
                .expect("git");
            assert!(out.status.success());
        };
        // Parents are LATER than their children, and the published commit
        // (s1) is the oldest by date.
        at(1, "2030-01-05T00:00:00+00:00");
        at(2, "2020-01-01T00:00:00+00:00");
        at(3, "2029-01-01T00:00:00+00:00");
        at(4, "2010-01-01T00:00:00+00:00");
        publish(r, "origin/main", "HEAD~2");
        let repo = fx.write_repo(false);
        let (head, back) = (oid(r, "HEAD"), oid(r, "HEAD~3"));
        let err = check_move(&repo, &chk(RewritePolicy::Unpublished), Some(head), &NewTip::Existing(back))
            .expect_err("HEAD~2 is published and dropped");
        assert!(err.to_string().contains(&short(oid(r, "HEAD~2"))), "{err}");
        // With a commit-graph the answer is the same.
        git_stdout(r, &["commit-graph", "write", "--reachable"]);
        let err = check_move(&repo, &chk(RewritePolicy::Unpublished), Some(head), &NewTip::Existing(back))
            .expect_err("same answer with generation numbers");
        assert!(err.to_string().contains(&short(oid(r, "HEAD~2"))), "{err}");
        check_move(&repo, &chk(RewritePolicy::Unpublished), Some(head), &NewTip::Existing(oid(r, "HEAD~2")))
            .expect("dropping only the unpublished tail");
    }

    #[test]
    fn a_commit_graph_gives_the_same_verdicts() {
        let fx = Fixture::new();
        commits(&fx.repo, 6);
        publish(&fx.repo, "origin/main", "HEAD~2");
        git_stdout(&fx.repo, &["commit-graph", "write", "--reachable"]);
        let repo = fx.write_repo(false);
        let head = oid(&fx.repo, "HEAD");
        for (back, refused) in [("HEAD~1", false), ("HEAD~2", false), ("HEAD~3", true), ("HEAD~5", true)] {
            let r = check_move(&repo, &chk(RewritePolicy::Unpublished), Some(head), &NewTip::Existing(oid(&fx.repo, back)));
            assert_eq!(r.is_err(), refused, "{back}: {r:?}");
        }
        // An amend of HEAD: its parent is the new tip's side.
        let parents = vec![oid(&fx.repo, "HEAD~1")];
        check_move(&repo, &chk(RewritePolicy::Unpublished), Some(head), &NewTip::Replacement(parents.clone()))
            .expect("HEAD is not published");
        publish(&fx.repo, "origin/main", "HEAD");
        assert!(check_move(&repo, &chk(RewritePolicy::Unpublished), Some(head), &NewTip::Replacement(parents)).is_err());
    }

    /// The stop rule S2 only runs when the file's numbers are real. This pins
    /// that git's current `commit-graph write` produces numbers that
    /// `gix-commitgraph` reads as a strictly increasing level, so the graph
    /// path is the one under test above and not a silent fallback.
    #[test]
    fn the_commit_graph_git_writes_has_usable_generation_numbers() {
        let fx = Fixture::new();
        commits(&fx.repo, 4);
        git_stdout(&fx.repo, &["commit-graph", "write", "--reachable"]);
        let repo = fx.write_repo(false);
        let graph = Graph::load(&repo).expect("the graph loads");
        let gens: Vec<u32> = ["HEAD~3", "HEAD~2", "HEAD~1", "HEAD"]
            .iter()
            .map(|r| graph.generation(&oid(&fx.repo, r)).expect("in the graph"))
            .collect();
        assert!(gens[0] >= 1 && gens.windows(2).all(|w| w[0] < w[1]), "{gens:?}");
        // A link on the way refuses the file: the walk falls back to exact.
        std::fs::remove_file(fx.repo.join(".git/objects/info/commit-graph")).expect("rm");
        assert!(Graph::load(&repo).is_none());
    }
}
