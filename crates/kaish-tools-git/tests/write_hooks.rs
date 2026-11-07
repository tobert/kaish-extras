//! The hook boundary of the ref-moving verbs (docs/design/git-write-profile.md
//! §5.1): `reference_transaction`, `post_checkout` and `post_rewrite`, called
//! by `commit`, `commit --amend`, `reset`, `branch`, `tag`, `switch -c` and
//! `checkout -b`, with git as the oracle.
//!
//! The oracle is a TWIN repository with scripts that log what git passes
//! them. Git runs the hook per transaction (`prepared`, `committed`, then the
//! next transaction), and this crate sends every `prepared` before its first
//! write, so the two logs are compared per state, and the order of this
//! crate's calls is pinned on its own.
//!
//! The recording provider panics with `unimplemented!` on every method a test
//! does not mean to reach. A silent `Ok` would let a verb call a hook it
//! should not and pass.

#![cfg(feature = "commit")]

#[path = "support.rs"]
mod support;

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::{Arc, Mutex};

use kaish_tool_api::Tool;
use kaish_tools_git::{
    GitConfig, GitHooks, HookContext, HookError, HookOutput, RefTransactionState, RefUpdate, RewrittenCommit,
};
use kaish_types::Value;

use support::{Fixture, StrictBackend, TestCtx};

const DATE: &str = "2026-08-01T10:00:00+00:00";

fn git_raw(cwd: &Path, args: &[&str]) -> (i32, String, String) {
    let out = Command::new("git")
        .args(args)
        .current_dir(cwd)
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_SYSTEM", "/dev/null")
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("XDG_CONFIG_HOME", "/nonexistent-kaish-tools-git-hermetic-xdg")
        .env("GIT_TERMINAL_PROMPT", "0")
        .env("GIT_AUTHOR_NAME", "Fixture Author")
        .env("GIT_AUTHOR_EMAIL", "author@example.invalid")
        .env("GIT_COMMITTER_NAME", "Fixture Committer")
        .env("GIT_COMMITTER_EMAIL", "committer@example.invalid")
        .env("GIT_AUTHOR_DATE", DATE)
        .env("GIT_COMMITTER_DATE", DATE)
        .output()
        .expect("run git");
    (
        out.status.code().unwrap_or(-1),
        String::from_utf8_lossy(&out.stdout).into_owned(),
        String::from_utf8_lossy(&out.stderr).into_owned(),
    )
}

fn default_vars() -> Vec<(&'static str, &'static str)> {
    vec![
        ("GIT_AUTHOR_NAME", "Fixture Author"),
        ("GIT_AUTHOR_EMAIL", "author@example.invalid"),
        ("GIT_COMMITTER_NAME", "Fixture Committer"),
        ("GIT_COMMITTER_EMAIL", "committer@example.invalid"),
        ("GIT_AUTHOR_DATE", DATE),
        ("GIT_COMMITTER_DATE", DATE),
    ]
}

async fn run_with(root: &Path, cwd: &str, argv: &[&str], config: kaish_tools_git::WritableGitConfig) -> kaish_types::ExecResult {
    let backend = Arc::new(StrictBackend::single(PathBuf::from("/mnt"), root));
    let mut ctx = TestCtx::new(backend, cwd);
    for (k, v) in default_vars() {
        kaish_tool_api::ToolCtx::set_var(&mut ctx, k, Value::String(v.to_string()));
    }
    let git = kaish_tools_git::tool(config).expect("config builds");
    let (verb, rest) = argv.split_first().expect("a verb");
    git.execute(support::verb_words(verb, rest), &mut ctx).await
}

fn base(dir: &Path) {
    support::write_file(dir, "a.txt", "alpha\n");
    support::git(dir, &["add", "."]);
    git_raw(dir, &["commit", "-qm", "one"]);
    support::write_file(dir, "a.txt", "alpha two\n");
    git_raw(dir, &["commit", "-qam", "two"]);
}

/// `base` plus a staged change, for `commit` and `commit --amend`.
fn staged(dir: &Path) {
    base(dir);
    support::write_file(dir, "a.txt", "alpha three\n");
    support::git(dir, &["add", "a.txt"]);
}

fn detached(dir: &Path) {
    staged(dir);
    support::git(dir, &["checkout", "-q", "--detach"]);
}

/// A staged file and no commit.
fn unborn(dir: &Path) {
    support::write_file(dir, "a.txt", "alpha\n");
    support::git(dir, &["add", "."]);
}

/// What the provider heard, one string per call.
#[derive(Default)]
struct Rec {
    calls: Mutex<Vec<String>>,
    trees: Mutex<Vec<Option<String>>>,
    /// Refuse `prepared` for a transaction that names this ref.
    veto_ref: Option<&'static str>,
    /// Return `Err` at this state, for every transaction.
    err_at: Option<RefTransactionState>,
    /// Does `pre_commit`, `commit_msg` and `post_commit` belong in this test?
    commit_hooks: bool,
    /// Called inside `Prepared`, to act like another writer.
    on_prepared: Option<Box<dyn Fn() + Send + Sync>>,
}

impl Rec {
    fn push(&self, line: String) {
        self.calls.lock().expect("lock").push(line);
    }
    fn calls(&self) -> Vec<String> {
        self.calls.lock().expect("lock").clone()
    }
}

fn joined(updates: &[RefUpdate]) -> String {
    updates.iter().map(RefUpdate::line).collect::<Vec<_>>().join("|")
}

impl GitHooks for Rec {
    fn pre_commit(&self, ctx: &HookContext<'_>) -> Result<HookOutput, HookError> {
        assert!(self.commit_hooks, "pre_commit is not part of this test");
        self.trees.lock().expect("lock").push(ctx.tree.map(str::to_string));
        self.push("pre-commit".to_string());
        Ok(HookOutput::none())
    }

    fn commit_msg(&self, _: &HookContext<'_>, _: &mut String) -> Result<HookOutput, HookError> {
        assert!(self.commit_hooks, "commit_msg is not part of this test");
        self.push("commit-msg".to_string());
        Ok(HookOutput::none())
    }

    fn post_commit(&self, _: &HookContext<'_>, _: &str) -> HookOutput {
        assert!(self.commit_hooks, "post_commit is not part of this test");
        self.push("post-commit".to_string());
        HookOutput::none()
    }

    fn reference_transaction(
        &self,
        ctx: &HookContext<'_>,
        state: RefTransactionState,
        updates: &[RefUpdate],
    ) -> Result<HookOutput, HookError> {
        self.trees.lock().expect("lock").push(ctx.tree.map(str::to_string));
        self.push(format!("rt {} {}", state.as_str(), joined(updates)));
        if state == RefTransactionState::Prepared {
            if let Some(f) = &self.on_prepared {
                f();
            }
            if self.veto_ref.is_some_and(|r| updates.iter().any(|u| u.name == r)) {
                return Err(HookError::new("reference-transaction", "no ref moves today"));
            }
        }
        if self.err_at == Some(state) {
            return Err(HookError::new("reference-transaction", "the provider broke"));
        }
        Ok(HookOutput::none())
    }

    fn post_checkout(&self, _: &HookContext<'_>, previous: &str, new: &str, branch_checkout: bool) -> HookOutput {
        self.push(format!("post-checkout {previous} {new} {}", u8::from(branch_checkout)));
        HookOutput::none()
    }

    fn post_rewrite(&self, _: &HookContext<'_>, command: &str, rewritten: &[RewrittenCommit]) -> HookOutput {
        for r in rewritten {
            self.push(format!("post-rewrite {command} {}", r.line()));
        }
        HookOutput::none()
    }
}

struct Pair {
    fx: Fixture,
    ours: PathBuf,
    twin: PathBuf,
}

impl Pair {
    fn new(setup: &dyn Fn(&Path)) -> Self {
        support::require_git();
        let fx = Fixture::empty();
        let ours = fx.path("ours");
        let twin = fx.path("twin");
        for dir in [&ours, &twin] {
            std::fs::create_dir_all(dir).expect("dir");
            support::git(dir, &["init", "-q", "-b", "main"]);
            setup(dir);
        }
        Pair { fx, ours, twin }
    }

    async fn run(&self, rec: &Arc<Rec>, argv: &[&str]) -> kaish_types::ExecResult {
        let config = GitConfig::read_only().with_commit_profile().with_hooks(rec.clone());
        run_with(&self.fx.root(), "/mnt/ours", argv, config).await
    }

    fn git_dir(&self) -> PathBuf {
        self.ours.join(".git")
    }

    /// Scripts in the twin that log what git passes, in the recorder's format.
    fn log_twin_hooks(&self) -> PathBuf {
        let log = self.fx.path("twin-hooks.log");
        let dir = self.twin.join(".git/hooks");
        let script = |name: &str, body: &str| {
            let path = dir.join(name);
            std::fs::write(&path, format!("#!/bin/sh\n{body}\n")).expect("hook");
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).expect("chmod");
        };
        let log_s = log.display();
        script(
            "reference-transaction",
            &format!(
                "case \"$1\" in prepared|committed|aborted) ;; *) cat >/dev/null; exit 0;; esac\n\
                 lines=$(grep -v ' AUTO_MERGE$' | tr '\\n' '|')\n\
                 [ -n \"$lines\" ] && echo \"rt $1 ${{lines%|}}\" >> '{log_s}'\n\
                 exit 0"
            ),
        );
        script("post-commit", &format!("echo post-commit >> '{log_s}'"));
        script("post-checkout", &format!("echo \"post-checkout $1 $2 $3\" >> '{log_s}'"));
        script("post-rewrite", &format!("while read l; do echo \"post-rewrite $1 $l\" >> '{log_s}'; done"));
        log
    }
}

/// The calls that a git hook script also sees, grouped by kind. This crate's
/// `pre-commit` and `commit-msg` calls have no twin script and are dropped.
fn by_kind(calls: &[String]) -> BTreeMap<String, Vec<String>> {
    let mut out: BTreeMap<String, Vec<String>> = BTreeMap::new();
    for c in calls {
        if c == "pre-commit" || c == "commit-msg" {
            continue;
        }
        let kind = if let Some(rest) = c.strip_prefix("rt ") {
            format!("rt {}", rest.split(' ').next().expect("state"))
        } else {
            c.split(' ').next().expect("kind").to_string()
        };
        out.entry(kind).or_default().push(c.clone());
    }
    out
}

fn twin_calls(log: &Path) -> Vec<String> {
    std::fs::read_to_string(log).unwrap_or_default().lines().map(str::to_string).collect()
}

/// Fingerprint differences that are not in the object database.
fn changed_outside_objects(after: &support::Fingerprint, before: &support::Fingerprint) -> Vec<String> {
    after.differences_from(before).into_iter().filter(|d| !d.starts_with("objects")).collect()
}

type Setup = fn(&Path);
type Case = (&'static str, Setup, &'static [&'static str]);
/// (label, setup, argv, the ref to refuse, whether an object is written first)
type VetoCase = (&'static str, Setup, &'static [&'static str], &'static str, bool);
/// (label, setup, argv, the lock to plant, how many transactions are prepared)
type LockCase = (&'static str, Setup, &'static [&'static str], &'static str, usize);

/// Every verb that moves a ref, in one spelling or more.
const CASES: &[Case] = &[
    ("branch", base, &["branch", "x"]),
    ("branch with a start", base, &["branch", "x", "HEAD~1"]),
    ("tag", base, &["tag", "x"]),
    ("annotated tag", base, &["tag", "-a", "x", "-m", "m"]),
    ("switch -c", base, &["switch", "-c", "x"]),
    ("checkout -b", base, &["checkout", "-b", "x"]),
    ("switch -c on an unborn branch", unborn, &["switch", "-c", "x"]),
    ("switch -c on a detached HEAD", detached, &["switch", "-c", "x"]),
    ("commit", staged, &["commit", "-m", "three"]),
    ("commit on an unborn branch", unborn, &["commit", "-m", "first"]),
    ("commit on a detached HEAD", detached, &["commit", "-m", "three"]),
    ("commit --amend", staged, &["commit", "--amend", "-m", "three"]),
    ("reset --soft", staged, &["reset", "--soft", "HEAD~1"]),
    ("reset --mixed", staged, &["reset", "HEAD~1"]),
    ("reset", staged, &["reset"]),
    ("reset HEAD", staged, &["reset", "HEAD"]),
    ("reset --mixed with no revision", staged, &["reset", "--mixed"]),
    ("reset on a detached HEAD", detached, &["reset", "--soft", "HEAD~1"]),
    ("reset with a path", staged, &["reset", "--", "a.txt"]),
];

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn each_verb_tells_the_provider_what_git_tells_its_scripts() {
    for (label, setup, argv) in CASES {
        let p = Pair::new(setup);
        let log = p.log_twin_hooks();
        let rec = Arc::new(Rec { commit_hooks: argv[0] == "commit", ..Rec::default() });
        let r = p.run(&rec, argv).await;
        assert_eq!(r.code, 0, "[{label}] {}", r.err);
        let (code, _, err) = git_raw(&p.twin, argv);
        assert_eq!(code, 0, "[{label}] {err}");
        let ours = by_kind(&rec.calls());
        let git = by_kind(&twin_calls(&log));
        assert_eq!(ours, git, "[{label}] the calls differ from what git's scripts saw");
        // A negative control: a case that moves a ref was compared on something.
        let moves_a_ref = !matches!(*label, "reset with a path");
        assert_eq!(!git.is_empty(), moves_a_ref, "[{label}] git's own log: {git:?}");
        assert!(!rec.calls().iter().any(|c| c.contains("AUTO_MERGE")), "[{label}] {:?}", rec.calls());
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_calls_come_in_this_order() {
    let main_tip = |p: &Pair| git_raw(&p.ours, &["rev-parse", "main"]).1.trim().to_string();
    let zero = "0".repeat(40);

    // commit: the commit's own hooks, then the transaction around the branch move.
    let p = Pair::new(&staged);
    let before = main_tip(&p);
    let rec = Arc::new(Rec { commit_hooks: true, ..Rec::default() });
    assert_eq!(p.run(&rec, &["commit", "-m", "three"]).await.code, 0);
    let after = main_tip(&p);
    assert_eq!(
        rec.calls(),
        [
            "pre-commit".to_string(),
            "commit-msg".to_string(),
            format!("rt prepared {before} {after} refs/heads/main"),
            format!("rt committed {before} {after} refs/heads/main"),
            "post-commit".to_string(),
        ]
    );
    // The commit has a tree, and says so; the other verbs do not.
    let tree = git_raw(&p.ours, &["rev-parse", "HEAD^{tree}"]).1.trim().to_string();
    assert!(rec.trees.lock().expect("lock").iter().all(|t| t.as_deref() == Some(tree.as_str())));

    // amend: post-commit, then post-rewrite with the old and the new commit.
    let p = Pair::new(&staged);
    let before = main_tip(&p);
    let rec = Arc::new(Rec { commit_hooks: true, ..Rec::default() });
    assert_eq!(p.run(&rec, &["commit", "--amend", "-m", "again"]).await.code, 0);
    let after = main_tip(&p);
    assert_eq!(
        rec.calls(),
        [
            "pre-commit".to_string(),
            "commit-msg".to_string(),
            format!("rt prepared {before} {after} refs/heads/main"),
            format!("rt committed {before} {after} refs/heads/main"),
            "post-commit".to_string(),
            format!("post-rewrite amend {before} {after}"),
        ]
    );

    // reset: ORIG_HEAD, then the branch. Both are prepared before either is written.
    let p = Pair::new(&staged);
    let before = main_tip(&p);
    let parent = git_raw(&p.ours, &["rev-parse", "HEAD~1"]).1.trim().to_string();
    let rec = Arc::new(Rec::default());
    assert_eq!(p.run(&rec, &["reset", "--soft", "HEAD~1"]).await.code, 0);
    assert_eq!(
        rec.calls(),
        [
            format!("rt prepared {zero} {before} ORIG_HEAD"),
            format!("rt prepared {before} {parent} refs/heads/main"),
            format!("rt committed {zero} {before} ORIG_HEAD"),
            format!("rt committed {before} {parent} refs/heads/main"),
        ]
    );
    assert!(rec.trees.lock().expect("lock").iter().all(Option::is_none), "a reset makes no commit, so it has no tree");

    // The second reset sees the first one's ORIG_HEAD as the old value.
    let rec = Arc::new(Rec::default());
    assert_eq!(p.run(&rec, &["reset"]).await.code, 0);
    let calls = rec.calls();
    assert_eq!(calls[0], format!("rt prepared {before} {parent} ORIG_HEAD"), "{calls:?}");
    assert_eq!(calls[1], format!("rt prepared {parent} {parent} refs/heads/main"), "{calls:?}");

    // branch and tag: one transaction each, from the null oid.
    for (argv, name) in [(vec!["branch", "x"], "refs/heads/x"), (vec!["tag", "x"], "refs/tags/x"), (vec!["tag", "-a", "x", "-m", "m"], "refs/tags/x")] {
        let p = Pair::new(&base);
        let rec = Arc::new(Rec::default());
        assert_eq!(p.run(&rec, &argv).await.code, 0, "{argv:?}");
        let new = git_raw(&p.ours, &["rev-parse", name]).1.trim().to_string();
        assert_eq!(
            rec.calls(),
            [format!("rt prepared {zero} {new} {name}"), format!("rt committed {zero} {new} {name}")],
            "{argv:?}"
        );
    }

    // switch -c: the branch, then HEAD naming it, then post-checkout.
    for argv in [vec!["switch", "-c", "x"], vec!["checkout", "-b", "x"]] {
        let p = Pair::new(&base);
        let tip = main_tip(&p);
        let rec = Arc::new(Rec::default());
        assert_eq!(p.run(&rec, &argv).await.code, 0, "{argv:?}");
        assert_eq!(
            rec.calls(),
            [
                format!("rt prepared {zero} {tip} refs/heads/x"),
                format!("rt prepared {zero} ref:refs/heads/x HEAD"),
                format!("rt committed {zero} {tip} refs/heads/x"),
                format!("rt committed {zero} ref:refs/heads/x HEAD"),
                format!("post-checkout {tip} {tip} 1"),
            ],
            "{argv:?}"
        );
    }

    // switch -c with no commit: only HEAD changes, and git runs no post-checkout.
    let p = Pair::new(&unborn);
    let rec = Arc::new(Rec::default());
    assert_eq!(p.run(&rec, &["switch", "-c", "x"]).await.code, 0);
    assert_eq!(
        rec.calls(),
        [format!("rt prepared {zero} ref:refs/heads/x HEAD"), format!("rt committed {zero} ref:refs/heads/x HEAD")]
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_hook_file_does_not_run_and_does_not_refuse_when_a_provider_is_installed() {
    let sentinel = |p: &Pair| p.fx.path("sentinel");
    for (setup, argv) in [(base as fn(&Path), vec!["branch", "x"]), (base, vec!["tag", "x"]), (base, vec!["switch", "-c", "x"]), (staged, vec!["reset"]), (staged, vec!["commit", "-m", "c"])] {
        let p = Pair::new(&setup);
        let s = sentinel(&p);
        for name in ["reference-transaction", "post-checkout", "post-commit", "post-rewrite"] {
            let path = p.ours.join(".git/hooks").join(name);
            std::fs::write(&path, format!("#!/bin/sh\ntouch '{}'\nexit 1\n", s.display())).expect("hook");
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).expect("chmod");
        }
        let rec = Arc::new(Rec { commit_hooks: argv[0] == "commit", ..Rec::default() });
        let r = p.run(&rec, &argv).await;
        assert_eq!(r.code, 0, "{argv:?}: {}", r.err);
        assert!(!s.exists(), "{argv:?}: this crate spawned a hook");
        assert!(rec.calls().iter().any(|c| c.starts_with("rt prepared")), "{argv:?}: the provider heard nothing");
    }
}

/// Each case is refused at `prepared`: exit 1, nothing written but objects.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_veto_at_prepared_leaves_git_untouched_and_says_what_was_written() {
    let cases: &[VetoCase] = &[
        ("branch", base, &["branch", "x"], "refs/heads/x", false),
        ("tag", base, &["tag", "x"], "refs/tags/x", false),
        ("annotated tag", base, &["tag", "-a", "x", "-m", "m"], "refs/tags/x", true),
        ("switch -c", base, &["switch", "-c", "x"], "refs/heads/x", false),
        ("switch -c, HEAD's transaction", base, &["switch", "-c", "x"], "HEAD", false),
        ("checkout -b", base, &["checkout", "-b", "x"], "refs/heads/x", false),
        ("switch -c on an unborn branch", unborn, &["switch", "-c", "x"], "HEAD", false),
        ("commit", staged, &["commit", "-m", "c"], "refs/heads/main", true),
        ("commit -a", staged, &["commit", "-a", "-m", "c"], "refs/heads/main", true),
        ("commit on an unborn branch", unborn, &["commit", "-m", "c"], "refs/heads/main", true),
        ("commit on a detached HEAD", detached, &["commit", "-m", "c"], "HEAD", true),
        ("commit --amend", staged, &["commit", "--amend", "-m", "c"], "refs/heads/main", true),
        ("reset --soft, the branch", staged, &["reset", "--soft", "HEAD~1"], "refs/heads/main", false),
        ("reset --soft, ORIG_HEAD", staged, &["reset", "--soft", "HEAD~1"], "ORIG_HEAD", false),
        ("reset", staged, &["reset", "HEAD~1"], "refs/heads/main", false),
        ("plain reset, ORIG_HEAD", staged, &["reset"], "ORIG_HEAD", false),
    ];
    for (label, setup, argv, veto, objects) in cases {
        let p = Pair::new(setup);
        let before = support::Fingerprint::take(&p.git_dir());
        let rec = Arc::new(Rec { veto_ref: Some(veto), commit_hooks: argv[0] == "commit", ..Rec::default() });
        let r = p.run(&rec, argv).await;
        assert_eq!(r.code, 1, "[{label}] {}", r.err);
        assert!(r.err.contains("reference-transaction hook refused the"), "[{label}] {}", r.err);
        assert!(r.err.contains("no ref moves today"), "[{label}] {}", r.err);
        let after = support::Fingerprint::take(&p.git_dir());
        let diffs = if *objects { changed_outside_objects(&after, &before) } else { after.differences_from(&before) };
        assert!(diffs.is_empty(), "[{label}] a veto changed .git: {diffs:?}");
        if *objects {
            assert!(r.err.contains("no ref or index changed"), "[{label}] {}", r.err);
            assert!(!r.err.contains("Nothing was changed"), "[{label}] {}", r.err);
        } else {
            assert!(r.err.contains("Nothing was changed."), "[{label}] {}", r.err);
        }
        // Every transaction that was prepared is aborted, and none is committed.
        let calls = rec.calls();
        assert!(!calls.iter().any(|c| c.starts_with("rt committed")), "[{label}] {calls:?}");
        assert!(!calls.iter().any(|c| c.starts_with("post-")), "[{label}] {calls:?}");
        let prepared = calls.iter().filter(|c| c.starts_with("rt prepared")).count();
        let aborted = calls.iter().filter(|c| c.starts_with("rt aborted")).count();
        assert!(prepared >= 1 && aborted >= 1 && aborted <= prepared, "[{label}] {calls:?}");
        // Git agrees that a veto at prepared stops the command with exit 128.
        let twin_veto = p.fx.path("twin-veto");
        let path = p.twin.join(".git/hooks/reference-transaction");
        std::fs::write(&path, format!("#!/bin/sh\n[ \"$1\" = prepared ] && {{ touch '{}'; exit 1; }}\nexit 0\n", twin_veto.display())).expect("hook");
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).expect("chmod");
        let (code, _, _) = git_raw(&p.twin, argv);
        assert!(twin_veto.exists(), "[{label}] git never reached prepared");
        assert!(code != 0, "[{label}] git ran the command despite the veto");
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_veto_of_the_second_transaction_aborts_the_first_one_too() {
    let p = Pair::new(&staged);
    let rec = Arc::new(Rec { veto_ref: Some("refs/heads/main"), ..Rec::default() });
    let r = p.run(&rec, &["reset", "--soft", "HEAD~1"]).await;
    assert_eq!(r.code, 1, "{}", r.err);
    let calls = rec.calls();
    assert_eq!(calls.len(), 4, "{calls:?}");
    assert!(calls[0].starts_with("rt prepared") && calls[0].ends_with("ORIG_HEAD"), "{calls:?}");
    assert!(calls[1].starts_with("rt prepared") && calls[1].ends_with("refs/heads/main"), "{calls:?}");
    assert!(calls[2].starts_with("rt aborted") && calls[2].ends_with("ORIG_HEAD"), "{calls:?}");
    assert!(calls[3].starts_with("rt aborted") && calls[3].ends_with("refs/heads/main"), "{calls:?}");
    assert!(!p.git_dir().join("ORIG_HEAD").exists(), "ORIG_HEAD was written before the veto");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_error_at_committed_or_aborted_is_shown_and_changes_nothing() {
    let p = Pair::new(&base);
    let rec = Arc::new(Rec { err_at: Some(RefTransactionState::Committed), ..Rec::default() });
    let r = p.run(&rec, &["branch", "x"]).await;
    assert_eq!(r.code, 0, "{}", r.err);
    assert!(r.err.contains("failed at committed: the provider broke"), "{}", r.err);
    assert_eq!(git_raw(&p.ours, &["rev-parse", "--verify", "refs/heads/x"]).0, 0, "the branch was made");

    // Aborted: a lock appears after `prepared`, as another process would take it.
    let p = Pair::new(&base);
    let lock = p.ours.join(".git/refs/heads/y.lock");
    let rec = Arc::new(Rec {
        err_at: Some(RefTransactionState::Aborted),
        on_prepared: Some(Box::new({
            let lock = lock.clone();
            move || std::fs::write(&lock, "").expect("lock")
        })),
        ..Rec::default()
    });
    let r = p.run(&rec, &["branch", "y"]).await;
    assert_eq!(r.code, 4, "{}", r.err);
    let calls = rec.calls();
    assert_eq!(calls.len(), 2, "{calls:?}");
    assert!(calls[0].starts_with("rt prepared") && calls[1].starts_with("rt aborted"), "{calls:?}");
    assert!(!p.git_dir().join("refs/heads/y").exists());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_write_that_fails_after_prepared_is_aborted_for_each_verb() {
    // A lock another writer takes between `prepared` and the write.
    let cases: &[LockCase] = &[
        ("tag", base, &["tag", "x"], "refs/tags/x.lock", 1),
        ("annotated tag", base, &["tag", "-a", "x", "-m", "m"], "refs/tags/x.lock", 1),
        ("switch -c", base, &["switch", "-c", "x"], "refs/heads/x.lock", 2),
        ("commit", staged, &["commit", "-m", "c"], "refs/heads/main.lock", 1),
        ("reset --soft", staged, &["reset", "--soft", "HEAD~1"], "refs/heads/main.lock", 2),
    ];
    for (label, setup, argv, lock, prepared) in cases {
        let p = Pair::new(setup);
        let lock = p.ours.join(".git").join(lock);
        let rec = Arc::new(Rec {
            commit_hooks: argv[0] == "commit",
            on_prepared: Some(Box::new({
                let lock = lock.clone();
                move || {
                    let _ = std::fs::write(&lock, "");
                }
            })),
            ..Rec::default()
        });
        let r = p.run(&rec, argv).await;
        assert_ne!(r.code, 0, "[{label}] the lock did not stop the write: {}", r.err);
        let calls = rec.calls();
        let n = |kind: &str| calls.iter().filter(|c| c.starts_with(kind)).count();
        assert_eq!(n("rt prepared"), *prepared, "[{label}] {calls:?}");
        assert!(n("rt aborted") >= 1, "[{label}] nothing was aborted: {calls:?}");
        assert!(n("post-") == 0, "[{label}] a post hook ran after a failed write: {calls:?}");
        // What the move left committed is not reported as aborted: the
        // branch, or ORIG_HEAD of a reset, was written before the lock stopped the rest.
        assert_eq!(n("rt committed") + n("rt aborted"), *prepared, "[{label}] every prepared transaction ends once: {calls:?}");
    }
}
