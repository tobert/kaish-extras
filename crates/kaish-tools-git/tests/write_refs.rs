//! Phase 4: `branch <name>`, `tag <name>`, `switch -c` and `checkout -b`
//! against real git as the oracle.
//!
//! Every comparison test builds a TWIN: two repositories made by the same
//! commands, so their object ids agree. Real git acts in the twin with fixed
//! identity and dates, this crate acts in the other with the same identity and
//! dates given as kernel variables, and the two are compared on:
//!
//! - every ref and its oid (`for-each-ref`) and the `HEAD` file;
//! - every reflog under `.git/logs`, byte for byte (the dates are fixed, so
//!   nothing is excluded), which also shows a tag got none;
//! - `git cat-file -p` of an annotated tag, and its oid;
//! - `git fsck --strict` on ours, and no stray lock or temp file.

#![cfg(feature = "commit")]

#[path = "support.rs"]
mod support;

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Arc;

use kaish_tool_api::Tool;
use kaish_tools_git::GitConfig;
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

async fn run_with(
    root: &Path,
    cwd: &str,
    argv: &[&str],
    vars: &[(&str, &str)],
    config: kaish_tools_git::WritableGitConfig,
) -> kaish_types::ExecResult {
    let backend = Arc::new(StrictBackend::single(PathBuf::from("/mnt"), root));
    let mut ctx = TestCtx::new(backend, cwd);
    for (k, v) in vars {
        kaish_tool_api::ToolCtx::set_var(&mut ctx, k, Value::String((*v).to_string()));
    }
    let git = kaish_tools_git::tool(config).expect("config builds");
    let (verb, rest) = argv.split_first().expect("a verb");
    git.execute(support::verb_words(verb, rest), &mut ctx).await
}

fn commit_profile() -> kaish_tools_git::WritableGitConfig {
    GitConfig::read_only().with_commit_profile()
}

fn read_log(repo: &Path, name: &str) -> String {
    std::fs::read_to_string(repo.join(".git").join(name)).unwrap_or_default()
}

/// Every file under `<git>/logs`, by relative path.
fn logs_of(git_dir: &Path) -> BTreeMap<String, String> {
    let mut out = BTreeMap::new();
    fn walk(root: &Path, dir: &Path, out: &mut BTreeMap<String, String>) {
        let Ok(read) = std::fs::read_dir(dir) else { return };
        for e in read {
            let p = e.expect("entry").path();
            if p.is_dir() {
                walk(root, &p, out);
            } else {
                out.insert(
                    p.strip_prefix(root).expect("under root").display().to_string(),
                    std::fs::read_to_string(&p).expect("log"),
                );
            }
        }
    }
    walk(git_dir, &git_dir.join("logs"), &mut out);
    out
}

fn lockfiles(git_dir: &Path) -> Vec<String> {
    let mut out = Vec::new();
    fn walk(dir: &Path, out: &mut Vec<String>) {
        for e in std::fs::read_dir(dir).expect("read_dir") {
            let p = e.expect("entry").path();
            let name = p.file_name().expect("name").to_string_lossy().into_owned();
            if name.ends_with(".lock") || name.starts_with(".tmp") {
                out.push(p.display().to_string());
            }
            if p.is_dir() {
                walk(&p, out);
            }
        }
    }
    walk(git_dir, &mut out);
    out
}

fn base(dir: &Path) {
    support::write_file(dir, "a.txt", "alpha\n");
    support::git(dir, &["add", "."]);
    support::git(dir, &["commit", "-qm", "one"]);
    support::write_file(dir, "a.txt", "alpha two\n");
    support::git(dir, &["commit", "-qam", "two"]);
}

pub struct Pair {
    pub fx: Fixture,
    pub ours: PathBuf,
    pub twin: PathBuf,
}

impl Pair {
    pub fn new(setup: &dyn Fn(&Path)) -> Self {
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

    pub async fn ours(&self, cwd: &str, argv: &[&str]) -> kaish_types::ExecResult {
        run_with(&self.fx.root(), &format!("/mnt/ours/{cwd}"), argv, &default_vars(), commit_profile()).await
    }

    pub fn twin(&self, args: &[&str]) -> (i32, String, String) {
        git_raw(&self.twin, args)
    }

    pub fn git_dir(&self, ours: bool) -> PathBuf {
        if ours { self.ours.join(".git") } else { self.twin.join(".git") }
    }

    /// Both repositories hold the same refs, HEAD and reflogs.
    pub fn assert_same(&self, label: &str) {
        let refs = |p: &Path| git_raw(p, &["for-each-ref", "--format=%(refname) %(objectname) %(objecttype)"]).1;
        assert_eq!(refs(&self.ours), refs(&self.twin), "[{label}] refs differ");
        let head = |p: &Path| std::fs::read_to_string(p.join(".git/HEAD")).expect("HEAD");
        assert_eq!(head(&self.ours), head(&self.twin), "[{label}] HEAD differs");
        assert_eq!(logs_of(&self.git_dir(true)), logs_of(&self.git_dir(false)), "[{label}] reflogs differ");
        let (code, _, err) = git_raw(&self.ours, &["fsck", "--strict"]);
        assert_eq!(code, 0, "[{label}] fsck --strict failed on ours: {err}");
        let leftovers = lockfiles(&self.ours.join(".git"));
        assert!(leftovers.is_empty(), "[{label}] stray lock or temp files: {leftovers:?}");
    }

    /// Every annotated tag object is the same bytes in both.
    pub fn assert_tag_same(&self, name: &str) {
        let cat = |p: &Path| git_raw(p, &["cat-file", "tag", name]);
        let (a, b) = (cat(&self.ours), cat(&self.twin));
        assert_eq!(b.0, 0, "git cat-file tag {name}: {}", b.2);
        assert!(a.1.as_bytes() == b.1.as_bytes(), "[{name}] tag objects differ\nours: {:?}\ntwin: {:?}", a.1, b.1);
        let oid = |p: &Path| git_raw(p, &["rev-parse", name]).1;
        assert_eq!(oid(&self.ours), oid(&self.twin), "[{name}] tag oids differ");
    }
}

// ── branch ─────────────────────────────────────────────────────────────────

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn branch_without_a_start_matches_git_and_names_the_current_branch_in_the_reflog() {
    let p = Pair::new(&base);
    let r = p.ours("", &["branch", "topic"]).await;
    assert_eq!(r.code, 0, "{}", r.err);
    assert_eq!(p.twin(&["branch", "topic"]).0, 0);
    p.assert_same("branch");
    let log = read_log(&p.ours, "logs/refs/heads/topic");
    // The design said `Created from HEAD`; git 2.56 writes the branch's name.
    assert!(log.ends_with("\tbranch: Created from main\n"), "{log}");
    // HEAD did not move, so its log has no new line.
    assert!(!read_log(&p.ours, "logs/HEAD").contains("topic"));
    assert_eq!(r.text_out(), "");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn branch_without_a_start_on_a_detached_head_says_head() {
    let p = Pair::new(&|d| {
        base(d);
        support::git(d, &["checkout", "-q", "--detach", "HEAD~1"]);
    });
    assert_eq!(p.ours("", &["branch", "topic"]).await.code, 0);
    assert_eq!(p.twin(&["branch", "topic"]).0, 0);
    p.assert_same("detached");
    assert!(read_log(&p.ours, "logs/refs/heads/topic").ends_with("\tbranch: Created from HEAD\n"));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn branch_with_a_start_records_the_start_as_typed() {
    let p = Pair::new(&|d| {
        base(d);
        support::git(d, &["tag", "light"]);
        support::git(d, &["tag", "-a", "annot", "-m", "annotated", "HEAD~1"]);
    });
    let oid = git_raw(&p.twin, &["rev-parse", "HEAD"]).1.trim().to_string();
    let starts = ["HEAD", "HEAD~1", "main", "light", "annot", oid.as_str(), "refs/heads/main", "@", "HEAD^"];
    for (n, start) in starts.iter().enumerate() {
        let name = format!("b{n}");
        let r = p.ours("", &["branch", &name, start]).await;
        assert_eq!(r.code, 0, "[{start}] {}", r.err);
        let (code, _, err) = p.twin(&["branch", &name, start]);
        assert_eq!(code, 0, "[{start}] {err}");
    }
    p.assert_same("starts");
    let log = read_log(&p.ours, "logs/refs/heads/b0");
    assert!(log.ends_with("\tbranch: Created from HEAD\n"), "{log}");
    let log = read_log(&p.ours, "logs/refs/heads/b5");
    assert!(log.ends_with(&format!("\tbranch: Created from {oid}\n")), "{log}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn nested_branch_names_create_their_directories_and_logs() {
    let p = Pair::new(&base);
    for name in ["feat/x", "feat/y", "a/b/c/d", "deep/er/still"] {
        assert_eq!(p.ours("", &["branch", name]).await.code, 0, "{name}");
        assert_eq!(p.twin(&["branch", name]).0, 0);
    }
    p.assert_same("nested");
    assert!(p.ours.join(".git/logs/refs/heads/a/b/c/d").is_file());
}

/// Names git accepts and refuses, compared one by one. Every name here is
/// independent of the others (none is a path prefix of another).
const NAMES: &[&str] = &[
    "HEAD", "-x", "--x", "-", "a..b", "a.lock", "a@{b", "a/", "ok-1", "b/c@", "@", "a b", "a~1", "a^", "a:b", "a?b", "a*b",
    "a[b", "a\\b", ".a", "a.", "/a", "a//b", "a/.b", "a.lock/b", "", "é", "a/b.lock", "refs/heads/z", "heads/q", "FETCH_HEAD",
    "a@b", "a\tb", "x\u{7f}", "@{", "a.b", "a..", "..", ".", "ok.lockx", "A", "a\u{1}b", "m/", "@{-1}", "ok@", "ok/-x", "-ok",
    "ok-", "ok_1", "ok.1", "123", "ok/./x2", "ok/..", "ok\u{3000}x", "a.LOCK",
];

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn branch_names_are_accepted_and_refused_as_git_does() {
    let p = Pair::new(&base);
    let mut accepted = 0;
    let mut refused = 0;
    for name in NAMES {
        let (code, _, gerr) = p.twin(&["branch", "--", name]);
        let r = p.ours("", &["branch", "--", name]).await;
        assert_eq!(r.code == 0, code == 0, "[{name:?}] git said {code} ({gerr}), ours said {}: {}", r.code, r.err);
        if code == 0 {
            accepted += 1;
        } else {
            refused += 1;
            assert_eq!(r.code, 2, "[{name:?}] {}", r.err);
            assert!(r.err.contains("not a valid branch name"), "[{name:?}] {}", r.err);
            assert!(r.err.contains("Nothing was changed."), "[{name:?}] {}", r.err);
        }
    }
    // Negative controls: the table exercises both outcomes.
    assert!(accepted >= 8 && refused >= 20, "accepted {accepted}, refused {refused}");
    p.assert_same("names");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn tag_names_are_accepted_and_refused_as_git_does() {
    let p = Pair::new(&base);
    let (mut accepted, mut refused) = (0, 0);
    for name in NAMES {
        let (code, _, gerr) = p.twin(&["tag", "--", name]);
        let r = p.ours("", &["tag", "--", name]).await;
        assert_eq!(r.code == 0, code == 0, "[{name:?}] git said {code} ({gerr}), ours said {}: {}", r.code, r.err);
        if code == 0 {
            accepted += 1;
        } else {
            refused += 1;
            assert_eq!(r.code, 2, "[{name:?}] {}", r.err);
            assert!(r.err.contains("not a valid tag name"), "[{name:?}] {}", r.err);
        }
    }
    assert!(accepted >= 8 && refused >= 20, "accepted {accepted}, refused {refused}");
    p.assert_same("tag names");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_branch_that_exists_is_refused_with_git_s_words_and_changes_nothing() {
    let p = Pair::new(&|d| {
        base(d);
        support::git(d, &["branch", "taken"]);
    });
    let before = support::Fingerprint::take(&p.git_dir(true));
    let r = p.ours("", &["branch", "taken"]).await;
    assert_eq!(r.code, 1, "{}", r.err);
    assert!(r.err.contains("A branch named 'taken' already exists."), "{}", r.err);
    assert!(r.err.contains("Nothing was changed."), "{}", r.err);
    let (code, _, gerr) = p.twin(&["branch", "taken"]);
    assert_ne!(code, 0);
    assert!(gerr.contains("a branch named 'taken' already exists"), "{gerr}");
    assert_eq!(support::Fingerprint::take(&p.git_dir(true)).differences_from(&before), Vec::<String>::new());
    // The branch the current HEAD names is a branch that exists too.
    assert_eq!(p.ours("", &["branch", "main"]).await.code, 1);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_branch_that_exists_only_in_packed_refs_is_refused() {
    let p = Pair::new(&|d| {
        base(d);
        support::git(d, &["branch", "packed"]);
        support::git(d, &["tag", "ptag"]);
        support::git(d, &["pack-refs", "--all", "--prune"]);
    });
    assert!(!p.ours.join(".git/refs/heads/packed").exists(), "the fixture must hold the branch in packed-refs only");
    let before = support::Fingerprint::take(&p.git_dir(true));
    for argv in [vec!["branch", "packed"], vec!["tag", "ptag"], vec!["branch", "main"], vec!["switch", "-c", "packed"], vec!["checkout", "-b", "main"]] {
        let r = p.ours("", &argv).await;
        assert_eq!(r.code, 1, "{argv:?}: {}", r.err);
        assert!(r.err.contains("already exists"), "{argv:?}: {}", r.err);
    }
    assert_eq!(support::Fingerprint::take(&p.git_dir(true)).differences_from(&before), Vec::<String>::new());
    // A new name beside packed refs works and matches git.
    assert_eq!(p.ours("", &["branch", "fresh"]).await.code, 0);
    assert_eq!(p.twin(&["branch", "fresh"]).0, 0);
    p.assert_same("beside packed refs");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_name_that_is_a_file_or_a_directory_of_branches_is_refused_like_git() {
    for (existing, wanted, what) in [("a", "a/b", "file"), ("c/d", "c", "directory"), ("x/y/z", "x/y", "directory"), ("p", "p/q/r", "file")] {
        let p = Pair::new(&|d| {
            base(d);
            support::git(d, &["branch", existing]);
        });
        let before = support::Fingerprint::take(&p.git_dir(true));
        let r = p.ours("", &["branch", wanted]).await;
        assert_eq!(r.code, 1, "[{what}] {}", r.err);
        assert!(r.err.contains(&format!("'refs/heads/{existing}' exists; cannot create 'refs/heads/{wanted}'")), "[{what}] {}", r.err);
        assert!(r.err.contains("Nothing was changed."), "{}", r.err);
        let (code, _, gerr) = p.twin(&["branch", wanted]);
        assert_ne!(code, 0);
        assert!(gerr.contains(&format!("'refs/heads/{existing}' exists; cannot create 'refs/heads/{wanted}'")), "{gerr}");
        assert_eq!(support::Fingerprint::take(&p.git_dir(true)).differences_from(&before), Vec::<String>::new());
        // Tags collide the same way.
        let r = p.ours("", &["tag", "t/1"]).await;
        assert_eq!(r.code, 0, "{}", r.err);
        let r = p.ours("", &["tag", "t"]).await;
        assert_eq!(r.code, 1, "{}", r.err);
        assert!(r.err.contains("'refs/tags/t/1' exists; cannot create 'refs/tags/t'"), "{}", r.err);
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_ref_in_another_namespace_with_a_similar_name_does_not_block_the_branch() {
    // `try_find` expands a name through the tag and remote prefixes; the
    // answer must be for `refs/heads/x` itself.
    let p = Pair::new(&|d| {
        base(d);
        support::git(d, &["tag", "x"]);
        support::git(d, &["update-ref", "refs/tags/refs/heads/y", "HEAD"]);
    });
    assert_eq!(p.ours("", &["branch", "x"]).await.code, 0);
    assert_eq!(p.twin(&["branch", "x"]).0, 0);
    assert_eq!(p.ours("", &["branch", "y"]).await.code, 0);
    assert_eq!(p.twin(&["branch", "y"]).0, 0);
    p.assert_same("similar names");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_start_that_does_not_name_a_commit_refuses_and_changes_nothing() {
    let p = Pair::new(&base);
    let before = support::Fingerprint::take(&p.git_dir(true));
    for argv in [vec!["branch", "x", "nope"], vec!["branch", "x", "main:a.txt"], vec!["branch", "x", "HEAD^{tree}"], vec!["branch", "x", "a", "b"]] {
        let r = p.ours("", &argv).await;
        assert_ne!(r.code, 0, "{argv:?}");
    }
    assert_eq!(support::Fingerprint::take(&p.git_dir(true)).differences_from(&before), Vec::<String>::new());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_unborn_head_has_nothing_to_start_a_branch_from() {
    let p = Pair::new(&|_| {});
    let before = support::Fingerprint::take(&p.git_dir(true));
    for argv in [vec!["branch", "x"], vec!["tag", "x"], vec!["tag", "-a", "x", "-m", "m"]] {
        let r = p.ours("", &argv).await;
        assert_eq!(r.code, 1, "{argv:?}: {}", r.err);
        assert!(r.err.contains("has no commits yet") || r.err.contains("nothing to tag"), "{argv:?}: {}", r.err);
        assert!(r.err.contains("Nothing was changed."), "{}", r.err);
        assert_ne!(p.twin(&argv).0, 0, "git refuses {argv:?} too");
    }
    assert_eq!(support::Fingerprint::take(&p.git_dir(true)).differences_from(&before), Vec::<String>::new());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_linked_worktree_creates_in_the_common_dir_and_logs_head_in_its_own() {
    let p = Pair::new(&base);
    for repo in [&p.ours, &p.twin] {
        let wt = repo.parent().expect("parent").join(format!("{}-wt", repo.file_name().expect("name").to_string_lossy()));
        support::git(repo, &["worktree", "add", "-q", "-b", "side", wt.to_str().expect("utf8")]);
    }
    let twin_wt = p.twin.parent().expect("parent").join("twin-wt");
    // branch: refs and the branch log in the common dir.
    assert_eq!(p.ours("../ours-wt", &["branch", "from-wt"]).await.code, 0);
    assert_eq!(git_raw(&twin_wt, &["branch", "from-wt"]).0, 0);
    // switch -c: the worktree's HEAD and its logs/HEAD.
    let r = p.ours("../ours-wt", &["switch", "-c", "moved"]).await;
    assert_eq!(r.code, 0, "{}", r.err);
    assert_eq!(git_raw(&twin_wt, &["switch", "-c", "moved"]).0, 0);
    // checkout -b, a nested name.
    let r = p.ours("../ours-wt", &["checkout", "-b", "n/m"]).await;
    assert_eq!(r.code, 0, "{}", r.err);
    assert_eq!(git_raw(&twin_wt, &["checkout", "-q", "-b", "n/m"]).0, 0);
    // tag from the worktree.
    assert_eq!(p.ours("../ours-wt", &["tag", "-a", "wt-tag", "-m", "from the worktree"]).await.code, 0);
    assert_eq!(git_raw(&twin_wt, &["tag", "-a", "wt-tag", "-m", "from the worktree"]).0, 0);

    p.assert_same("linked worktree");
    p.assert_tag_same("wt-tag");
    let first_dir = |repo: &Path| std::fs::read_dir(repo.join(".git/worktrees")).expect("worktrees").next().expect("one").expect("entry").path();
    let log = std::fs::read_to_string(first_dir(&p.ours).join("logs/HEAD")).expect("worktree log");
    assert!(log.contains("\tcheckout: moving from side to moved\n"), "{log}");
    assert!(log.contains("\tcheckout: moving from moved to n/m\n"), "{log}");
    assert_eq!(std::fs::read_to_string(first_dir(&p.ours).join("HEAD")).expect("HEAD"), "ref: refs/heads/n/m\n");
    // The worktree's own logs/HEAD, byte for byte against git's, which
    // `assert_same` (the common directory's logs) does not see.
    let own = |repo: &Path| std::fs::read(first_dir(repo).join("logs/HEAD")).expect("worktree logs/HEAD");
    assert!(own(&p.ours) == own(&p.twin), "worktree logs/HEAD differs\nours: {:?}\ntwin: {:?}", String::from_utf8_lossy(&own(&p.ours)), String::from_utf8_lossy(&own(&p.twin)));
    assert!(!own(&p.ours).is_empty());
    // The main worktree's HEAD is untouched.
    assert_eq!(std::fs::read_to_string(p.ours.join(".git/HEAD")).expect("HEAD"), "ref: refs/heads/main\n");
    assert!(p.ours.join(".git/logs/refs/heads/n/m").is_file(), "branch logs live in the common dir");
}

/// Git removes an empty directory in the way of a ref and creates the ref.
/// The containment walk refuses it (exit 4, nothing changed) because it wants a
/// regular file or nothing at that path. Safe, and different: docs/issues.md W36.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_empty_directory_where_a_branch_goes_is_refused_though_git_replaces_it() {
    let p = Pair::new(&base);
    for repo in [&p.ours, &p.twin] {
        std::fs::create_dir_all(repo.join(".git/refs/heads/emptydir/sub")).expect("dir");
    }
    let before = support::Fingerprint::take(&p.git_dir(true));
    let r = p.ours("", &["branch", "emptydir"]).await;
    assert_eq!(r.code, 4, "{}", r.err);
    assert!(r.err.contains("refs/heads/emptydir"), "{}", r.err);
    assert_eq!(support::Fingerprint::take(&p.git_dir(true)).differences_from(&before), Vec::<String>::new());
    assert_eq!(p.twin(&["branch", "emptydir"]).0, 0, "git replaces the empty directory");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_bare_repository_takes_tags_and_takes_branches_only_with_reflogs_on() {
    let p = Pair::new(&base);
    for (from, to) in [(&p.ours, "ours-bare"), (&p.twin, "twin-bare")] {
        let dest = p.fx.path(to);
        support::git(from, &["clone", "-q", "--bare", ".", dest.to_str().expect("utf8")]);
    }
    let (ours_bare, twin_bare) = (p.fx.path("ours-bare"), p.fx.path("twin-bare"));
    assert_eq!(p.ours("../ours-bare", &["tag", "-a", "tt", "-m", "m"]).await.code, 0);
    assert_eq!(git_raw(&twin_bare, &["tag", "-a", "tt", "-m", "m"]).0, 0);
    // Git writes no reflog for a branch in a bare repository by default; this
    // shell always writes one, so it refuses rather than differ.
    let before = support::Fingerprint::take(&ours_bare);
    let r = p.ours("../ours-bare", &["branch", "bb"]).await;
    assert_eq!(r.code, 4, "{}", r.err);
    assert!(r.err.contains("bare repository") && r.err.contains("core.logAllRefUpdates"), "{}", r.err);
    assert_eq!(support::Fingerprint::take(&ours_bare).differences_from(&before), Vec::<String>::new());
    assert_eq!(git_raw(&twin_bare, &["branch", "bb"]).0, 0);
    assert!(!twin_bare.join("logs/refs/heads/bb").exists(), "the premise: git wrote no log");
    // With reflogs on, both write the same.
    for d in [&ours_bare, &twin_bare] {
        assert_eq!(git_raw(d, &["config", "core.logAllRefUpdates", "true"]).0, 0);
    }
    assert_eq!(p.ours("../ours-bare", &["branch", "bc"]).await.code, 0);
    assert_eq!(git_raw(&twin_bare, &["branch", "bc"]).0, 0);
    // The twin still holds `bb`, which ours refused.
    let refs = |d: &Path| {
        let all = git_raw(d, &["for-each-ref", "--format=%(refname) %(objectname)"]).1;
        all.lines().filter(|l| !l.starts_with("refs/heads/bb ")).collect::<Vec<_>>().join("\n")
    };
    assert_eq!(refs(&ours_bare), refs(&twin_bare));
    assert_eq!(logs_of(&ours_bare).get("logs/refs/heads/bc"), logs_of(&twin_bare).get("logs/refs/heads/bc"));
    assert!(logs_of(&ours_bare).contains_key("logs/refs/heads/bc"));
    assert_eq!(git_raw(&ours_bare, &["fsck", "--strict"]).0, 0);
    // A switch needs a working tree.
    let r = p.ours("../ours-bare", &["switch", "-c", "x"]).await;
    assert_ne!(r.code, 0, "{}", r.err);
    assert_ne!(git_raw(&twin_bare, &["switch", "-c", "x"]).0, 0);
}

// ── tag ────────────────────────────────────────────────────────────────────

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_lightweight_tag_is_a_ref_with_no_reflog_and_matches_git() {
    let p = Pair::new(&base);
    for argv in [vec!["tag", "v1"], vec!["tag", "v0", "HEAD~1"], vec!["tag", "rel/1.0", "main"]] {
        let r = p.ours("", &argv).await;
        assert_eq!(r.code, 0, "{argv:?}: {}", r.err);
        assert_eq!(r.text_out(), "");
        assert_eq!(p.twin(&argv).0, 0);
    }
    p.assert_same("lightweight");
    assert!(!p.ours.join(".git/logs/refs/tags").exists(), "git writes no reflog for a tag");
    assert_eq!(git_raw(&p.ours, &["cat-file", "-t", "v1"]).1.trim(), "commit");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_annotated_tag_is_byte_identical_to_gits() {
    let p = Pair::new(&|d| {
        base(d);
        support::git(d, &["tag", "base"]);
    });
    for argv in [
        vec!["tag", "-a", "v1", "-m", "release one"],
        vec!["tag", "-a", "v0", "-m", "older", "HEAD~1"],
        vec!["tag", "-m", "only m", "implied"],
        vec!["tag", "-a", "multi", "-m", "para one", "-m", "para two"],
        vec!["tag", "-am", "joined", "short"],
        vec!["tag", "-a", "nested", "-m", "a tag of a tag", "base"],
        vec!["tag", "-a", "of-tree", "-m", "a tree", "HEAD^{tree}"],
    ] {
        // `HEAD^{tree}` is outside the revision grammar here; git takes it.
        if argv.contains(&"HEAD^{tree}") {
            continue;
        }
        let r = p.ours("", &argv).await;
        assert_eq!(r.code, 0, "{argv:?}: {}", r.err);
        let (code, _, err) = p.twin(&argv);
        assert_eq!(code, 0, "{argv:?}: {err}");
    }
    p.assert_same("annotated");
    for name in ["v1", "v0", "implied", "multi", "short", "nested"] {
        p.assert_tag_same(name);
    }
    // `-m` without `-a` still makes a tag object, as git does.
    assert_eq!(git_raw(&p.ours, &["cat-file", "-t", "implied"]).1.trim(), "tag");
    let obj = git_raw(&p.ours, &["cat-file", "-p", "v1"]).1;
    assert!(obj.contains("tagger Fixture Committer <committer@example.invalid> 1785578400 +0000\n\nrelease one\n"), "{obj}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_tag_of_a_tree_or_a_blob_is_made_as_git_makes_it() {
    let p = Pair::new(&|d| {
        base(d);
        let tree = support::git(d, &["rev-parse", "HEAD^{tree}"]);
        let blob = support::git(d, &["rev-parse", "HEAD:a.txt"]);
        support::git(d, &["update-ref", "refs/keep/tree", &tree]);
        support::git(d, &["update-ref", "refs/keep/blob", &blob]);
    });
    let tree = git_raw(&p.twin, &["rev-parse", "HEAD^{tree}"]).1.trim().to_string();
    let blob = git_raw(&p.twin, &["rev-parse", "HEAD:a.txt"]).1.trim().to_string();
    for (name, obj) in [("t-tree", &tree), ("t-blob", &blob)] {
        let a = p.ours("", &["tag", "-a", name, "-m", "m", obj]).await;
        assert_eq!(a.code, 0, "{}", a.err);
        assert_eq!(p.twin(&["tag", "-a", name, "-m", "m", obj]).0, 0);
        p.assert_tag_same(name);
        let l = p.ours("", &["tag", &format!("{name}-l"), obj]).await;
        assert_eq!(l.code, 0, "{}", l.err);
        assert_eq!(p.twin(&["tag", &format!("{name}-l"), obj]).0, 0);
    }
    p.assert_same("tree and blob tags");
}

/// Messages through `-m`, compared with git's cleanup of a tag message.
const MESSAGES: &[&str] = &[
    "subject",
    "  lead\n\n\n\nbody  \n",
    "\n\nx\n\n",
    "x\n# comment\ny",
    "# only a comment",
    "a\r\nb\r\n",
    "tab\there\t\n",
    "",
    "   ",
    "para\n\n\n\n\nafter",
    "\u{b}vt\u{b}",
    "x\u{c}\n",
    "#a\nb\n#c",
    " # indented comment",
    "x\n\n# gone\n\ny",
    "no newline at all",
    "\n",
    "trailing   \nspaces   ",
    "é unicode ✓",
];

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn tag_messages_are_cleaned_as_git_cleans_them() {
    let p = Pair::new(&base);
    for (n, msg) in MESSAGES.iter().enumerate() {
        let name = format!("m{n}");
        let r = p.ours("", &["tag", "-a", &name, "-m", msg]).await;
        let (code, _, err) = p.twin(&["tag", "-a", &name, "-m", msg]);
        assert_eq!(r.code == 0, code == 0, "[{msg:?}] git {code} {err}; ours {} {}", r.code, r.err);
        if code == 0 {
            p.assert_tag_same(&name);
        }
    }
    p.assert_same("messages");
    // Negative control: the comment line really goes, so the table can fail.
    let obj = git_raw(&p.ours, &["cat-file", "-p", "m3"]).1;
    assert!(obj.ends_with("\n\nx\ny\n"), "{obj:?}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn tag_f_reads_the_message_from_the_vfs_and_cleans_it_the_same() {
    use kaish_kernel::{Kernel, KernelConfig};
    let p = Pair::new(&base);
    let backend: Arc<dyn kaish_tool_api::KernelBackend> = Arc::new(StrictBackend::single(PathBuf::from("/mnt"), p.fx.root()));
    let git = kaish_tools_git::tool(commit_profile()).expect("config");
    let mut cfg = KernelConfig::transient()
        .with_var("GIT_AUTHOR_NAME", Value::String("Fixture Author".into()))
        .with_var("GIT_AUTHOR_EMAIL", Value::String("author@example.invalid".into()))
        .with_var("GIT_COMMITTER_NAME", Value::String("Fixture Committer".into()))
        .with_var("GIT_COMMITTER_EMAIL", Value::String("committer@example.invalid".into()))
        .with_var("GIT_AUTHOR_DATE", Value::String(DATE.into()))
        .with_var("GIT_COMMITTER_DATE", Value::String(DATE.into()));
    cfg.cwd = PathBuf::from("/mnt/ours");
    let kernel = Kernel::with_backend(backend, cfg, |_| {}, |tools| tools.register(git)).expect("kernel");
    // printf escapes: a message with a comment line, trailing spaces, a run of
    // blank lines, no final newline, and an empty file.
    let files: &[(&str, &str)] = &[
        ("subject", "subject\\n"),
        ("body", "  lead\\n\\n\\n\\nbody  \\n# gone\\n\\n"),
        ("noeol", "no newline"),
        ("crlf", "a\\r\\nb\\r\\n"),
        ("empty", ""),
    ];
    for (name, escaped) in files {
        let w = kernel.execute(&format!("printf '{escaped}' > /v/blobs/{name}.txt")).await.expect("write");
        assert_eq!(w.code, 0, "{}", w.err);
        let r = kernel.execute(&format!("git tag -a f-{name} -F /v/blobs/{name}.txt")).await.expect("tag");
        assert_eq!(r.code, 0, "[{name}] {}", r.err);
    }
    let want: &[(&str, &[u8])] = &[
        ("subject", b"subject\n"),
        ("body", b"  lead\n\n\n\nbody  \n# gone\n\n"),
        ("noeol", b"no newline"),
        ("crlf", b"a\r\nb\r\n"),
        ("empty", b""),
    ];
    for (name, bytes) in want {
        std::fs::write(p.fx.path("twin-msg.txt"), bytes).expect("msg");
        let (code, _, err) = p.twin(&["tag", "-a", &format!("f-{name}"), "-F", p.fx.path("twin-msg.txt").to_str().expect("utf8")]);
        assert_eq!(code, 0, "[{name}] {err}");
        p.assert_tag_same(&format!("f-{name}"));
    }
    p.assert_same("-F");
    let r = kernel.execute("git tag -a nofile -F /v/blobs/missing.txt").await.expect("tag");
    assert_eq!(r.code, 2, "{}", r.err);
    assert!(r.err.contains("git tag -F") && r.err.contains("could not be read"), "{}", r.err);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_tag_that_exists_is_refused_and_changes_nothing() {
    let p = Pair::new(&|d| {
        base(d);
        support::git(d, &["tag", "taken"]);
        support::git(d, &["tag", "-a", "taken-a", "-m", "m"]);
    });
    let before = support::Fingerprint::take(&p.git_dir(true));
    for argv in [vec!["tag", "taken"], vec!["tag", "-a", "taken-a", "-m", "other"], vec!["tag", "-m", "x", "taken"]] {
        let r = p.ours("", &argv).await;
        assert_eq!(r.code, 1, "{argv:?}: {}", r.err);
        assert!(r.err.contains("already exists"), "{}", r.err);
        assert!(r.err.contains("Nothing was changed."), "{}", r.err);
        assert!(p.twin(&argv).2.contains("already exists"));
    }
    // The refusal came before the tag object was written.
    assert_eq!(support::Fingerprint::take(&p.git_dir(true)).differences_from(&before), Vec::<String>::new());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn tag_identity_is_the_committer_and_a_missing_one_refuses_before_any_object() {
    let p = Pair::new(&base);
    let before = support::Fingerprint::take(&p.git_dir(true));
    let r = run_with(&p.fx.root(), "/mnt/ours", &["tag", "-a", "v1", "-m", "m"], &[], commit_profile()).await;
    assert_eq!(r.code, 2, "{}", r.err);
    assert!(r.err.contains("GIT_COMMITTER_NAME"), "{}", r.err);
    assert_eq!(support::Fingerprint::take(&p.git_dir(true)).differences_from(&before), Vec::<String>::new());
    // A lightweight tag needs no identity: git writes none.
    let r = run_with(&p.fx.root(), "/mnt/ours", &["tag", "light"], &[], commit_profile()).await;
    assert_eq!(r.code, 0, "{}", r.err);
    // A pinned identity wins over the variables.
    let a = kaish_tools_git::Identity::new("Pinned", "pin@example.org").expect("identity");
    let config = commit_profile().with_identity(a.clone(), a);
    let r = run_with(&p.fx.root(), "/mnt/ours", &["tag", "-a", "pinned", "-m", "m"], &default_vars(), config).await;
    assert_eq!(r.code, 0, "{}", r.err);
    let obj = git_raw(&p.ours, &["cat-file", "-p", "pinned"]).1;
    assert!(obj.contains("tagger Pinned <pin@example.org> "), "{obj}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn signing_settings_and_always_logging_refuse_a_tag() {
    for (key, value, argv) in [
        ("tag.gpgSign", "true", vec!["tag", "x"]),
        ("tag.gpgSign", "true", vec!["tag", "-a", "x", "-m", "m"]),
        ("tag.forceSignAnnotated", "true", vec!["tag", "-a", "x", "-m", "m"]),
        ("core.logAllRefUpdates", "always", vec!["tag", "x"]),
        ("core.commentChar", ";", vec!["tag", "-a", "x", "-m", "m"]),
    ] {
        let p = Pair::new(&|d| {
            base(d);
            support::git(d, &["config", key, value]);
        });
        let before = support::Fingerprint::take(&p.git_dir(true));
        let r = p.ours("", &argv).await;
        assert_eq!(r.code, 4, "[{key}] {}", r.err);
        assert!(r.err.contains(key.split('.').next_back().map(|k| k.to_lowercase()).as_deref().map_or("", |_| key)), "[{key}] {}", r.err);
        assert!(r.err.contains("Nothing was changed."), "{}", r.err);
        assert_eq!(support::Fingerprint::take(&p.git_dir(true)).differences_from(&before), Vec::<String>::new());
    }
    // Negative control: forceSignAnnotated does not touch a lightweight tag.
    let p = Pair::new(&|d| {
        base(d);
        support::git(d, &["config", "tag.forceSignAnnotated", "true"]);
    });
    assert_eq!(p.ours("", &["tag", "light"]).await.code, 0);
}

// ── switch -c and checkout -b ──────────────────────────────────────────────

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn switch_c_points_head_at_the_new_branch_and_logs_like_git() {
    for verb in [("switch", "-c"), ("checkout", "-b")] {
        let p = Pair::new(&base);
        let index_before = std::fs::read(p.ours.join(".git/index")).expect("index");
        let r = p.ours("", &[verb.0, verb.1, "topic"]).await;
        assert_eq!(r.code, 0, "{}", r.err);
        assert_eq!(r.err, "Switched to a new branch 'topic'\n");
        assert_eq!(r.text_out(), "");
        let (code, _, gerr) = p.twin(&[verb.0, verb.1, "topic"]);
        assert_eq!(code, 0, "{gerr}");
        p.assert_same(verb.0);
        assert_eq!(std::fs::read_to_string(p.ours.join(".git/HEAD")).expect("HEAD"), "ref: refs/heads/topic\n");
        let head_log = read_log(&p.ours, "logs/HEAD");
        assert!(head_log.ends_with("\tcheckout: moving from main to topic\n"), "{head_log}");
        let blog = read_log(&p.ours, "logs/refs/heads/topic");
        assert!(blog.ends_with("\tbranch: Created from HEAD\n"), "{blog}");
        // The index is byte-for-byte what it was, and the tree is clean.
        assert_eq!(std::fs::read(p.ours.join(".git/index")).expect("index"), index_before);
        assert_eq!(git_raw(&p.ours, &["status", "--porcelain=v2"]).1, git_raw(&p.twin, &["status", "--porcelain=v2"]).1);
        // And a second switch -c from the new branch records the old name.
        assert_eq!(p.ours("", &[verb.0, verb.1, "second"]).await.code, 0);
        assert_eq!(p.twin(&[verb.0, verb.1, "second"]).0, 0);
        p.assert_same("second switch");
        assert!(read_log(&p.ours, "logs/HEAD").ends_with("\tcheckout: moving from topic to second\n"));
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn switch_c_keeps_local_edits_and_staged_changes_exactly() {
    let p = Pair::new(&|d| {
        base(d);
        support::write_file(d, "a.txt", "dirty\n");
        support::write_file(d, "staged.txt", "staged\n");
        support::git(d, &["add", "staged.txt"]);
        support::write_file(d, "untracked.txt", "u\n");
    });
    let index_before = std::fs::read(p.ours.join(".git/index")).expect("index");
    assert_eq!(p.ours("", &["switch", "-c", "wip"]).await.code, 0);
    assert_eq!(p.twin(&["switch", "-c", "wip"]).0, 0);
    p.assert_same("dirty");
    assert_eq!(std::fs::read_to_string(p.ours.join("a.txt")).expect("a"), "dirty\n");
    assert_eq!(std::fs::read(p.ours.join(".git/index")).expect("index"), index_before);
    assert_eq!(git_raw(&p.ours, &["status", "--porcelain=v2", "-uall"]).1, git_raw(&p.twin, &["status", "--porcelain=v2", "-uall"]).1);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_detached_head_is_attached_to_the_new_branch_and_logs_the_full_oid() {
    let p = Pair::new(&|d| {
        base(d);
        support::git(d, &["checkout", "-q", "--detach", "HEAD~1"]);
    });
    let oid = git_raw(&p.ours, &["rev-parse", "HEAD"]).1.trim().to_string();
    assert_eq!(p.ours("", &["switch", "-c", "rescued"]).await.code, 0);
    assert_eq!(p.twin(&["switch", "-c", "rescued"]).0, 0);
    p.assert_same("detached");
    assert!(read_log(&p.ours, "logs/HEAD").ends_with(&format!("\tcheckout: moving from {oid} to rescued\n")));
    assert_eq!(std::fs::read_to_string(p.ours.join(".git/HEAD")).expect("HEAD"), "ref: refs/heads/rescued\n");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_start_point_on_the_commit_head_is_on_is_allowed_and_one_elsewhere_refuses() {
    let p = Pair::new(&|d| {
        base(d);
        support::git(d, &["branch", "same"]);
        support::git(d, &["tag", "same-tag"]);
        support::git(d, &["tag", "-a", "same-annot", "-m", "m"]);
        support::git(d, &["branch", "old", "HEAD~1"]);
    });
    for (verb, flag, name, start) in [
        ("switch", "-c", "s1", "same"),
        ("checkout", "-b", "s2", "same-tag"),
        ("switch", "-c", "s3", "same-annot"),
        ("switch", "-c", "s4", "HEAD"),
        ("checkout", "-b", "s5", "main"),
    ] {
        // Each starts from main again.
        assert_eq!(p.ours("", &["switch", "-c", &format!("back-{name}")]).await.code, 0);
        assert_eq!(p.twin(&["switch", "-c", &format!("back-{name}")]).0, 0);
        let r = p.ours("", &[verb, flag, name, start]).await;
        assert_eq!(r.code, 0, "{verb} {start}: {}", r.err);
        assert_eq!(p.twin(&[verb, flag, name, start]).0, 0);
        assert!(read_log(&p.ours, &format!("logs/refs/heads/{name}")).ends_with(&format!("\tbranch: Created from {start}\n")));
    }
    p.assert_same("same-commit starts");

    let before = support::Fingerprint::take(&p.git_dir(true));
    for argv in [vec!["switch", "-c", "z", "old"], vec!["checkout", "-b", "z", "HEAD~1"]] {
        let r = p.ours("", &argv).await;
        assert_eq!(r.code, 2, "{argv:?}: {}", r.err);
        assert!(r.err.contains("git branch z "), "the refusal names the alternative: {}", r.err);
        assert!(r.err.contains("Nothing was changed."), "{}", r.err);
    }
    assert_eq!(support::Fingerprint::take(&p.git_dir(true)).differences_from(&before), Vec::<String>::new());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn on_an_unborn_branch_head_just_names_the_new_one_and_nothing_is_logged() {
    let p = Pair::new(&|_| {});
    for verb in [("switch", "-c"), ("checkout", "-b")] {
        assert_eq!(p.ours("", &[verb.0, verb.1, "first"]).await.code, 0);
        assert_eq!(p.twin(&[verb.0, verb.1, "first"]).0, 0);
        p.assert_same(verb.0);
        assert_eq!(std::fs::read_to_string(p.ours.join(".git/HEAD")).expect("HEAD"), "ref: refs/heads/first\n");
        assert!(!p.ours.join(".git/logs").exists());
        assert!(!p.ours.join(".git/refs/heads/first").exists(), "no branch is made without a commit");
        // Put HEAD back for the second spelling.
        for repo in [&p.ours, &p.twin] {
            std::fs::write(repo.join(".git/HEAD"), "ref: refs/heads/main\n").expect("HEAD");
        }
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_new_branch_name_that_exists_is_refused_by_switch_and_checkout() {
    let p = Pair::new(&|d| {
        base(d);
        support::git(d, &["branch", "taken"]);
    });
    let before = support::Fingerprint::take(&p.git_dir(true));
    for argv in [vec!["switch", "-c", "taken"], vec!["checkout", "-b", "taken"], vec!["switch", "-c", "main"], vec!["switch", "-c", "a..b"], vec!["switch", "-c", "HEAD"]] {
        let r = p.ours("", &argv).await;
        assert!(r.code == 1 || r.code == 2, "{argv:?}: {} {}", r.code, r.err);
        assert!(r.err.contains("Nothing was changed."), "{argv:?}: {}", r.err);
        assert_ne!(p.twin(&argv).0, 0);
    }
    assert_eq!(support::Fingerprint::take(&p.git_dir(true)).differences_from(&before), Vec::<String>::new());
}

// ── refusals ───────────────────────────────────────────────────────────────

/// Every refusal that claims "Nothing was changed." leaves `.git` byte
/// identical, directory mtimes included.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn every_nothing_was_changed_refusal_leaves_git_byte_identical() {
    let p = Pair::new(&|d| {
        base(d);
        support::git(d, &["branch", "taken"]);
        support::git(d, &["tag", "taken-tag"]);
    });
    let table: &[(&[&str], i64)] = &[
        (&["branch", "-d", "taken"], 2),
        (&["branch", "-D", "taken"], 2),
        (&["branch", "--delete", "taken"], 2),
        (&["branch", "-m", "taken", "new"], 2),
        (&["branch", "-M", "taken", "new"], 2),
        (&["branch", "--move", "new"], 2),
        (&["branch", "-c", "taken", "new"], 2),
        (&["branch", "-C", "taken", "new"], 2),
        (&["branch", "-f", "taken"], 2),
        (&["branch", "--force", "taken", "HEAD~1"], 2),
        (&["branch", "-u", "origin/main"], 2),
        (&["branch", "--set-upstream-to=origin/main"], 2),
        (&["branch", "--set-upstream-to", "origin/main", "taken"], 2),
        (&["branch", "--unset-upstream"], 2),
        (&["branch", "--track", "x", "main"], 2),
        (&["branch", "-t", "x"], 2),
        (&["branch", "taken"], 1),
        (&["branch", "a..b"], 2),
        (&["branch", "HEAD"], 2),
        (&["branch", "x", "a", "b"], 2),
        (&["branch", "-a", "x"], 2),
        (&["branch", "--show-current", "x"], 2),
        (&["branch", "--contains", "HEAD", "x"], 2),
        (&["tag", "-d", "taken-tag"], 2),
        (&["tag", "--delete", "taken-tag"], 2),
        (&["tag", "-f", "taken-tag"], 2),
        (&["tag", "--force", "-a", "taken-tag", "-m", "m"], 2),
        (&["tag", "-s", "x", "-m", "m"], 2),
        (&["tag", "--sign", "x"], 2),
        (&["tag", "-u", "KEY", "x", "-m", "m"], 2),
        (&["tag", "-a", "x"], 2),
        (&["tag", "-a", "x", "-m", "m", "-F", "/mnt/none"], 2),
        (&["tag", "taken-tag"], 1),
        (&["tag", "a b"], 2),
        (&["tag"], 0),
        (&["tag", "x", "a", "b"], 2),
        (&["tag", "--contains", "HEAD", "x"], 2),
        (&["switch", "taken"], 2),
        (&["switch", "-"], 2),
        (&["switch"], 2),
        (&["switch", "--detach", "HEAD~1"], 2),
        (&["switch", "-d"], 2),
        (&["switch", "-C", "taken"], 2),
        (&["switch", "--force-create", "taken"], 2),
        (&["switch", "--orphan", "x"], 2),
        (&["switch", "-f", "taken"], 2),
        (&["switch", "--discard-changes", "taken"], 2),
        (&["switch", "-m", "taken"], 2),
        (&["switch", "-c", "x", "--track"], 2),
        (&["switch", "-t", "-c", "x"], 2),
        (&["switch", "-c", "x", "main", "extra"], 2),
        (&["checkout", "taken"], 2),
        (&["checkout", "-"], 2),
        (&["checkout"], 2),
        (&["checkout", "--", "a.txt"], 2),
        (&["checkout", "a.txt"], 2),
        (&["checkout", "HEAD", "a.txt"], 2),
        (&["checkout", "-B", "taken"], 2),
        (&["checkout", "-f", "-b", "x"], 2),
        (&["checkout", "-p"], 2),
        (&["checkout", "--detach"], 2),
        (&["checkout", "-b", "x", "main", "--", "a.txt"], 2),
        (&["checkout", "--orphan", "x"], 2),
    ];
    let before = support::Fingerprint::take(&p.git_dir(true));
    let mut refusals = 0;
    for (argv, code) in table {
        let r = p.ours("", argv).await;
        if *code == 0 {
            // The listing is not a refusal: the control that the table can pass.
            assert_eq!(r.code, 0, "{argv:?}: {}", r.err);
            continue;
        }
        refusals += 1;
        assert_eq!(r.code, *code, "{argv:?}: {}", r.err);
        assert!(r.err.contains("Nothing was changed."), "{argv:?}: {}", r.err);
        assert!(r.err.contains(": refused."), "{argv:?}: {}", r.err);
        let diffs = support::Fingerprint::take(&p.git_dir(true)).differences_from(&before);
        assert!(diffs.is_empty(), "{argv:?} left a mark: {diffs:?}");
    }
    assert!(refusals > 50);
    // A control: the same fingerprint sees a real creation.
    assert_eq!(p.ours("", &["branch", "real"]).await.code, 0);
    assert!(!support::Fingerprint::take(&p.git_dir(true)).differences_from(&before).is_empty());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn refused_spellings_name_a_next_step_that_works() {
    let p = Pair::new(&|d| {
        base(d);
        support::git(d, &["branch", "taken"]);
    });
    // (argv, text the refusal must hold, a command that runs and exits 0)
    let table: &[(&[&str], &str, &[&str])] = &[
        (&["branch", "-d", "taken"], "git branch", &["branch"]),
        (&["branch", "-u", "origin/main"], "git branch --no-track <name> <start>", &["branch", "--no-track", "n1", "main"]),
        (&["branch", "-f", "taken"], "git branch <new-name> <start>", &["branch", "n2", "main"]),
        (&["branch", "-m", "taken"], "git branch <new-name> <old-name>", &["branch", "n3", "taken"]),
        (&["tag", "-s", "x"], "git tag -a <name> -m '<message>'", &["tag", "-a", "n4", "-m", "m"]),
        (&["tag", "-a", "x"], "git tag -a <name> -m '<message>'", &["tag", "-a", "n5", "-m", "m"]),
        (&["tag", "-f", "x"], "git tag <new-name> [<rev>]", &["tag", "n6"]),
        (&["switch", "taken"], "git switch -c <name>", &["switch", "-c", "n7"]),
        (&["checkout", "taken"], "git checkout -b <name>", &["checkout", "-b", "n8"]),
        (&["switch", "-c", "n9", "taken~1"], "git branch n9", &["branch", "n9", "taken~1"]),
    ];
    for (bad, text, good) in table {
        let r = p.ours("", bad).await;
        assert_ne!(r.code, 0, "{bad:?}");
        assert!(r.err.contains(text), "{bad:?} should hold {text:?}: {}", r.err);
        let ok = p.ours("", good).await;
        assert_eq!(ok.code, 0, "{good:?} (the next step of {bad:?}): {}", ok.err);
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_remote_tracking_start_would_write_tracking_config_and_refuses_unless_no_track() {
    let p = Pair::new(&|d| {
        base(d);
        support::git(d, &["update-ref", "refs/remotes/origin/main", "HEAD"]);
    });
    let before = support::Fingerprint::take(&p.git_dir(true));
    for argv in [vec!["branch", "x", "origin/main"], vec!["switch", "-c", "x", "origin/main"], vec!["checkout", "-b", "x", "refs/remotes/origin/main"]] {
        let r = p.ours("", &argv).await;
        assert_eq!(r.code, 4, "{argv:?}: {}", r.err);
        assert!(r.err.contains("--no-track"), "{}", r.err);
        assert!(r.err.contains("Nothing was changed."), "{}", r.err);
    }
    assert_eq!(support::Fingerprint::take(&p.git_dir(true)).differences_from(&before), Vec::<String>::new());
    // `--no-track` is what git would do with no tracking configured.
    assert_eq!(p.ours("", &["branch", "--no-track", "x", "origin/main"]).await.code, 0);
    assert_eq!(p.twin(&["branch", "--no-track", "x", "origin/main"]).0, 0);
    assert_eq!(p.ours("", &["switch", "-c", "y", "--no-track", "origin/main"]).await.code, 0);
    assert_eq!(p.twin(&["switch", "-c", "y", "--no-track", "origin/main"]).0, 0);
    p.assert_same("no-track");
    assert_eq!(std::fs::read_to_string(p.ours.join(".git/config")).expect("config"), std::fs::read_to_string(p.twin.join(".git/config")).expect("config"));
    // branch.autoSetupMerge=false: no tracking whatever the start.
    let q = Pair::new(&|d| {
        base(d);
        support::git(d, &["update-ref", "refs/remotes/origin/main", "HEAD"]);
        support::git(d, &["config", "branch.autoSetupMerge", "false"]);
    });
    assert_eq!(q.ours("", &["branch", "x", "origin/main"]).await.code, 0);
    assert_eq!(q.twin(&["branch", "x", "origin/main"]).0, 0);
    q.assert_same("autoSetupMerge=false");
    // autoSetupMerge=always tracks even a local start.
    let q = Pair::new(&|d| {
        base(d);
        support::git(d, &["config", "branch.autoSetupMerge", "always"]);
    });
    assert_eq!(q.ours("", &["branch", "x", "main"]).await.code, 4);
}

// ── listing keeps working ──────────────────────────────────────────────────

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_listing_forms_work_under_the_commit_profile_and_see_what_was_created() {
    let p = Pair::new(&|d| {
        base(d);
        support::git(d, &["branch", "feat/one"]);
    });
    assert_eq!(p.ours("", &["branch", "feat/two"]).await.code, 0);
    assert_eq!(p.ours("", &["tag", "-a", "v1", "-m", "release"]).await.code, 0);
    assert_eq!(p.ours("", &["tag", "light"]).await.code, 0);
    for argv in [vec!["branch"], vec!["branch", "-a"], vec!["branch", "-r"], vec!["branch", "--list"], vec!["branch", "--list", "feat/*"], vec!["branch", "--show-current"], vec!["branch", "--contains", "HEAD"], vec!["tag"], vec!["tag", "-l", "v*"], vec!["tag", "--list"], vec!["tag", "--contains", "HEAD"], vec!["branch", "--json"], vec!["tag", "--json"]] {
        let r = p.ours("", &argv).await;
        assert_eq!(r.code, 0, "{argv:?}: {}", r.err);
    }
    let listed = p.ours("", &["branch", "--list", "feat/*"]).await;
    let names = listed.text_out();
    assert!(names.contains("feat/one") && names.contains("feat/two"), "{names}");
    let tags = p.ours("", &["tag", "-l", "v*"]).await;
    assert!(tags.text_out().contains("v1"), "{}", tags.text_out());
    let json = p.ours("", &["tag", "--json"]).await;
    let v = json.output().and_then(|o| o.rich_json.clone()).expect("--json carries the model");
    let kinds: Vec<&str> = v["tags"].as_array().expect("tags").iter().map(|t| t["kind"].as_str().expect("kind")).collect();
    assert!(kinds.contains(&"annotated") && kinds.contains(&"lightweight"), "{v}");
}

// ── hostile repositories ───────────────────────────────────────────────────

fn plant_hook(repo: &Path, name: &str, sentinel: &Path) {
    use std::os::unix::fs::PermissionsExt;
    let path = repo.join(".git/hooks").join(name);
    std::fs::create_dir_all(path.parent().expect("hooks")).expect("hooks dir");
    std::fs::write(&path, format!("#!/bin/sh\necho ran >> '{}'\n", sentinel.display())).expect("hook");
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).expect("chmod");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_reference_transaction_hook_without_a_provider_refuses_every_ref_write_and_runs_nothing() {
    let p = Pair::new(&base);
    let sentinel = p.fx.path("sentinel");
    plant_hook(&p.ours, "reference-transaction", &sentinel);
    let before = support::Fingerprint::take(&p.git_dir(true));
    for argv in [
        vec!["branch", "x"],
        vec!["branch", "x", "HEAD~1"],
        vec!["tag", "x"],
        vec!["tag", "-a", "x", "-m", "m"],
        vec!["switch", "-c", "x"],
        vec!["checkout", "-b", "x"],
    ] {
        let r = p.ours("", &argv).await;
        assert_eq!(r.code, 4, "{argv:?}: {}", r.err);
        assert!(r.err.contains("reference-transaction hook"), "{}", r.err);
        assert!(r.err.contains("Nothing was changed."), "{}", r.err);
    }
    assert!(!sentinel.exists(), "a hook ran");
    assert_eq!(support::Fingerprint::take(&p.git_dir(true)).differences_from(&before), Vec::<String>::new());
    // Git runs that hook for the same writes: the refusal is not over-cautious.
    plant_hook(&p.twin, "reference-transaction", &p.fx.path("twin-sentinel"));
    assert_eq!(p.twin(&["branch", "x"]).0, 0);
    assert!(p.fx.path("twin-sentinel").exists());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn post_checkout_refuses_switch_only_and_other_hooks_refuse_nothing_here() {
    let p = Pair::new(&base);
    let sentinel = p.fx.path("sentinel");
    plant_hook(&p.ours, "post-checkout", &sentinel);
    for argv in [vec!["switch", "-c", "x"], vec!["checkout", "-b", "x"]] {
        let r = p.ours("", &argv).await;
        assert_eq!(r.code, 4, "{argv:?}: {}", r.err);
        assert!(r.err.contains("post-checkout hook"), "{}", r.err);
    }
    // Git does not run post-checkout for branch or tag.
    assert_eq!(p.ours("", &["branch", "y"]).await.code, 0);
    assert_eq!(p.ours("", &["tag", "-a", "t", "-m", "m"]).await.code, 0);
    assert!(!sentinel.exists());
    // A hook git does not run for these writes does not refuse them.
    plant_hook(&p.ours, "pre-commit", &sentinel);
    plant_hook(&p.ours, "commit-msg", &sentinel);
    assert_eq!(p.ours("", &["branch", "z"]).await.code, 0);
    assert_eq!(p.ours("", &["tag", "-a", "t2", "-m", "m"]).await.code, 0);
    // A sample is not a hook.
    std::fs::write(p.ours.join(".git/hooks/reference-transaction.sample"), "#!/bin/sh\n").expect("sample");
    assert_eq!(p.ours("", &["branch", "w"]).await.code, 0);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_planted_link_in_a_ref_write_set_refuses_every_creation_untouched() {
    // (what to plant, as a path under .git, and the argv that would write through it)
    let cases: &[(&str, &[&str])] = &[
        ("logs/refs/heads", &["branch", "x"]),
        ("logs/HEAD", &["switch", "-c", "x"]),
        ("logs/HEAD", &["checkout", "-b", "x"]),
        ("refs/heads/x.lock", &["branch", "x"]),
        ("refs/heads/x.lock", &["switch", "-c", "x"]),
        ("refs/tags", &["tag", "x"]),
        ("refs/tags/x.lock", &["tag", "x"]),
        ("refs/heads/feat", &["branch", "feat/x"]),
        ("logs/refs/heads/feat", &["branch", "feat/x"]),
        ("HEAD.lock", &["switch", "-c", "x"]),
    ];
    for (target, argv) in cases {
        let p = Pair::new(&base);
        let outside = p.fx.path("outside");
        std::fs::create_dir_all(&outside).expect("outside");
        let dest = p.ours.join(".git").join(target);
        std::fs::create_dir_all(dest.parent().expect("parent")).expect("parent");
        let is_dir_target = matches!(*target, "logs/refs/heads" | "refs/tags" | "refs/heads/feat" | "logs/refs/heads/feat");
        if is_dir_target {
            let _ = std::fs::remove_dir_all(&dest);
            std::os::unix::fs::symlink(&outside, &dest).expect("plant");
        } else {
            let victim = outside.join("victim");
            std::fs::write(&victim, "precious\n").expect("victim");
            let _ = std::fs::remove_file(&dest);
            std::os::unix::fs::symlink(&victim, &dest).expect("plant");
        }
        let before = support::Fingerprint::take(&p.ours.join(".git"));
        let r = p.ours("", argv).await;
        assert_eq!(r.code, 4, "[{target}] {argv:?}: {}", r.err);
        assert!(!r.err.contains(outside.to_str().expect("utf8")), "[{target}] named a path outside the mount: {}", r.err);
        let written: Vec<_> = std::fs::read_dir(&outside).expect("outside").map(|e| e.expect("e").file_name()).collect();
        let expected = if is_dir_target { 0 } else { 1 };
        assert_eq!(written.len(), expected, "[{target}] wrote through the link: {written:?}");
        if !is_dir_target {
            assert_eq!(std::fs::read_to_string(outside.join("victim")).expect("victim"), "precious\n");
        }
        let diffs = support::Fingerprint::take(&p.ours.join(".git")).differences_from(&before);
        assert!(diffs.is_empty(), "[{target}] {argv:?}: {diffs:?}");
    }

    // A hard link: `lstat` says regular file, and the append would reach the other name.
    for (target, argv) in [("logs/HEAD", vec!["switch", "-c", "x"]), ("logs/refs/heads/main", vec!["branch", "x"]), ("logs/refs/heads/x", vec!["branch", "x"])] {
        let p = Pair::new(&base);
        let victim = p.fx.path("hardlinked");
        std::fs::write(&victim, "precious\n").expect("victim");
        let dest = p.ours.join(".git").join(target);
        std::fs::create_dir_all(dest.parent().expect("parent")).expect("parent");
        let _ = std::fs::remove_file(&dest);
        std::fs::hard_link(&victim, &dest).expect("link");
        let r = p.ours("", &argv).await;
        if target == "logs/refs/heads/main" {
            // A link on a log this write does not touch is not in its way.
            assert_eq!(r.code, 0, "[{target}] {}", r.err);
            continue;
        }
        assert_eq!(r.code, 4, "[{target}] {}", r.err);
        assert_eq!(std::fs::read_to_string(&victim).expect("victim"), "precious\n", "[{target}] appended through a hard link");
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_planted_object_directory_link_stops_an_annotated_tag_before_any_write() {
    let p = Pair::new(&base);
    // The tag object's id is known from the twin, which writes the same bytes.
    assert_eq!(p.twin(&["tag", "-a", "v1", "-m", "m"]).0, 0);
    let oid = git_raw(&p.twin, &["rev-parse", "v1"]).1.trim().to_string();
    let outside = p.fx.path("outside");
    std::fs::create_dir_all(&outside).expect("outside");
    let fan = p.ours.join(".git/objects").join(&oid[..2]);
    let _ = std::fs::remove_dir_all(&fan);
    std::os::unix::fs::symlink(&outside, &fan).expect("plant");
    let before = support::Fingerprint::take(&p.ours.join(".git"));
    let r = p.ours("", &["tag", "-a", "v1", "-m", "m"]).await;
    assert_eq!(r.code, 4, "{}", r.err);
    assert_eq!(std::fs::read_dir(&outside).expect("outside").count(), 0, "wrote through the link");
    assert!(support::Fingerprint::take(&p.ours.join(".git")).differences_from(&before).is_empty());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_stale_lock_refuses_with_exit_4_and_is_left_alone() {
    for (lock, argv) in [
        ("refs/heads/x.lock", vec!["branch", "x"]),
        ("refs/tags/x.lock", vec!["tag", "x"]),
        ("refs/tags/x.lock", vec!["tag", "-a", "x", "-m", "m"]),
        ("HEAD.lock", vec!["switch", "-c", "x"]),
        ("refs/heads/x.lock", vec!["checkout", "-b", "x"]),
        ("packed-refs.lock", vec!["branch", "x"]),
    ] {
        let p = Pair::new(&base);
        let path = p.ours.join(".git").join(lock);
        std::fs::create_dir_all(path.parent().expect("parent")).expect("dir");
        std::fs::write(&path, "").expect("lock");
        let before = support::Fingerprint::take(&p.ours.join(".git"));
        let r = p.ours("", &argv).await;
        assert_eq!(r.code, 4, "[{lock}] {argv:?}: {}", r.err);
        assert!(r.err.contains(lock), "[{lock}] {}", r.err);
        assert!(path.exists(), "a lock this shell did not take was removed");
        assert!(support::Fingerprint::take(&p.ours.join(".git")).differences_from(&before).is_empty(), "[{lock}] left a mark");
    }
}

// ── the schema ─────────────────────────────────────────────────────────────

#[test]
fn the_ref_writing_leaves_declare_git_ref_and_a_read_only_tool_declares_nothing() {
    let schema = kaish_tools_git::tool(commit_profile()).expect("tool");
    let schema = schema.schema();
    let ops = |name: &str| -> Vec<String> {
        schema.subcommands.iter().find(|c| c.name == name).unwrap_or_else(|| panic!("no leaf {name}")).operations.clone()
    };
    for leaf in ["branch", "tag", "switch", "checkout"] {
        assert_eq!(ops(leaf), ["git.ref"], "{leaf}");
    }
    assert!(schema.operations.contains(&"git.ref".to_string()));
    // The listing leaves outside this phase declare nothing.
    for leaf in ["log", "status", "info"] {
        assert!(ops(leaf).is_empty(), "{leaf}");
    }
    let read = kaish_tools_git::tool(GitConfig::read_only()).expect("tool").schema();
    assert!(read.operations.is_empty());
    assert!(read.subcommands.iter().all(|c| c.operations.is_empty()));
    assert!(read.subcommands.iter().all(|c| c.name != "switch" && c.name != "checkout"));
    // The read-only tool's branch and tag publish none of the creation flags.
    for leaf in ["branch", "tag"] {
        let flags: Vec<&str> = read.subcommands.iter().find(|c| c.name == leaf).expect("leaf").params.iter().map(|p| p.name.as_str()).collect();
        assert!(!flags.contains(&"no-track") && !flags.contains(&"annotate") && !flags.contains(&"delete"), "{leaf}: {flags:?}");
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_subtracted_verb_takes_its_creation_with_it() {
    let p = Pair::new(&base);
    for (verb, argv) in [
        (kaish_tools_git::Verb::Branch, vec!["branch", "x"]),
        (kaish_tools_git::Verb::Tag, vec!["tag", "x"]),
        (kaish_tools_git::Verb::Switch, vec!["switch", "-c", "x"]),
        (kaish_tools_git::Verb::Checkout, vec!["checkout", "-b", "x"]),
    ] {
        let config = commit_profile().without_verb(verb);
        let r = run_with(&p.fx.root(), "/mnt/ours", &argv, &default_vars(), config).await;
        assert_ne!(r.code, 0, "{argv:?}");
    }
    assert!(!p.ours.join(".git/refs/heads/x").exists() && !p.ours.join(".git/refs/tags/x").exists());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn through_a_real_kernel_branch_tag_and_switch_work() {
    use kaish_kernel::{Kernel, KernelConfig};
    support::require_git();
    let fx = Fixture::empty();
    let root = fx.path("repo");
    std::fs::create_dir_all(&root).expect("dir");
    support::git(&root, &["init", "-q", "-b", "main"]);
    base(&root);
    let backend: Arc<dyn kaish_tool_api::KernelBackend> = Arc::new(StrictBackend::single(PathBuf::from("/mnt"), fx.root()));
    let git = kaish_tools_git::tool(commit_profile()).expect("config");
    let mut cfg = KernelConfig::transient()
        .with_var("GIT_AUTHOR_NAME", Value::String("Kernel Author".into()))
        .with_var("GIT_AUTHOR_EMAIL", Value::String("kernel@example.org".into()))
        .with_var("GIT_AUTHOR_DATE", Value::String(DATE.into()))
        .with_var("GIT_COMMITTER_DATE", Value::String(DATE.into()));
    cfg.cwd = PathBuf::from("/mnt/repo");
    let kernel = Kernel::with_backend(backend, cfg, |_| {}, |tools| tools.register(git)).expect("kernel");
    for script in ["git branch feature", "git tag v1", "git tag -a v2 -m 'second release'", "git switch -c work", "git checkout -b other"] {
        let r = kernel.execute(script).await.expect(script);
        assert_eq!(r.code, 0, "{script}: {}", r.err);
    }
    assert_eq!(git_raw(&root, &["rev-parse", "--abbrev-ref", "HEAD"]).1.trim(), "other");
    let obj = git_raw(&root, &["cat-file", "-p", "v2"]).1;
    assert!(obj.contains("tagger Kernel Author <kernel@example.org> 1785578400 +0000\n\nsecond release\n"), "{obj}");
    assert_eq!(git_raw(&root, &["fsck", "--strict"]).0, 0);
}

// ── review fixes: HEAD as a start point, git's booleans, objects that remain ──

fn append_config(dir: &Path, text: &str) {
    use std::io::Write;
    let mut f = std::fs::OpenOptions::new().append(true).open(dir.join(".git/config")).expect("config");
    f.write_all(text.as_bytes()).expect("write config");
}

/// `switch -c <name> HEAD` resolves HEAD like any other start point. On an
/// unborn HEAD git fails ("invalid reference: HEAD"); this build used to exit
/// 0, print "Switched to a new branch" and make no branch.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_start_of_head_on_an_unborn_head_refuses_in_both_spellings() {
    for (verb, flag, start) in [("switch", "-c", "HEAD"), ("checkout", "-b", "HEAD"), ("switch", "-c", "@"), ("checkout", "-b", "@")] {
        let p = Pair::new(&|_| {});
        let before = support::Fingerprint::take(&p.git_dir(true));
        let r = p.ours("", &[verb, flag, "first", start]).await;
        assert_eq!(r.code, 1, "{verb} {start}: {}", r.err);
        assert!(!r.err.contains("Switched to a new branch"), "{verb} {start}: {}", r.err);
        assert!(!r.text_out().contains("Switched"), "{verb} {start}");
        assert!(r.err.contains("has no commits yet") && r.err.contains("Nothing was changed."), "{verb} {start}: {}", r.err);
        assert_eq!(support::Fingerprint::take(&p.git_dir(true)).differences_from(&before), Vec::<String>::new());
        assert_eq!(std::fs::read_to_string(p.ours.join(".git/HEAD")).expect("HEAD"), "ref: refs/heads/main\n");
        // Git refuses too, and leaves HEAD on main.
        let (code, _, gerr) = p.twin(&[verb, flag, "first", start]);
        assert_ne!(code, 0, "git {verb} {start}: {gerr}");
        assert_eq!(std::fs::read_to_string(p.twin.join(".git/HEAD")).expect("HEAD"), "ref: refs/heads/main\n");
    }
    // Negative control: a born HEAD still takes the same start.
    let p = Pair::new(&base);
    assert_eq!(p.ours("", &["switch", "-c", "ok", "HEAD"]).await.code, 0);
    assert_eq!(p.twin(&["switch", "-c", "ok", "HEAD"]).0, 0);
    p.assert_same("born HEAD");
}

/// The values the review and git's `git_config_bool` disagree on, run against
/// `git config --type=bool` as the oracle. A value git reads as true refuses
/// the write; false lets it proceed; a bad one refuses and names the key and
/// the value (exit 4).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn signing_settings_are_read_as_git_reads_a_boolean() {
    // None is a key written with no `=`.
    let values: &[Option<&str>] = &[
        None, Some(""), Some("true"), Some("TRUE"), Some("yes"), Some("on"), Some("1"), Some("2"), Some("-1"), Some("0"),
        Some("0x10"), Some("1k"), Some("false"), Some("no"), Some("off"), Some("maybe"), Some("08"), Some("2g"), Some("1kb"),
    ];
    let (mut yes, mut no, mut bad) = (0, 0, 0);
    for (section, key, argv) in [
        ("commit", "gpgSign", vec!["commit", "--allow-empty", "-m", "m"]),
        ("tag", "gpgSign", vec!["tag", "t"]),
        ("tag", "gpgSign", vec!["tag", "-a", "t", "-m", "m"]),
    ] {
        for value in values {
            let p = Pair::new(&base);
            // A second key follows, so a key with no `=` cannot be read as the
            // value of the key after it.
            let line = match value {
                None => format!("[{section}]\n\t{key}\n\tzzz = false\n"),
                Some(v) => format!("[{section}]\n\t{key} = {v}\n\tzzz = false\n"),
            };
            for dir in [&p.ours, &p.twin] {
                append_config(dir, &line);
            }
            let name = format!("{section}.{key}");
            let (code, out, gerr) = p.twin(&["config", "--type=bool", &name]);
            let before = support::Fingerprint::take(&p.git_dir(true));
            let r = p.ours("", &argv).await;
            let label = format!("{name}={value:?} {argv:?}");
            match (code, out.trim()) {
                (0, "true") => {
                    yes += 1;
                    assert_eq!(r.code, 4, "[{label}] git signs, ours must refuse: {}", r.err);
                    assert!(r.err.contains(&format!("{name} is true")), "[{label}] {}", r.err);
                    assert!(r.err.contains("Nothing was changed."), "[{label}] {}", r.err);
                    assert_eq!(support::Fingerprint::take(&p.git_dir(true)).differences_from(&before), Vec::<String>::new(), "[{label}] wrote");
                }
                (0, "false") => {
                    no += 1;
                    assert_eq!(r.code, 0, "[{label}] git does not sign: {}", r.err);
                }
                (c, _) => {
                    bad += 1;
                    assert_ne!(c, 0, "{gerr}");
                    assert_eq!(r.code, 4, "[{label}] git dies on a bad boolean: {}", r.err);
                    let shown = value.expect("a key with no = is never bad");
                    assert!(r.err.contains(&name) && r.err.contains(&format!("'{shown}'")), "[{label}] names key and value: {}", r.err);
                    assert!(r.err.contains("Nothing was changed."), "[{label}] {}", r.err);
                    assert_eq!(support::Fingerprint::take(&p.git_dir(true)).differences_from(&before), Vec::<String>::new(), "[{label}] wrote");
                }
            }
        }
    }
    // Negative controls: every outcome happened.
    assert!(yes >= 24 && no >= 12 && bad >= 12, "yes {yes}, no {no}, bad {bad}");
}

/// `commit.gpgSign=2` is true to git, which signs. This shell never signs, so it
/// writes no commit.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn commit_gpgsign_2_writes_no_commit() {
    let p = Pair::new(&base);
    append_config(&p.ours, "[commit]\n\tgpgSign = 2\n");
    let head = git_raw(&p.ours, &["rev-parse", "HEAD"]).1;
    let before = support::Fingerprint::take(&p.git_dir(true));
    let r = p.ours("", &["commit", "--allow-empty", "-m", "unsigned"]).await;
    assert_eq!(r.code, 4, "{}", r.err);
    assert_eq!(git_raw(&p.ours, &["rev-parse", "HEAD"]).1, head);
    assert_eq!(git_raw(&p.ours, &["rev-list", "--count", "--all"]).1, git_raw(&p.twin, &["rev-list", "--count", "--all"]).1);
    assert_eq!(support::Fingerprint::take(&p.git_dir(true)).differences_from(&before), Vec::<String>::new());
}

fn running_as_root() -> bool {
    let probe = tempfile::tempdir().expect("tempdir");
    let dir = probe.path().join("d");
    std::fs::create_dir(&dir).expect("dir");
    let mut perm = std::fs::metadata(&dir).expect("meta").permissions();
    std::os::unix::fs::PermissionsExt::set_mode(&mut perm, 0o555);
    std::fs::set_permissions(&dir, perm).expect("chmod");
    let writable = std::fs::write(dir.join("f"), b"x").is_ok();
    let mut perm = std::fs::metadata(&dir).expect("meta").permissions();
    std::os::unix::fs::PermissionsExt::set_mode(&mut perm, 0o755);
    std::fs::set_permissions(&dir, perm).expect("chmod back");
    writable
}

fn chmod(path: &Path, mode: u32) {
    std::fs::set_permissions(path, std::os::unix::fs::PermissionsExt::from_mode(mode)).expect("chmod");
}

/// An annotated tag's object is written before its ref. A ref that cannot be
/// created (an I/O error, not a refusal) must say the object remains.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_annotated_tag_whose_ref_cannot_be_made_says_its_object_remains() {
    if running_as_root() {
        eprintln!("skipped: root ignores directory modes");
        return;
    }
    let p = Pair::new(&base);
    // The twin makes the same bytes, so its tag id is the one ours writes.
    assert_eq!(p.twin(&["tag", "-a", "v1", "-m", "m"]).0, 0);
    let tag_id = git_raw(&p.twin, &["rev-parse", "v1"]).1.trim().to_string();
    assert!(git_raw(&p.ours, &["cat-file", "-t", &tag_id]).0 != 0, "the object is not there yet");
    let tags = p.ours.join(".git/refs/tags");
    chmod(&tags, 0o555);
    let before_refs = git_raw(&p.ours, &["for-each-ref"]).1;
    let r = p.ours("", &["tag", "-a", "v1", "-m", "m"]).await;
    chmod(&tags, 0o755);
    assert_ne!(r.code, 0, "{}", r.err);
    assert!(r.err.contains("stay in the object database") && r.err.contains("git gc prunes"), "{}", r.err);
    assert!(r.err.contains("no ref or index changed"), "{}", r.err);
    assert!(!r.err.contains("Nothing was changed."), "{}", r.err);
    let (code, kind, _) = git_raw(&p.ours, &["cat-file", "-t", &tag_id]);
    assert!(code == 0 && kind.trim() == "tag", "the tag object remains: {kind}");
    assert_eq!(git_raw(&p.ours, &["for-each-ref"]).1, before_refs, "no ref was made");
    assert!(lockfiles(&p.ours.join(".git")).is_empty());
    // A lightweight tag writes no object and so claims none.
    chmod(&tags, 0o555);
    let r = p.ours("", &["tag", "light"]).await;
    chmod(&tags, 0o755);
    assert_ne!(r.code, 0);
    assert!(!r.err.contains("object database"), "{}", r.err);
}

/// The same for a commit: the ref cannot be moved after the objects exist.
/// The lock check passes (nothing is locked), and the lock cannot be taken.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_commit_whose_branch_cannot_move_says_its_objects_remain() {
    if running_as_root() {
        eprintln!("skipped: root ignores directory modes");
        return;
    }
    let p = Pair::new(&base);
    let heads = p.ours.join(".git/refs/heads");
    let head = git_raw(&p.ours, &["rev-parse", "HEAD"]).1;
    let count = |d: &Path| git_raw(d, &["count-objects"]).1;
    let objects_before = count(&p.ours);
    chmod(&heads, 0o555);
    let r = p.ours("", &["commit", "--allow-empty", "-m", "blocked"]).await;
    chmod(&heads, 0o755);
    assert_ne!(r.code, 0, "{}", r.err);
    assert!(r.err.contains("stay in the object database") && r.err.contains("git gc prunes"), "{}", r.err);
    assert!(!r.err.contains("Nothing was changed."), "{}", r.err);
    assert_eq!(git_raw(&p.ours, &["rev-parse", "HEAD"]).1, head, "the branch did not move");
    assert_ne!(count(&p.ours), objects_before, "the commit object was written");
    assert!(lockfiles(&p.ours.join(".git")).is_empty());
}

/// A directory at `logs/refs/tags/<name>` does not stop git from making the
/// tag, because a tag has no log to write. It did stop this build.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_directory_where_a_tag_log_would_be_does_not_block_the_tag() {
    let p = Pair::new(&base);
    for dir in [&p.ours, &p.twin] {
        std::fs::create_dir_all(dir.join(".git/logs/refs/tags/x")).expect("dir");
        std::fs::write(dir.join(".git/logs/refs/tags/x/inside"), "keep\n").expect("file");
    }
    assert_eq!(p.twin(&["tag", "x"]).0, 0, "git makes the tag");
    let r = p.ours("", &["tag", "x"]).await;
    assert_eq!(r.code, 0, "{}", r.err);
    assert_eq!(p.twin(&["tag", "-a", "y", "-m", "m"]).0, 0);
    for dir in [&p.ours, &p.twin] {
        std::fs::create_dir_all(dir.join(".git/logs/refs/tags/z")).expect("dir");
        std::fs::write(dir.join(".git/logs/refs/tags/z/inside"), "keep\n").expect("file");
    }
    assert_eq!(p.twin(&["tag", "-a", "z", "-m", "m"]).0, 0);
    assert_eq!(p.ours("", &["tag", "-a", "y", "-m", "m"]).await.code, 0);
    assert_eq!(p.ours("", &["tag", "-a", "z", "-m", "m"]).await.code, 0);
    p.assert_same("tag log directories");
    // Negative control: a branch's log directory still stops the branch
    // (git: "there are still logs under").
    for dir in [&p.ours, &p.twin] {
        std::fs::create_dir_all(dir.join(".git/logs/refs/heads/b")).expect("dir");
        std::fs::write(dir.join(".git/logs/refs/heads/b/inside"), "keep\n").expect("file");
    }
    assert_ne!(p.twin(&["branch", "b"]).0, 0);
    assert_ne!(p.ours("", &["branch", "b"]).await.code, 0);
}

// ── pins ───────────────────────────────────────────────────────────────────

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_tag_of_an_annotated_tag_is_a_tag_object_of_type_tag_like_git_makes_it() {
    let p = Pair::new(&|d| {
        base(d);
        support::git(d, &["tag", "-a", "inner", "-m", "inner message"]);
    });
    let r = p.ours("", &["tag", "-a", "outer", "-m", "outer message", "inner"]).await;
    assert_eq!(r.code, 0, "{}", r.err);
    assert_eq!(p.twin(&["tag", "-a", "outer", "-m", "outer message", "inner"]).0, 0);
    p.assert_same("tag of a tag");
    p.assert_tag_same("outer");
    let body = git_raw(&p.ours, &["cat-file", "tag", "outer"]).1;
    assert!(body.contains("\ntype tag\n"), "{body}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_tag_with_a_branch_s_name_keeps_the_short_name_in_the_checkout_log() {
    // A tag named for the branch HEAD is on, and a tag named for the branch
    // being made: `checkout: moving from main to next` is unchanged by either.
    for verb in [("switch", "-c"), ("checkout", "-b")] {
        let p = Pair::new(&|d| {
            base(d);
            support::git(d, &["tag", "main"]);
            support::git(d, &["tag", "next"]);
        });
        let r = p.ours("", &[verb.0, verb.1, "next"]).await;
        assert_eq!(r.code, 0, "{}", r.err);
        let (code, _, gerr) = p.twin(&[verb.0, verb.1, "next"]);
        assert_eq!(code, 0, "{gerr}");
        p.assert_same("tag shares the name");
        assert!(read_log(&p.ours, "logs/HEAD").ends_with("\tcheckout: moving from main to next\n"));
        // And back again, from a branch that shares its name with a tag.
        assert_eq!(p.ours("", &[verb.0, verb.1, "after"]).await.code, 0);
        assert_eq!(p.twin(&[verb.0, verb.1, "after"]).0, 0);
        p.assert_same("from the shared name");
        assert!(read_log(&p.ours, "logs/HEAD").ends_with("\tcheckout: moving from next to after\n"));
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_symlinked_logs_head_does_not_affect_branch_or_tag() {
    let p = Pair::new(&base);
    let victim = p.fx.path("victim");
    std::fs::write(&victim, "precious\n").expect("victim");
    let log = p.ours.join(".git/logs/HEAD");
    std::fs::remove_file(&log).expect("log");
    std::os::unix::fs::symlink(&victim, &log).expect("plant");
    for argv in [vec!["branch", "b1"], vec!["tag", "t1"], vec!["tag", "-a", "t2", "-m", "m"], vec!["branch", "b2", "HEAD~1"]] {
        let r = p.ours("", &argv).await;
        assert_eq!(r.code, 0, "{argv:?}: {}", r.err);
    }
    assert_eq!(std::fs::read_to_string(&victim).expect("victim"), "precious\n", "wrote through the link");
    // Negative control: a switch does write logs/HEAD, so it must refuse.
    let r = p.ours("", &["switch", "-c", "moved"]).await;
    assert_eq!(r.code, 4, "{}", r.err);
    assert_eq!(std::fs::read_to_string(&victim).expect("victim"), "precious\n");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_symlinked_head_refuses_switch_in_both_spellings_and_writes_nothing() {
    for argv in [vec!["switch", "-c", "x"], vec!["checkout", "-b", "x"]] {
        let p = Pair::new(&base);
        let target = p.fx.path("head-target");
        std::fs::copy(p.ours.join(".git/HEAD"), &target).expect("copy");
        let head = p.ours.join(".git/HEAD");
        std::fs::remove_file(&head).expect("rm");
        std::os::unix::fs::symlink(&target, &head).expect("plant");
        let before = std::fs::read(&target).expect("target");
        let refs_before = git_raw(&p.ours, &["for-each-ref"]).1;
        let r = p.ours("", &argv).await;
        assert_ne!(r.code, 0, "{argv:?}: {}", r.err);
        assert!(!r.err.contains("Switched to a new branch"), "{}", r.err);
        assert_eq!(std::fs::read(&target).expect("target"), before, "{argv:?}: wrote through HEAD");
        assert!(std::fs::symlink_metadata(&head).expect("HEAD").file_type().is_symlink(), "{argv:?}: the link was replaced");
        assert_eq!(git_raw(&p.ours, &["for-each-ref"]).1, refs_before, "{argv:?}: a branch appeared");
    }
}

/// BUG, found by the review's §2.1 row: git accepts a name whose directory
/// part ends in a dot (`a./b`), because only the whole name may not end in
/// one. This build asks gix for the ref `refs/heads/a.` while it walks the
/// name's prefixes, and gix says that is not a valid ref name, so the verb
/// fails (exit 1) instead of making the ref.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "BUG: a name with a directory part ending in a dot (a./b) fails in the prefix walk; git accepts it"]
async fn a_name_with_a_directory_part_ending_in_a_dot_is_accepted_as_git_accepts_it() {
    for (verb, name) in [("branch", "a./b"), ("tag", "a./b"), ("branch", "dot./x.LOCK")] {
        let p = Pair::new(&base);
        let (code, _, gerr) = p.twin(&[verb, name]);
        assert_eq!(code, 0, "git {verb} {name}: {gerr}");
        let r = p.ours("", &[verb, name]).await;
        assert_eq!(r.code, 0, "{verb} {name}: {}", r.err);
        p.assert_same(name);
    }
}

// ── divergences filed in docs/issues.md (W40-W44), each ignored until fixed ──

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "W40: core.logAllRefUpdates=false in a non-bare repository refuses; git makes the ref with no log"]
async fn log_all_ref_updates_false_makes_the_branch_with_no_log_as_git_does() {
    let p = Pair::new(&|d| {
        base(d);
        support::git(d, &["config", "core.logAllRefUpdates", "false"]);
    });
    assert_eq!(p.twin(&["branch", "nolog"]).0, 0);
    let r = p.ours("", &["branch", "nolog"]).await;
    assert_eq!(r.code, 0, "{}", r.err);
    p.assert_same("logAllRefUpdates=false");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "W41: branch.autoSetupMerge=simple refuses a start under refs/remotes whose name differs; git writes no tracking"]
async fn auto_setup_merge_simple_writes_no_tracking_when_the_names_differ() {
    let p = Pair::new(&|d| {
        base(d);
        support::git(d, &["config", "remote.origin.url", "/nowhere"]);
        support::git(d, &["config", "remote.origin.fetch", "+refs/heads/*:refs/remotes/origin/*"]);
        support::git(d, &["update-ref", "refs/remotes/origin/feat", "HEAD"]);
        support::git(d, &["config", "branch.autoSetupMerge", "simple"]);
    });
    assert_eq!(p.twin(&["branch", "other", "origin/feat"]).0, 0);
    assert_eq!(git_raw(&p.twin, &["config", "--get-regexp", "^branch\\.other"]).0, 1, "git wrote no tracking");
    let r = p.ours("", &["branch", "other", "origin/feat"]).await;
    assert_eq!(r.code, 0, "{}", r.err);
    p.assert_same("autoSetupMerge=simple");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "W42: @{-1} is not expanded as a start point"]
async fn at_minus_one_is_a_start_point_as_in_git() {
    let p = Pair::new(&|d| {
        base(d);
        support::git(d, &["switch", "-q", "-c", "other"]);
        support::git(d, &["switch", "-q", "main"]);
    });
    assert_eq!(p.twin(&["branch", "prev", "@{-1}"]).0, 0);
    let r = p.ours("", &["branch", "prev", "@{-1}"]).await;
    assert_eq!(r.code, 0, "{}", r.err);
    p.assert_same("@{-1}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "W43: a tag target written as HEAD^{tree} or HEAD:path is not resolved; git tags the tree or the blob"]
async fn a_tag_target_may_be_a_tree_or_blob_spelling() {
    let p = Pair::new(&base);
    for target in ["HEAD^{tree}", "HEAD:a.txt"] {
        let name = format!("t-{}", target.len());
        assert_eq!(p.twin(&["tag", &name, target]).0, 0, "git tags {target}");
        let r = p.ours("", &["tag", &name, target]).await;
        assert_eq!(r.code, 0, "{target}: {}", r.err);
    }
    p.assert_same("tree and blob spellings");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "W44: a log file that exists for a new tag gets the tag's line in git; this writes none"]
async fn an_existing_log_file_for_a_new_tag_is_appended_to_as_git_does() {
    let p = Pair::new(&base);
    for dir in [&p.ours, &p.twin] {
        std::fs::create_dir_all(dir.join(".git/logs/refs/tags")).expect("dir");
        std::fs::write(dir.join(".git/logs/refs/tags/x"), "stale\n").expect("log");
    }
    assert_eq!(p.twin(&["tag", "x"]).0, 0);
    assert_eq!(p.ours("", &["tag", "x"]).await.code, 0);
    p.assert_same("stale tag log");
}
