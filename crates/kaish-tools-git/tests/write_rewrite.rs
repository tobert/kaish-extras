//! Phase 3: `git commit --amend`, `git reset --soft|--mixed <rev>` and the
//! rewrite policy, against real git as the oracle.
//!
//! The same shape as `write_commit.rs`: a TWIN pair of repositories built by
//! the same commands, real git acting in one and this crate in the other, then
//! compared on the commit's bytes, both reflogs, `ORIG_HEAD`, the index and
//! `fsck --strict`.
//!
//! The policy tests are the negative controls (design section 8): every
//! `RewritePolicy` against a published and an unpublished HEAD, for `--amend`
//! and for `reset --soft`. A refusal is asserted by its exact text and a
//! byte-identical `.git`; a success is asserted against the twin that real git
//! changed. A suite that checked refusals alone would pass over a build where
//! amend never works.

#![cfg(feature = "commit")]

#[path = "support.rs"]
mod support;

use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::{Arc, Mutex};

use kaish_tool_api::Tool;
use kaish_tools_git::{GitConfig, GitHooks, HookContext, HookError, HookOutput, RewritePolicy};
use kaish_types::Value;

use support::{Fixture, StrictBackend, TestCtx};

const ORIGINAL: (&str, &str) = ("Original Author", "orig@example.org");
const ORIGINAL_DATE: &str = "2020-02-02T02:02:02+05:30";
const DATE2: &str = "2026-09-09T09:09:09+00:00";

fn git_raw_env(cwd: &Path, args: &[&str], env: &[(&str, &str)]) -> (i32, String, String) {
    let mut cmd = Command::new("git");
    cmd.args(args)
        .current_dir(cwd)
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_SYSTEM", "/dev/null")
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("XDG_CONFIG_HOME", "/nonexistent-kaish-tools-git-hermetic-xdg")
        .env("GIT_TERMINAL_PROMPT", "0")
        .env("GIT_EDITOR", "false");
    for (k, v) in env {
        cmd.env(k, v);
    }
    let out = cmd.output().expect("run git");
    (
        out.status.code().unwrap_or(-1),
        String::from_utf8_lossy(&out.stdout).into_owned(),
        String::from_utf8_lossy(&out.stderr).into_owned(),
    )
}

fn git_raw(cwd: &Path, args: &[&str]) -> (i32, String, String) {
    git_raw_env(cwd, args, &[])
}

/// The identity and clock an amend or reset runs under. They differ from the
/// history's, so a kept author is visibly the old one.
fn later_vars() -> Vec<(&'static str, &'static str)> {
    vec![
        ("GIT_AUTHOR_NAME", "Amend Author"),
        ("GIT_AUTHOR_EMAIL", "amend@example.org"),
        ("GIT_COMMITTER_NAME", "Amender"),
        ("GIT_COMMITTER_EMAIL", "amender@example.org"),
        ("GIT_AUTHOR_DATE", DATE2),
        ("GIT_COMMITTER_DATE", DATE2),
    ]
}

async fn run_tool_with(
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

fn config(policy: RewritePolicy) -> kaish_tools_git::WritableGitConfig {
    GitConfig::read_only().with_commit_profile().with_rewrite_policy(policy)
}

fn read_log(repo: &Path, name: &str) -> String {
    std::fs::read_to_string(repo.join(".git").join(name)).unwrap_or_default()
}

fn lockfiles(git_dir: &Path) -> Vec<String> {
    let mut out = Vec::new();
    fn walk(dir: &Path, out: &mut Vec<String>) {
        for e in std::fs::read_dir(dir).expect("read_dir") {
            let e = e.expect("entry");
            let p = e.path();
            let name = e.file_name().to_string_lossy().into_owned();
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

    async fn ours_with(&self, policy: RewritePolicy, argv: &[&str]) -> kaish_types::ExecResult {
        run_tool_with(&self.fx.root(), "/mnt/ours", argv, &later_vars(), config(policy)).await
    }

    async fn ours(&self, argv: &[&str]) -> kaish_types::ExecResult {
        self.ours_with(RewritePolicy::Allow, argv).await
    }

    fn twin(&self, args: &[&str]) -> (i32, String, String) {
        git_raw_env(&self.twin, args, &later_vars())
    }

    fn git_dir(&self) -> PathBuf {
        self.ours.join(".git")
    }

    fn refused_unchanged(&self, r: &kaish_types::ExecResult, before: &support::Fingerprint, label: &str) {
        assert!(r.code != 0, "[{label}] expected a refusal, got success: {}", r.text_out());
        let diffs = support::Fingerprint::take(&self.git_dir()).differences_from(before);
        assert!(diffs.is_empty(), "[{label}] a refusal changed .git: {diffs:?}");
        assert!(r.err.trim_end().ends_with("Nothing was changed."), "[{label}] {}", r.err);
    }

    /// Both repositories hold the same state after a branch move.
    fn assert_same(&self, label: &str) {
        let one = |p: &Path, a: &[&str]| git_raw(p, a).1;
        assert_eq!(one(&self.ours, &["rev-parse", "HEAD"]), one(&self.twin, &["rev-parse", "HEAD"]), "[{label}] HEAD differs");
        assert_eq!(
            one(&self.ours, &["cat-file", "-p", "HEAD"]),
            one(&self.twin, &["cat-file", "-p", "HEAD"]),
            "[{label}] commit objects differ"
        );
        for log in ["logs/HEAD", "logs/refs/heads/main"] {
            assert_eq!(read_log(&self.ours, log), read_log(&self.twin, log), "[{label}] {log} differs");
        }
        assert_eq!(
            std::fs::read_to_string(self.ours.join(".git/ORIG_HEAD")).ok(),
            std::fs::read_to_string(self.twin.join(".git/ORIG_HEAD")).ok(),
            "[{label}] ORIG_HEAD differs"
        );
        assert_eq!(
            one(&self.ours, &["ls-files", "-s"]),
            one(&self.twin, &["ls-files", "-s"]),
            "[{label}] the index differs"
        );
        let status = |p: &Path| one(p, &["status", "--porcelain=v2", "--untracked-files=all"]);
        assert_eq!(status(&self.ours), status(&self.twin), "[{label}] status differs");
        let (code, _, err) = git_raw(&self.ours, &["fsck", "--strict"]);
        assert_eq!(code, 0, "[{label}] fsck --strict failed on ours: {err}");
        let leftovers = lockfiles(&self.git_dir());
        assert!(leftovers.is_empty(), "[{label}] stray lock or temp files: {leftovers:?}");
    }
}

fn commit_as(dir: &Path, msg: &str) {
    support::git_as(dir, ORIGINAL, ORIGINAL_DATE, &["commit", "-q", "-m", msg]);
}

/// Three commits by the original author, then a staged change.
fn history(dir: &Path) {
    support::write_file(dir, "a.txt", "alpha\n");
    support::write_file(dir, "b.txt", "bravo\n");
    support::write_file(dir, "d/c.txt", "charlie\n");
    support::git(dir, &["add", "."]);
    commit_as(dir, "one");
    support::write_file(dir, "a.txt", "alpha two\n");
    support::git(dir, &["add", "a.txt"]);
    commit_as(dir, "two");
    support::write_file(dir, "e.txt", "echo\n");
    support::git(dir, &["add", "e.txt"]);
    support::git_as(dir, ORIGINAL, ORIGINAL_DATE, &["commit", "-q", "-m", "three\n\nthe body\n"]);
}

fn staged(dir: &Path) {
    history(dir);
    support::write_file(dir, "a.txt", "alpha three\n");
    support::git(dir, &["add", "a.txt"]);
}

fn publish(dir: &Path, rev: &str) {
    support::git(dir, &["update-ref", "refs/remotes/origin/main", rev]);
}

// ── --amend against real git ────────────────────────────────────────────────

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn amend_is_byte_identical_to_gits_and_keeps_the_original_author() {
    type Setup = fn(&Path);
    let dirty_tracked: Setup = |d| {
        staged(d);
        support::write_file(d, "b.txt", "bravo edited\n");
    };
    let cases: Vec<(&str, Setup, Vec<&str>)> = vec![
        ("-m", staged, vec!["--amend", "-m", "new subject"]),
        ("-m paragraphs and trailer", staged, vec!["--amend", "-m", "new subject", "-m", "body", "--trailer", "Fixes: 9"]),
        ("--no-edit", staged, vec!["--amend", "--no-edit"]),
        ("--no-edit with a trailer", staged, vec!["--amend", "--no-edit", "--trailer", "Reviewed-by: R <r@x.y>"]),
        ("--no-edit with -m", staged, vec!["--amend", "--no-edit", "-m", "from m"]),
        ("--author", staged, vec!["--amend", "--no-edit", "--author", "Z Person <z@example.org>"]),
        ("--reset-author", staged, vec!["--amend", "--no-edit", "--reset-author"]),
        ("-a", dirty_tracked, vec!["--amend", "-a", "-m", "all"]),
        ("--only", dirty_tracked, vec!["--amend", "--only", "b.txt", "-m", "only"]),
        ("nothing staged, new message", history, vec!["--amend", "-m", "reworded"]),
        ("nothing staged, --no-edit", history, vec!["--amend", "--no-edit"]),
    ];
    for (label, setup, args) in cases {
        let p = Pair::new(&setup);
        let mut ours = vec!["commit"];
        ours.extend(&args);
        let r = p.ours(&ours).await;
        assert_eq!(r.code, 0, "[{label}] {}", r.err);
        let mut twin = vec!["commit"];
        twin.extend(&args);
        let (code, gout, err) = p.twin(&twin);
        assert_eq!(code, 0, "[{label}] {err}");
        p.assert_same(label);
        assert_eq!(r.text_out(), gout, "[{label}] the summary differs from git's");
        // The point of the control: the author is the history's, unless asked.
        let show = git_raw(&p.ours, &["cat-file", "-p", "HEAD"]).1;
        let keeps = !args.contains(&"--reset-author") && !args.contains(&"--author");
        assert_eq!(show.contains("author Original Author <orig@example.org> 1580"), keeps, "[{label}] {show}");
        assert!(show.contains("committer Amender <amender@example.org> "), "[{label}] {show}");
        assert!(read_log(&p.ours, "logs/HEAD").lines().last().expect("line").contains("\tcommit (amend): "), "[{label}]");
        assert!(!p.ours.join(".git/ORIG_HEAD").exists(), "[{label}] git writes no ORIG_HEAD for an amend");
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn reset_author_with_author_refuses_as_git_does() {
    let p = Pair::new(&staged);
    let argv = ["commit", "--amend", "--no-edit", "--reset-author", "--author", "Z Person <z@example.org>"];
    let before = support::Fingerprint::take(&p.git_dir());
    let r = p.ours(&argv).await;
    assert_eq!(r.code, 2, "{}", r.err);
    p.refused_unchanged(&r, &before, "reset-author and author");
    assert_eq!(p.twin(&argv).0, 128, "git refuses the pair too");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn amend_keeps_a_messy_message_cleaned_as_git_does() {
    let setup = |d: &Path| {
        history(d);
        let tree = support::git(d, &["write-tree"]);
        let parent = support::git(d, &["rev-parse", "HEAD~1"]);
        let msg = d.join("..").join(format!("messy-{}", d.file_name().expect("name").to_string_lossy()));
        std::fs::write(&msg, "subject   \n\n\n\nbody  \t\n# kept\n\n\n").expect("msg");
        let new = support::git_as(d, ORIGINAL, ORIGINAL_DATE, &["commit-tree", &tree, "-p", &parent, "-F", msg.to_str().expect("utf8")]);
        support::git(d, &["update-ref", "refs/heads/main", &new]);
        support::git(d, &["reset", "-q"]);
        support::write_file(d, "a.txt", "alpha four\n");
        support::git(d, &["add", "a.txt"]);
    };
    let p = Pair::new(&setup);
    let r = p.ours(&["commit", "--amend", "--no-edit"]).await;
    assert_eq!(r.code, 0, "{}", r.err);
    assert_eq!(p.twin(&["commit", "-q", "--amend", "--no-edit"]).0, 0);
    p.assert_same("messy message kept");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn amend_of_a_root_commit_and_of_a_merge_keeps_their_parents() {
    // A root commit: no parents, and git calls it "(root-commit)".
    let root = |d: &Path| {
        support::write_file(d, "a.txt", "alpha\n");
        support::git(d, &["add", "."]);
        commit_as(d, "root");
        support::write_file(d, "a.txt", "alpha 2\n");
        support::git(d, &["add", "."]);
    };
    let p = Pair::new(&root);
    let r = p.ours(&["commit", "--amend", "-m", "root again"]).await;
    assert_eq!(r.code, 0, "{}", r.err);
    let (code, gout, err) = p.twin(&["commit", "--amend", "-m", "root again"]);
    assert_eq!(code, 0, "{err}");
    p.assert_same("root");
    assert_eq!(r.text_out(), gout);
    // Git says root-commit for a commit on an unborn branch, not for an amended root.
    assert!(r.text_out().starts_with("[main "), "{}", r.text_out());
    assert!(!r.text_out().contains("root-commit"), "{}", r.text_out());
    assert!(git_raw(&p.ours, &["rev-list", "--parents", "-n1", "HEAD"]).1.trim().split(' ').count() == 1, "still a root");

    // A merge: both parents stay, and an amend that changes nothing is allowed.
    let merge = |d: &Path| {
        history(d);
        support::git(d, &["checkout", "-q", "-b", "side", "HEAD~1"]);
        support::write_file(d, "s.txt", "side\n");
        support::git(d, &["add", "."]);
        commit_as(d, "side");
        support::git(d, &["checkout", "-q", "main"]);
        support::git_as(d, ORIGINAL, ORIGINAL_DATE, &["merge", "-q", "--no-ff", "side", "-m", "merge"]);
    };
    let p = Pair::new(&merge);
    let r = p.ours(&["commit", "--amend", "-m", "merged"]).await;
    assert_eq!(r.code, 0, "{}", r.err);
    assert_eq!(p.twin(&["commit", "-q", "--amend", "-m", "merged"]).0, 0);
    p.assert_same("merge");
    let show = git_raw(&p.ours, &["cat-file", "-p", "HEAD"]).1;
    assert_eq!(show.matches("\nparent ").count() + usize::from(show.starts_with("parent ")), 2, "{show}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn amend_carries_other_headers_over_and_drops_a_signature() {
    let setup = |d: &Path| {
        history(d);
        let tree = support::git(d, &["rev-parse", "HEAD^{tree}"]);
        let parent = support::git(d, &["rev-parse", "HEAD~1"]);
        let body = format!(
            "tree {tree}\nparent {parent}\nauthor Original Author <orig@example.org> 1580589722 +0530\n\
             committer Original Author <orig@example.org> 1580589722 +0530\nx-note hello\n\
             gpgsig -----BEGIN PGP SIGNATURE-----\n \n AAAA\n -----END PGP SIGNATURE-----\n\nsigned\n"
        );
        let file = d.join("..").join(format!("raw-{}", d.file_name().expect("name").to_string_lossy()));
        std::fs::write(&file, body).expect("raw commit");
        let new = support::git(d, &["hash-object", "-t", "commit", "-w", file.to_str().expect("utf8")]);
        support::git(d, &["update-ref", "refs/heads/main", &new]);
        support::write_file(d, "a.txt", "alpha five\n");
        support::git(d, &["add", "a.txt"]);
    };
    let p = Pair::new(&setup);
    let r = p.ours(&["commit", "--amend", "--no-edit"]).await;
    assert_eq!(r.code, 0, "{}", r.err);
    assert_eq!(p.twin(&["commit", "-q", "--amend", "--no-edit"]).0, 0);
    let show = git_raw(&p.ours, &["cat-file", "-p", "HEAD"]).1;
    assert!(show.contains("x-note hello"), "{show}");
    assert!(!show.contains("gpgsig"), "{show}");
    assert_eq!(show, git_raw(&p.twin, &["cat-file", "-p", "HEAD"]).1);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn amend_that_would_leave_the_commit_empty_needs_allow_empty_as_in_git() {
    let setup = |d: &Path| {
        support::write_file(d, "a.txt", "alpha\n");
        support::git(d, &["add", "."]);
        commit_as(d, "base");
        support::git_as(d, ORIGINAL, ORIGINAL_DATE, &["commit", "-q", "--allow-empty", "-m", "empty"]);
    };
    let p = Pair::new(&setup);
    let before = support::Fingerprint::take(&p.git_dir());
    let r = p.ours(&["commit", "--amend", "-m", "x"]).await;
    assert_eq!(r.code, 1, "{}", r.err);
    assert!(r.err.contains("would leave HEAD's commit empty"), "{}", r.err);
    assert!(r.err.contains("--amend --allow-empty"), "{}", r.err);
    p.refused_unchanged(&r, &before, "empty amend");
    assert_eq!(p.twin(&["commit", "-q", "--amend", "-m", "x"]).0, 1, "git refuses too");

    let r = p.ours(&["commit", "--amend", "--allow-empty", "-m", "x"]).await;
    assert_eq!(r.code, 0, "{}", r.err);
    assert_eq!(p.twin(&["commit", "-q", "--amend", "--allow-empty", "-m", "x"]).0, 0);
    p.assert_same("allow-empty amend");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn amend_on_an_unborn_branch_and_without_a_message_refuse_and_change_nothing() {
    let p = Pair::new(&|_| {});
    let before = support::Fingerprint::take(&p.git_dir());
    let r = p.ours(&["commit", "--amend", "-m", "x"]).await;
    assert_eq!(r.code, 1, "{}", r.err);
    assert!(r.err.contains("no commit to amend"), "{}", r.err);
    assert!(r.err.contains("git commit -m '<message>'"), "{}", r.err);
    p.refused_unchanged(&r, &before, "unborn");
    let (code, _, err) = p.twin(&["commit", "--amend", "-m", "x"]);
    assert_eq!(code, 128);
    assert!(err.contains("You have nothing to amend."), "{err}");

    let p = Pair::new(&staged);
    let before = support::Fingerprint::take(&p.git_dir());
    let r = p.ours(&["commit", "--amend"]).await;
    assert_eq!(r.code, 2, "{}", r.err);
    assert!(r.err.contains("There is no editor in this shell; pass -m '<message>' or --no-edit."), "{}", r.err);
    p.refused_unchanged(&r, &before, "no message");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn fixup_and_squash_say_to_make_a_normal_commit() {
    let p = Pair::new(&staged);
    for argv in [vec!["commit", "--fixup", "HEAD"], vec!["commit", "--squash=HEAD", "-m", "x"], vec!["commit", "--amend", "--fixup", "HEAD"]] {
        let before = support::Fingerprint::take(&p.git_dir());
        let r = p.ours(&argv).await;
        assert_eq!(r.code, 2, "{argv:?}: {}", r.err);
        assert!(r.err.contains("Make a normal commit instead"), "{argv:?}: {}", r.err);
        p.refused_unchanged(&r, &before, "fixup");
    }
}

// ── reset against real git ──────────────────────────────────────────────────

/// History with `ahead` three commits on, HEAD two commits back, a staged
/// edit and an unstaged one.
fn behind(d: &Path) {
    history(d);
    support::git(d, &["branch", "ahead"]);
    support::git(d, &["reset", "-q", "--hard", "HEAD~2"]);
    support::write_file(d, "b.txt", "bravo staged\n");
    support::git(d, &["add", "b.txt"]);
    support::write_file(d, "d/c.txt", "charlie unstaged\n");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn reset_soft_and_mixed_match_gits_branch_index_reflog_and_orig_head() {
    support::require_git();
    let probe = Pair::new(&staged);
    let two_back = git_raw(&probe.twin, &["rev-parse", "HEAD~2"]).1.trim().to_string();
    type Setup = fn(&Path);
    let detached: Setup = |d| {
        staged(d);
        support::git(d, &["checkout", "-q", "--detach"]);
    };
    let cases: Vec<(&str, Setup, Vec<String>)> = vec![
        ("--soft HEAD~1", staged, vec!["--soft".into(), "HEAD~1".into()]),
        ("--mixed HEAD~1", staged, vec!["--mixed".into(), "HEAD~1".into()]),
        ("HEAD~1", staged, vec!["HEAD~1".into()]),
        ("HEAD^", staged, vec!["HEAD^".into()]),
        ("--soft HEAD~2", staged, vec!["--soft".into(), "HEAD~2".into()]),
        ("HEAD~2", staged, vec!["HEAD~2".into()]),
        ("--soft oid", staged, vec!["--soft".into(), two_back.clone()]),
        ("--mixed oid", staged, vec!["--mixed".into(), two_back]),
        ("--soft HEAD", staged, vec!["--soft".into(), "HEAD".into()]),
        ("--soft with no revision", staged, vec!["--soft".into()]),
        ("--soft branch name, forward", behind, vec!["--soft".into(), "ahead".into()]),
        ("branch name, forward", behind, vec!["ahead".into()]),
        ("detached --soft", detached, vec!["--soft".into(), "HEAD~1".into()]),
        ("detached mixed", detached, vec!["HEAD~2".into()]),
    ];
    for (label, setup, args) in cases {
        let p = Pair::new(&setup);
        let mut argv = vec!["reset".to_string()];
        argv.extend(args.iter().cloned());
        let words: Vec<&str> = argv.iter().map(String::as_str).collect();
        let r = p.ours(&words).await;
        assert_eq!(r.code, 0, "[{label}] {}", r.err);
        let (code, _, err) = p.twin(&words);
        assert_eq!(code, 0, "[{label}] {err}");
        // `reset --soft` alone: git's HEAD, and the same reflog text.
        p.assert_same(label);
        if args.iter().any(|a| a.starts_with("HEAD")) {
            let line = read_log(&p.ours, "logs/HEAD");
            let last = line.lines().last().expect("a line");
            assert!(last.ends_with(&format!("\treset: moving to {}", args.last().expect("rev"))) || label.contains("no revision"), "[{label}] {last}");
        }
        assert!(p.ours.join(".git/ORIG_HEAD").exists(), "[{label}] git writes ORIG_HEAD for a reset with a revision");
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn reset_with_a_revision_and_paths_copies_index_entries_without_moving() {
    let p = Pair::new(&staged);
    let r = p.ours(&["reset", "HEAD~1", "--", "e.txt"]).await;
    assert_eq!(r.code, 0, "{}", r.err);
    assert_eq!(p.twin(&["reset", "-q", "HEAD~1", "--", "e.txt"]).0, 0);
    p.assert_same("reset <rev> -- <path>");
    assert!(!p.ours.join(".git/ORIG_HEAD").exists(), "no branch moved, so no ORIG_HEAD");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn reset_to_a_missing_revision_refuses_and_changes_nothing() {
    let p = Pair::new(&staged);
    let before = support::Fingerprint::take(&p.git_dir());
    for rev in ["nonexistent", "HEAD~9"] {
        let r = p.ours(&["reset", "--soft", rev]).await;
        assert!(r.code != 0, "{rev}: {}", r.text_out());
        let diffs = support::Fingerprint::take(&p.git_dir()).differences_from(&before);
        assert!(diffs.is_empty(), "{rev}: {diffs:?}");
    }
}

// ── the policy matrix: negative controls ────────────────────────────────────

fn short_head(p: &Pair) -> String {
    git_raw(&p.ours, &["rev-parse", "HEAD"]).1.trim()[..7].to_string()
}

fn short(p: &Pair, rev: &str) -> String {
    git_raw(&p.ours, &["rev-parse", rev]).1.trim()[..7].to_string()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn every_policy_against_a_published_and_an_unpublished_head_for_amend_and_reset() {
    #[derive(Clone, Copy, PartialEq, Debug)]
    enum Outcome {
        Refused,
        Done,
    }
    use Outcome::{Done, Refused};
    // (policy, HEAD is published, outcome)
    let matrix = [
        (RewritePolicy::Refuse, true, Refused),
        (RewritePolicy::Refuse, false, Refused),
        (RewritePolicy::Unpublished, true, Refused),
        (RewritePolicy::Unpublished, false, Done),
        (RewritePolicy::Allow, true, Done),
        (RewritePolicy::Allow, false, Done),
    ];
    for (policy, published, expected) in matrix {
        for verb in ["amend", "reset"] {
            let label = format!("{verb} under {policy:?}, HEAD published={published}");
            let setup = move |d: &Path| {
                staged(d);
                // origin/main is HEAD itself, or one commit behind it.
                publish(d, if published { "HEAD" } else { "HEAD~1" });
            };
            let p = Pair::new(&setup);
            let argv: Vec<&str> = if verb == "amend" {
                vec!["commit", "--amend", "-m", "x"]
            } else {
                vec!["reset", "--soft", "HEAD~1"]
            };
            let before = support::Fingerprint::take(&p.git_dir());
            let r = p.ours_with(policy, &argv).await;
            match expected {
                Refused => {
                    assert_eq!(r.code, 2, "[{label}] {}", r.err);
                    let head = short_head(&p);
                    let command = argv.join(" ");
                    let command = format!("git {command}");
                    let body = match (policy, verb) {
                        (RewritePolicy::Refuse, "amend") => {
                            format!("This shell never replaces a commit, and --amend would replace HEAD {head}.")
                        }
                        (RewritePolicy::Refuse, _) => {
                            "This shell never replaces a commit, and this reset would drop commits from the branch.".to_string()
                        }
                        (_, "amend") => format!(
                            "HEAD {head} is on origin/main, and this shell does not rewrite published commits. \
                             Remote-tracking refs show the last fetch, not the remote itself."
                        ),
                        (_, _) => format!(
                            "This reset would drop commit {head}, which is on origin/main, and this shell does not rewrite published commits. \
                             Remote-tracking refs show the last fetch, not the remote itself."
                        ),
                    };
                    let want = format!(
                        "{command}: refused. {body} Make a new commit instead: git commit -m '<message>'. \
                         There is no flag that overrides this; the policy is set by the program embedding this shell. \
                         Nothing was changed."
                    );
                    assert_eq!(r.err.trim_end(), want, "[{label}]");
                    p.refused_unchanged(&r, &before, &label);
                }
                Done => {
                    assert_eq!(r.code, 0, "[{label}] {}", r.err);
                    let mut twin: Vec<&str> = argv.clone();
                    if verb == "amend" {
                        twin.insert(1, "-q");
                    }
                    let (code, _, err) = p.twin(&twin);
                    assert_eq!(code, 0, "[{label}] {err}");
                    p.assert_same(&label);
                    let last = read_log(&p.ours, "logs/HEAD").lines().last().expect("line").to_string();
                    let tail = if verb == "amend" { "\tcommit (amend): x" } else { "\treset: moving to HEAD~1" };
                    assert!(last.ends_with(tail), "[{label}] {last}");
                }
            }
        }
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_reset_that_drops_a_published_ancestor_refuses_though_head_is_unpublished() {
    // The design's first wording asked only whether HEAD is published. Here
    // origin/main is HEAD~1: HEAD is not published, and HEAD~3 drops HEAD~1.
    let setup = |d: &Path| {
        history(d);
        support::write_file(d, "f.txt", "f\n");
        support::git(d, &["add", "."]);
        commit_as(d, "four");
        publish(d, "HEAD~1");
    };
    let p = Pair::new(&setup);
    let before = support::Fingerprint::take(&p.git_dir());
    let r = p.ours_with(RewritePolicy::Unpublished, &["reset", "--soft", "HEAD~3"]).await;
    assert_eq!(r.code, 2, "{}", r.err);
    let named = [short(&p, "HEAD~1"), short(&p, "HEAD~2")];
    assert!(named.iter().any(|n| r.err.contains(n.as_str())), "names a dropped published commit {named:?}: {}", r.err);
    assert!(r.err.contains("is on origin/main"), "{}", r.err);
    assert!(!r.err.contains(&short_head(&p)), "HEAD is not the published one: {}", r.err);
    p.refused_unchanged(&r, &before, "HEAD~3");
    // Mixed refuses the same way, and an index write never happened.
    let r = p.ours_with(RewritePolicy::Unpublished, &["reset", "HEAD~3"]).await;
    assert_eq!(r.code, 2, "{}", r.err);
    p.refused_unchanged(&r, &before, "mixed HEAD~3");
    // Stopping above the published commit drops only HEAD, which is fine.
    let r = p.ours_with(RewritePolicy::Unpublished, &["reset", "--soft", "HEAD~1"]).await;
    assert_eq!(r.code, 0, "{}", r.err);
    assert_eq!(p.twin(&["reset", "--soft", "HEAD~1"]).0, 0);
    p.assert_same("HEAD~1 drops only HEAD");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_forward_reset_and_a_plain_commit_pass_even_under_refuse() {
    let p = Pair::new(&behind);
    publish(&p.ours, "ahead");
    publish(&p.twin, "ahead");
    let r = p.ours_with(RewritePolicy::Refuse, &["reset", "--soft", "ahead"]).await;
    assert_eq!(r.code, 0, "{}", r.err);
    assert_eq!(p.twin(&["reset", "--soft", "ahead"]).0, 0);
    p.assert_same("forward reset");
    let r = p.ours_with(RewritePolicy::Refuse, &["commit", "-m", "plain"]).await;
    assert_eq!(r.code, 0, "{}", r.err);
    assert_eq!(p.twin(&["commit", "-q", "-m", "plain"]).0, 0);
    p.assert_same("plain commit under Refuse");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn packed_and_non_origin_remote_refs_count_as_published() {
    let setup = |d: &Path| {
        staged(d);
        support::git(d, &["update-ref", "refs/remotes/upstream/release", "HEAD"]);
        support::git(d, &["pack-refs", "--all"]);
    };
    let p = Pair::new(&setup);
    let before = support::Fingerprint::take(&p.git_dir());
    let r = p.ours_with(RewritePolicy::Unpublished, &["commit", "--amend", "-m", "x"]).await;
    assert_eq!(r.code, 2, "{}", r.err);
    assert!(r.err.contains("is on upstream/release"), "{}", r.err);
    p.refused_unchanged(&r, &before, "packed remote ref");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_shallow_repository_refuses_under_unpublished_and_changes_nothing() {
    let p = Pair::new(&|d| {
        staged(d);
        publish(d, "HEAD~2");
    });
    let boundary = git_raw(&p.ours, &["rev-parse", "HEAD~2"]).1;
    std::fs::write(p.ours.join(".git/shallow"), boundary).expect("shallow");
    let before = support::Fingerprint::take(&p.git_dir());
    for argv in [vec!["commit", "--amend", "-m", "x"], vec!["reset", "--soft", "HEAD~1"]] {
        let r = p.ours_with(RewritePolicy::Unpublished, &argv).await;
        assert_eq!(r.code, 4, "{argv:?}: {}", r.err);
        assert!(r.err.contains("shallow"), "{}", r.err);
        p.refused_unchanged(&r, &before, "shallow");
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn plumbing_refusals_name_the_policy_unless_it_is_allow() {
    let p = Pair::new(&staged);
    for (policy, name) in [(RewritePolicy::Refuse, "refuse"), (RewritePolicy::Unpublished, "unpublished")] {
        for verb in ["commit-tree", "update-ref", "hash-object", "write-tree"] {
            let r = p.ours_with(policy, &[verb, "HEAD"]).await;
            assert_eq!(r.code, 2, "{verb}: {}", r.err);
            assert!(r.err.contains(&format!("The rewrite policy is {name}")), "{verb}: {}", r.err);
            assert!(r.err.contains("no flag that overrides it"), "{verb}: {}", r.err);
        }
    }
    let r = p.ours_with(RewritePolicy::Allow, &["commit-tree", "HEAD"]).await;
    assert_eq!(r.code, 2, "{}", r.err);
    assert!(!r.err.contains("rewrite policy"), "{}", r.err);
}

// ── hooks ───────────────────────────────────────────────────────────────────

fn plant_hook(repo: &Path, name: &str, body: &str) {
    use std::os::unix::fs::PermissionsExt;
    let hooks = repo.join(".git/hooks");
    std::fs::create_dir_all(&hooks).expect("hooks dir");
    let path = hooks.join(name);
    std::fs::write(&path, body).expect("hook");
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).expect("chmod");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_post_rewrite_hook_without_a_provider_refuses_an_amend_before_anything_is_written() {
    let p = Pair::new(&staged);
    let sentinel = p.fx.path("sentinel");
    plant_hook(&p.ours, "post-rewrite", &format!("#!/bin/sh\n: > '{}'\nexit 0\n", sentinel.display()));
    let before = support::Fingerprint::take(&p.git_dir());
    for argv in [vec!["commit", "--amend", "-m", "x"], vec!["commit", "--amend", "--no-edit", "--no-verify"]] {
        let r = p.ours(&argv).await;
        assert_eq!(r.code, 4, "{argv:?}: {}", r.err);
        assert!(r.err.contains("post-rewrite"), "{}", r.err);
        assert!(r.err.contains("runs no hooks") || r.err.contains("skipping would look like running"), "{}", r.err);
        p.refused_unchanged(&r, &before, "post-rewrite");
    }
    assert!(!sentinel.exists(), "the hook ran");
    // Negative control: a plain commit does not run post-rewrite, so it goes through.
    let r = p.ours(&["commit", "-m", "plain"]).await;
    assert_eq!(r.code, 0, "{}", r.err);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_reference_transaction_hook_without_a_provider_refuses_a_reset() {
    let p = Pair::new(&staged);
    let sentinel = p.fx.path("sentinel");
    plant_hook(&p.ours, "reference-transaction", &format!("#!/bin/sh\n: > '{}'\nexit 0\n", sentinel.display()));
    let before = support::Fingerprint::take(&p.git_dir());
    let r = p.ours(&["reset", "--soft", "HEAD~1"]).await;
    assert_eq!(r.code, 4, "{}", r.err);
    assert!(r.err.contains("reference-transaction"), "{}", r.err);
    p.refused_unchanged(&r, &before, "reference-transaction");
    assert!(!sentinel.exists());
}

#[derive(Default)]
struct Seen {
    calls: Mutex<Vec<String>>,
}

impl GitHooks for Seen {
    fn pre_commit(&self, ctx: &HookContext<'_>) -> Result<HookOutput, HookError> {
        self.calls.lock().expect("lock").push(format!("pre-commit {:?}", ctx.hooks_present));
        Ok(HookOutput::none())
    }
    fn commit_msg(&self, _: &HookContext<'_>, _: &mut String) -> Result<HookOutput, HookError> {
        self.calls.lock().expect("lock").push("commit-msg".to_string());
        Ok(HookOutput::none())
    }
    fn post_commit(&self, _: &HookContext<'_>, _: &str) -> HookOutput {
        self.calls.lock().expect("lock").push("post-commit".to_string());
        HookOutput::none()
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_provider_is_told_about_post_rewrite_and_runs_for_an_amend() {
    let p = Pair::new(&staged);
    plant_hook(&p.ours, "post-rewrite", "#!/bin/sh\nexit 1\n");
    let seen = Arc::new(Seen::default());
    let cfg = config(RewritePolicy::Allow).with_hooks(seen.clone());
    let r = run_tool_with(&p.fx.root(), "/mnt/ours", &["commit", "--amend", "-m", "x"], &later_vars(), cfg).await;
    assert_eq!(r.code, 0, "{}", r.err);
    let calls = seen.calls.lock().expect("lock").clone();
    assert_eq!(calls, ["pre-commit [\"post-rewrite\"]", "commit-msg", "post-commit"], "{calls:?}");
}

// ── operations ids ──────────────────────────────────────────────────────────

#[test]
fn amend_and_reset_declare_the_rewrite_operation() {
    let schema = kaish_tools_git::tool(GitConfig::read_only().with_commit_profile()).expect("tool").schema();
    for verb in ["commit", "reset"] {
        let leaf = schema.subcommands.iter().find(|c| c.name == verb).expect("leaf");
        assert!(leaf.operations.iter().any(|o| o == "git.rewrite"), "{verb}: {:?}", leaf.operations);
        assert!(leaf.operations.iter().any(|o| o == "git.ref"), "{verb}: {:?}", leaf.operations);
    }
}
