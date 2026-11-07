//! Phase 2: `git commit` and `git var` against real git as the oracle.
//!
//! Every comparison test builds a TWIN: two repositories made by the same
//! commands, so their object ids agree. Real git commits in the twin with
//! fixed identity and dates, this crate commits in the other with the same
//! identity and dates given as kernel variables, and the two are compared on:
//!
//! - the commit's oid, which covers the tree build, the header order, the
//!   identity lines and the message cleanup in one assertion;
//! - `git cat-file -p HEAD`, so a failure shows which of them differs;
//! - both reflogs (`logs/HEAD` and the branch's), line for line: the dates are
//!   fixed, so nothing is excluded;
//! - `git ls-files -s --debug` for the paths a verb touched, so the real index
//!   after `-a` and `--only` is what git's is;
//! - `git fsck --strict` on ours.

#![cfg(feature = "commit")]

#[path = "support.rs"]
mod support;

use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::{Arc, Mutex};

use kaish_tool_api::Tool;
use kaish_tools_git::{GitConfig, GitHooks, HookContext, HookError, HookOutput, Identity};
use kaish_types::Value;

use support::{Fixture, StrictBackend, TestCtx};

const DATE: &str = "2026-08-01T10:00:00+00:00";

/// Run real git, hermetically, with the fixed identity and dates.
fn git_raw(cwd: &Path, args: &[&str]) -> (i32, String, String) {
    git_raw_env(cwd, args, &[])
}

fn git_raw_env(cwd: &Path, args: &[&str], env: &[(&str, &str)]) -> (i32, String, String) {
    let mut cmd = Command::new("git");
    cmd.args(args)
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
        .env("GIT_COMMITTER_DATE", DATE);
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

/// The identity the tool is given, as kernel variables, matching `git_raw`.
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

async fn run_tool(root: &Path, cwd: &str, argv: &[&str]) -> kaish_types::ExecResult {
    run_tool_with(root, cwd, argv, &default_vars(), GitConfig::read_only().with_commit_profile()).await
}

/// The deterministic part of `git ls-files -s --debug`.
fn index_view(repo: &Path) -> Vec<String> {
    let text = support::git(repo, &["ls-files", "-s", "--debug"]);
    let mut out = Vec::new();
    let mut header = String::new();
    for line in text.lines() {
        let t = line.trim_start();
        if t.starts_with("ctime:") || t.starts_with("mtime:") || t.starts_with("dev:") || t.starts_with("uid:") {
            continue;
        }
        if t.starts_with("size:") {
            out.push(format!("{header} {t}"));
        } else {
            header = line.to_string();
        }
    }
    out
}

fn read_log(repo: &Path, name: &str) -> String {
    std::fs::read_to_string(repo.join(".git").join(name)).unwrap_or_default()
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
        run_tool(&self.fx.root(), &format!("/mnt/ours/{cwd}"), argv).await
    }

    pub fn twin(&self, args: &[&str]) -> (i32, String, String) {
        git_raw(&self.twin, args)
    }

    /// Both repositories committed the same thing.
    pub fn assert_same(&self, label: &str) {
        let head = |p: &Path| git_raw(p, &["rev-parse", "HEAD"]).1;
        let show = |p: &Path| git_raw(p, &["cat-file", "-p", "HEAD"]).1;
        assert_eq!(show(&self.ours), show(&self.twin), "[{label}] commit objects differ");
        assert_eq!(head(&self.ours), head(&self.twin), "[{label}] commit oids differ");
        for log in ["logs/HEAD", "logs/refs/heads/main"] {
            assert_eq!(read_log(&self.ours, log), read_log(&self.twin, log), "[{label}] {log} differs");
        }
        assert_eq!(
            index_view(&self.ours),
            index_view(&self.twin),
            "[{label}] the index differs after the commit"
        );
        let (code, _, err) = git_raw(&self.ours, &["fsck", "--strict"]);
        assert_eq!(code, 0, "[{label}] fsck --strict failed on ours: {err}");
        let status = |p: &Path| git_raw(p, &["status", "--porcelain=v2", "--untracked-files=all"]).1;
        assert_eq!(status(&self.ours), status(&self.twin), "[{label}] status differs");
        let leftovers = lockfiles(&self.ours.join(".git"));
        assert!(leftovers.is_empty(), "[{label}] stray lock or temp files: {leftovers:?}");
    }
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

/// A first commit's worth of staged content, built by real git.
fn staged_tree(dir: &Path) {
    use std::os::unix::fs::PermissionsExt;
    support::write_file(dir, "a.txt", "alpha\n");
    // Names that sort differently if a directory is not compared as `name/`.
    support::write_file(dir, "d.txt", "dot-txt\n");
    support::write_file(dir, "d-x", "dash-x\n");
    support::write_file(dir, "d/inner.txt", "inner\n");
    support::write_file(dir, "d/deep/er/leaf.txt", "leaf\n");
    support::write_file(dir, "d0", "d-zero\n");
    support::write_file(dir, "run.sh", "#!/bin/sh\n");
    std::fs::set_permissions(dir.join("run.sh"), std::fs::Permissions::from_mode(0o755)).expect("chmod");
    std::os::unix::fs::symlink("a.txt", dir.join("link")).expect("symlink");
    support::git(dir, &["add", "."]);
    // A gitlink: the object need not exist.
    support::git(
        dir,
        &["update-index", "--add", "--cacheinfo", "160000,1234567890123456789012345678901234567890,sub"],
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_first_commit_is_byte_identical_to_gits() {
    let p = Pair::new(&staged_tree);
    let r = p.ours("", &["commit", "-m", "first"]).await;
    assert_eq!(r.code, 0, "{}", r.err);
    let (code, gout, err) = p.twin(&["commit", "-m", "first"]);
    assert_eq!(code, 0, "{err}");
    p.assert_same("first commit");
    // The summary is git's, byte for byte: abbreviated oid included, which is
    // the same because the commit oid is.
    let out = r.text_out();
    assert_eq!(out, gout);
    assert!(out.starts_with("[main (root-commit) "), "{out}");
    assert!(out.contains(" create mode 100755 run.sh\n"), "{out}");
    assert!(out.contains(" create mode 120000 link\n"), "{out}");
    assert!(out.contains(" create mode 160000 sub\n"), "{out}");
}

fn base(dir: &Path) {
    support::write_file(dir, "a.txt", "alpha\n");
    support::write_file(dir, "b.txt", "bravo\n");
    support::write_file(dir, "d/c.txt", "charlie\n");
    support::write_file(dir, "d/e/f.txt", "foxtrot\n");
    support::git(dir, &["add", "."]);
    support::git(dir, &["commit", "-qm", "base"]);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_second_commit_with_a_parent_and_changes_matches_git() {
    let setup = |dir: &Path| {
        base(dir);
        support::write_file(dir, "a.txt", "alpha two\nline\n");
        support::write_file(dir, "new/n.txt", "n\n");
        support::git(dir, &["add", "a.txt", "new"]);
        support::git(dir, &["rm", "-q", "b.txt"]);
    };
    let p = Pair::new(&setup);
    let r = p.ours("", &["commit", "-m", "second"]).await;
    assert_eq!(r.code, 0, "{}", r.err);
    let (code, gout, _) = p.twin(&["commit", "-m", "second"]);
    assert_eq!(code, 0);
    p.assert_same("second commit");
    let out = r.text_out();
    assert_eq!(out, gout);
    assert!(!out.contains("root-commit"), "{out}");
    assert!(out.contains(" 3 files changed, 3 insertions(+), 2 deletions(-)\n"), "{out}");
    assert!(out.contains(" delete mode 100644 b.txt\n"), "{out}");
    assert!(out.contains(" create mode 100644 new/n.txt\n"), "{out}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_empty_tree_needs_allow_empty_and_then_matches_git() {
    let p = Pair::new(&|_| {});
    let refused = p.ours("", &["commit", "-m", "x"]).await;
    assert_eq!(refused.code, 1, "{}", refused.err);
    assert!(refused.err.contains("Nothing to commit"), "{}", refused.err);
    assert!(refused.err.contains("--allow-empty"), "{}", refused.err);
    assert!(!p.ours.join(".git/refs/heads/main").exists(), "a refused commit moved a ref");

    let r = p.ours("", &["commit", "--allow-empty", "-m", "empty"]).await;
    assert_eq!(r.code, 0, "{}", r.err);
    assert_eq!(p.twin(&["commit", "-q", "--allow-empty", "-m", "empty"]).0, 0);
    p.assert_same("empty tree");
    let tree = git_raw(&p.ours, &["rev-parse", "HEAD^{tree}"]).1;
    assert_eq!(tree.trim(), "4b825dc642cb6eb9a060e54bf8d69288fbee4904");

    // On a branch with history, an unchanged tree is refused too.
    let again = p.ours("", &["commit", "-m", "x"]).await;
    assert_eq!(again.code, 1, "{}", again.err);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn messages_are_assembled_and_cleaned_as_git_does() {
    let messy = "  \n\nsubject   \n\n\n\nbody line  \n# not a comment\n\n\n";
    let cases: Vec<(&str, Vec<&str>)> = vec![
        ("one -m", vec!["-m", "subject"]),
        ("two -m", vec!["-m", "subject", "-m", "second paragraph"]),
        ("three -m with an empty one", vec!["-m", "one", "-m", "", "-m", "three"]),
        ("messy -m", vec!["-m", messy]),
        ("co-authored as the last -m", vec!["-m", "subject", "-m", "Co-authored-by: Ada <ada@example.com>"]),
        ("trailer", vec!["-m", "subject", "--trailer", "Fixes: 12"]),
        ("two trailers", vec!["-m", "subject", "--trailer", "Fixes: 12", "--trailer", "Reviewed-by: R <r@x.y>"]),
        ("equal neighbor trailers", vec!["-m", "subject", "--trailer", "Fixes: 12", "--trailer", "Fixes: 12"]),
        (
            "trailer after co-author block",
            vec!["-m", "subject", "-m", "Co-authored-by: Ada <ada@example.com>", "--trailer", "Signed-off-by: S <s@x.y>"],
        ),
        ("trailer equals sign", vec!["-m", "subject", "--trailer", "fixes=2"]),
        ("trailer after body", vec!["-m", "subject", "-m", "just a body paragraph", "--trailer", "Fixes: 1"]),
        ("trailing space paragraphs", vec!["-m", "a   ", "-m", "b\t\t"]),
        ("windows newlines", vec!["-m", "a\r\n\r\nb\r\n"]),
    ];
    for (label, args) in cases {
        let setup = |dir: &Path| {
            base(dir);
            support::write_file(dir, "a.txt", "changed\n");
            support::git(dir, &["add", "a.txt"]);
        };
        let p = Pair::new(&setup);
        let mut ours_argv = vec!["commit"];
        ours_argv.extend(&args);
        let r = p.ours("", &ours_argv).await;
        assert_eq!(r.code, 0, "[{label}] {}", r.err);
        let mut twin_argv = vec!["commit", "-q"];
        twin_argv.extend(&args);
        let (code, _, err) = p.twin(&twin_argv);
        assert_eq!(code, 0, "[{label}] {err}");
        p.assert_same(label);
    }
}

/// Found while building `tag` (phase 4): git's whitespace is space, tab,
/// newline and carriage return, so a trailing form feed or vertical tab stays
/// in the message. `message::cleanup` strips both. docs/issues.md W35.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "W35: message::cleanup strips a trailing form feed and vertical tab, which git keeps"]
async fn a_trailing_form_feed_or_vertical_tab_stays_in_the_message_as_in_git() {
    for (label, msg) in [("form feed", "subject\u{c}"), ("vertical tab", "subject\u{b}")] {
        let setup = |dir: &Path| {
            base(dir);
            support::write_file(dir, "a.txt", "changed\n");
            support::git(dir, &["add", "a.txt"]);
        };
        let p = Pair::new(&setup);
        let r = p.ours("", &["commit", "-m", msg]).await;
        assert_eq!(r.code, 0, "[{label}] {}", r.err);
        let (code, _, err) = p.twin(&["commit", "-q", "-m", msg]);
        assert_eq!(code, 0, "[{label}] {err}");
        p.assert_same(label);
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_empty_message_aborts_as_git_does() {
    let setup = |dir: &Path| {
        base(dir);
        support::write_file(dir, "a.txt", "changed\n");
        support::git(dir, &["add", "a.txt"]);
    };
    let p = Pair::new(&setup);
    for args in [vec!["-m", "   "], vec!["-m", ""], vec!["-m", "\n\n"]] {
        let mut argv = vec!["commit"];
        argv.extend(&args);
        let before = support::Fingerprint::take(&p.ours.join(".git"));
        let r = p.ours("", &argv).await;
        assert_eq!(r.code, 1, "{args:?}: {}", r.err);
        assert!(r.err.contains("empty commit message"), "{}", r.err);
        assert!(support::Fingerprint::take(&p.ours.join(".git")).differences_from(&before).is_empty(), "{args:?}");
        let mut twin = vec!["commit", "-q"];
        twin.extend(&args);
        assert_eq!(p.twin(&twin).0, 1, "git also aborts for {args:?}");
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn dates_and_the_author_flag_match_git() {
    // Dates git accepts, in the forms this shell reads. A date with no offset
    // is left out: git would use the local zone and this shell uses UTC.
    let dates = [
        "2026-08-01T10:00:00+00:00",
        "2026-08-01T10:00:00Z",
        "2026-08-01 10:00:00 +0200",
        "2026-08-01T10:00:00.123+01:00",
        "@1785578400 +0530",
        "1785578400 -0400",
    ];
    for d in dates {
        let setup = |dir: &Path| {
            base(dir);
            support::write_file(dir, "a.txt", "changed\n");
            support::git(dir, &["add", "a.txt"]);
        };
        let p = Pair::new(&setup);
        let mut vars = default_vars();
        vars.retain(|(k, _)| !k.ends_with("_DATE"));
        vars.push(("GIT_AUTHOR_DATE", d));
        vars.push(("GIT_COMMITTER_DATE", d));
        let r = run_tool_with(&p.fx.root(), "/mnt/ours", &["commit", "-m", "dated"], &vars, GitConfig::read_only().with_commit_profile()).await;
        assert_eq!(r.code, 0, "[{d}] {}", r.err);
        let (code, _, err) = git_raw_env(&p.twin, &["commit", "-q", "-m", "dated"], &[("GIT_AUTHOR_DATE", d), ("GIT_COMMITTER_DATE", d)]);
        assert_eq!(code, 0, "[{d}] {err}");
        let show = |r: &Path| git_raw(r, &["cat-file", "-p", "HEAD"]).1;
        assert_eq!(show(&p.ours), show(&p.twin), "[{d}]");
    }

    // --author replaces the author only.
    let setup = |dir: &Path| {
        base(dir);
        support::write_file(dir, "a.txt", "changed\n");
        support::git(dir, &["add", "a.txt"]);
    };
    let p = Pair::new(&setup);
    let r = p.ours("", &["commit", "-m", "m", "--author", "Other Person <other@example.org>"]).await;
    assert_eq!(r.code, 0, "{}", r.err);
    assert_eq!(p.twin(&["commit", "-q", "-m", "m", "--author", "Other Person <other@example.org>"]).0, 0);
    p.assert_same("--author");
    assert!(git_raw(&p.ours, &["cat-file", "-p", "HEAD"]).1.contains("author Other Person <other@example.org>"));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn commit_a_stages_tracked_changes_and_deletions_like_git() {
    for (label, ours_args, twin_args) in [
        ("-am", vec!["commit", "-am", "all"], vec!["commit", "-q", "-am", "all"]),
        ("-a -m", vec!["commit", "-a", "-m", "all"], vec!["commit", "-q", "-a", "-m", "all"]),
        ("--all", vec!["commit", "--all", "-m", "all"], vec!["commit", "-q", "--all", "-m", "all"]),
    ] {
        let setup = |dir: &Path| {
            base(dir);
            support::write_file(dir, "a.txt", "alpha two\n");
            std::fs::remove_file(dir.join("b.txt")).expect("rm b");
            std::fs::remove_file(dir.join("d/c.txt")).expect("rm c");
            // Untracked files stay untracked; a staged new file is committed.
            support::write_file(dir, "untracked.txt", "u\n");
            support::write_file(dir, "staged-new.txt", "s\n");
            support::git(dir, &["add", "staged-new.txt"]);
            support::write_file(dir, "staged-new.txt", "s then edited\n");
        };
        let p = Pair::new(&setup);
        let r = p.ours("", &ours_args).await;
        assert_eq!(r.code, 0, "[{label}] {}", r.err);
        assert_eq!(p.twin(&twin_args).0, 0, "[{label}]");
        p.assert_same(label);
        let tracked = git_raw(&p.ours, &["ls-files"]).1;
        assert!(!tracked.contains("untracked.txt"), "[{label}] -a staged an untracked file: {tracked}");
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn commit_a_from_a_subdirectory_still_commits_the_whole_tree() {
    let setup = |dir: &Path| {
        base(dir);
        support::write_file(dir, "a.txt", "alpha two\n");
        support::write_file(dir, "d/c.txt", "charlie two\n");
    };
    let p = Pair::new(&setup);
    let r = p.ours("d", &["commit", "-am", "from d"]).await;
    assert_eq!(r.code, 0, "{}", r.err);
    assert_eq!(git_raw(&p.twin.join("d"), &["commit", "-q", "-am", "from d"]).0, 0);
    p.assert_same("-a from a subdirectory");
}

/// Real git's `--only` semantics: the named paths' working-tree content goes
/// in the commit; other staged changes stay staged; the real index ends up
/// holding the worktree content for exactly the named paths.
fn only_setup(dir: &Path) {
    for f in ["a", "b", "c", "e"] {
        support::write_file(dir, f, &format!("{f}\n"));
    }
    support::write_file(dir, "d/x", "x\n");
    support::write_file(dir, "d/y", "y\n");
    support::git(dir, &["add", "."]);
    support::git(dir, &["commit", "-qm", "base"]);
    // b: staged, then edited again. n: staged new. e: staged deletion.
    support::write_file(dir, "b", "b2\n");
    support::git(dir, &["add", "b"]);
    support::write_file(dir, "n", "n\n");
    support::git(dir, &["add", "n"]);
    support::write_file(dir, "b", "b3\n");
    support::write_file(dir, "a", "a2\n");
    support::write_file(dir, "c", "c2\n");
    support::git(dir, &["rm", "-q", "--cached", "e"]);
    std::fs::remove_file(dir.join("d/y")).expect("rm y");
    support::write_file(dir, "d/x", "x2\n");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn only_commits_the_named_paths_and_leaves_other_staged_changes_like_git() {
    for (label, ours_args, twin_args) in [
        ("--only", vec!["commit", "--only", "a", "b", "d", "-m", "only"], vec!["commit", "-q", "--only", "a", "b", "d", "-m", "only"]),
        ("paths", vec!["commit", "a", "b", "d", "-m", "only"], vec!["commit", "-q", "a", "b", "d", "-m", "only"]),
        ("-- paths", vec!["commit", "-m", "only", "--", "a", "b", "d"], vec!["commit", "-q", "-m", "only", "--", "a", "b", "d"]),
        ("a staged-only path", vec!["commit", "n", "-m", "only"], vec!["commit", "-q", "n", "-m", "only"]),
    ] {
        let p = Pair::new(&only_setup);
        let r = p.ours("", &ours_args).await;
        assert_eq!(r.code, 0, "[{label}] {}", r.err);
        let (code, _, err) = p.twin(&twin_args);
        assert_eq!(code, 0, "[{label}] {err}");
        p.assert_same(label);
        if label == "--only" {
            // The facts the comparison above is for, stated once: the commit
            // holds b's worktree content (b3), not what was staged (b2); n
            // and the staged deletion of e are NOT in it.
            let tree = git_raw(&p.ours, &["ls-tree", "-r", "HEAD"]).1;
            assert!(tree.contains("\tb\n") && !tree.contains("\tn\n"), "{tree}");
            let b = git_raw(&p.ours, &["show", "HEAD:b"]).1;
            assert_eq!(b, "b3\n");
            let status = git_raw(&p.ours, &["status", "--short"]).1;
            assert!(status.contains("A  n"), "n must stay staged: {status}");
            assert!(status.contains("D  e"), "the staged deletion must stay staged: {status}");
        }
    }
}

/// W33: `a/` names a directory, so `commit a/` is refused where `a` is a file
/// (git: "did not match any file(s) known to git"), and `d/` commits the
/// directory like `d`.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_trailing_slash_commits_a_directory_and_refuses_a_file() {
    let p = Pair::new(&only_setup);
    let before = support::Fingerprint::take(&p.ours.join(".git"));
    let r = p.ours("", &["commit", "a/", "-m", "x"]).await;
    assert_ne!(r.code, 0, "{}", r.err);
    assert!(support::Fingerprint::take(&p.ours.join(".git")).differences_from(&before).is_empty());
    assert_ne!(p.twin(&["commit", "-q", "a/", "-m", "x"]).0, 0, "git refuses it too");

    let p = Pair::new(&only_setup);
    let r = p.ours("", &["commit", "d/", "-m", "dir"]).await;
    assert_eq!(r.code, 0, "{}", r.err);
    assert_eq!(p.twin(&["commit", "-q", "d/", "-m", "dir"]).0, 0);
    p.assert_same("commit d/");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn only_refuses_a_path_git_does_not_know_and_commits_nothing() {
    let p = Pair::new(&only_setup);
    support::write_file(&p.ours, "untracked", "u\n");
    let before = support::Fingerprint::take(&p.ours.join(".git"));
    let r = p.ours("", &["commit", "untracked", "-m", "x"]).await;
    assert_eq!(r.code, 1, "{}", r.err);
    assert!(r.err.contains("did not match any file known to git"), "{}", r.err);
    assert!(support::Fingerprint::take(&p.ours.join(".git")).differences_from(&before).is_empty());
    support::write_file(&p.twin, "untracked", "u\n");
    assert_ne!(p.twin(&["commit", "-q", "untracked", "-m", "x"]).0, 0, "git refuses it too");

    // A path HEAD knows, deleted from the index, back in the working tree
    // unchanged: the partial commit is HEAD's tree again, so there is nothing
    // to commit, and git says so too.
    let r = p.ours("", &["commit", "e", "-m", "x"]).await;
    assert_eq!(r.code, 1, "{}", r.err);
    assert!(r.err.contains("Nothing to commit"), "{}", r.err);
    support::write_file(&p.twin, "e", "e\n");
    let (code, _, _) = p.twin(&["commit", "-q", "e", "-m", "x"]);
    assert_eq!(code, 1, "git agrees");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_unborn_branch_gets_the_initial_reflog_and_a_branch_with_history_the_plain_one() {
    let p = Pair::new(&|d| {
        support::write_file(d, "a", "a\n");
        support::git(d, &["add", "a"]);
    });
    assert_eq!(p.ours("", &["commit", "-m", "first line\n\nbody"]).await.code, 0);
    assert_eq!(p.twin(&["commit", "-q", "-m", "first line\n\nbody"]).0, 0);
    let log = read_log(&p.ours, "logs/refs/heads/main");
    assert!(log.contains("\tcommit (initial): first line\n"), "{log}");
    assert!(log.starts_with(&"0".repeat(40)), "the first line's old oid is null: {log}");
    for f in ["logs/HEAD", "logs/refs/heads/main"] {
        assert_eq!(read_log(&p.ours, f), read_log(&p.twin, f), "{f}");
    }
    support::write_file(&p.ours, "b", "b\n");
    support::write_file(&p.twin, "b", "b\n");
    support::git(&p.ours, &["add", "b"]);
    support::git(&p.twin, &["add", "b"]);
    assert_eq!(p.ours("", &["commit", "-m", "second"]).await.code, 0);
    assert_eq!(p.twin(&["commit", "-q", "-m", "second"]).0, 0);
    p.assert_same("two commits");
    let log = read_log(&p.ours, "logs/HEAD");
    assert_eq!(log.lines().count(), 2, "{log}");
    assert!(log.lines().nth(1).expect("second").ends_with("\tcommit: second"), "{log}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_detached_head_moves_head_and_says_how_to_keep_the_commit() {
    let setup = |dir: &Path| {
        base(dir);
        support::git(dir, &["checkout", "-q", "--detach"]);
        support::write_file(dir, "a.txt", "changed\n");
        support::git(dir, &["add", "a.txt"]);
    };
    let p = Pair::new(&setup);
    let r = p.ours("", &["commit", "-m", "detached"]).await;
    assert_eq!(r.code, 0, "{}", r.err);
    assert_eq!(p.twin(&["commit", "-q", "-m", "detached"]).0, 0);
    assert_eq!(git_raw(&p.ours, &["rev-parse", "HEAD"]).1, git_raw(&p.twin, &["rev-parse", "HEAD"]).1);
    assert_eq!(read_log(&p.ours, "logs/HEAD"), read_log(&p.twin, "logs/HEAD"));
    assert!(r.text_out().starts_with("[detached HEAD "), "{}", r.text_out());
    let new = git_raw(&p.ours, &["rev-parse", "HEAD"]).1.trim().to_string();
    assert!(r.err.contains(&new) && r.err.contains("git switch -c <name>"), "{}", r.err);
    // The branch did not move.
    let main = git_raw(&p.ours, &["rev-parse", "main"]).1;
    assert_ne!(main.trim(), new);
    assert_eq!(git_raw(&p.ours, &["fsck", "--strict"]).0, 0);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_linked_worktree_commits_with_head_and_index_in_its_own_git_dir() {
    let setup = |dir: &Path| {
        base(dir);
    };
    let p = Pair::new(&setup);
    for repo in [&p.ours, &p.twin] {
        let wt = repo.parent().expect("parent").join(format!("{}-wt", repo.file_name().expect("name").to_string_lossy()));
        support::git(repo, &["worktree", "add", "-q", "-b", "side", wt.to_str().expect("utf8")]);
        support::write_file(&wt, "a.txt", "from the worktree\n");
        support::git(&wt, &["add", "a.txt"]);
    }
    let r = p.ours("../ours-wt", &["commit", "-m", "in worktree"]).await;
    assert_eq!(r.code, 0, "{}", r.err);
    let twin_wt = p.twin.parent().expect("parent").join("twin-wt");
    assert_eq!(git_raw(&twin_wt, &["commit", "-q", "-m", "in worktree"]).0, 0);
    let ours_wt = p.ours.parent().expect("parent").join("ours-wt");
    assert_eq!(git_raw(&ours_wt, &["rev-parse", "HEAD"]).1, git_raw(&twin_wt, &["rev-parse", "HEAD"]).1);
    // The branch ref and its log live in the common dir; HEAD's log in the
    // worktree's. Both are byte-equal to git's, this shell's hand-written
    // append to the worktree's logs/HEAD included.
    let first_dir = |repo: &Path| {
        std::fs::read_dir(repo.join(".git/worktrees")).expect("worktrees").next().expect("one").expect("entry").path()
    };
    let (ours_head_log, twin_head_log) = (std::fs::read(first_dir(&p.ours).join("logs/HEAD")).expect("log"), std::fs::read(first_dir(&p.twin).join("logs/HEAD")).expect("log"));
    assert!(ours_head_log == twin_head_log, "worktree logs/HEAD differs\nours: {:?}\ntwin: {:?}", String::from_utf8_lossy(&ours_head_log), String::from_utf8_lossy(&twin_head_log));
    let (ours_side, twin_side) = (read_log(&p.ours, "logs/refs/heads/side"), read_log(&p.twin, "logs/refs/heads/side"));
    assert_eq!(ours_side, twin_side, "logs/refs/heads/side differs");
    assert!(ours_side.contains("\tcommit: in worktree"), "{ours_side}");
    // HEAD in the common dir is the main worktree's: untouched.
    assert_eq!(read_log(&p.ours, "logs/HEAD"), read_log(&p.twin, "logs/HEAD"));
    assert_eq!(git_raw(&ours_wt, &["fsck", "--strict"]).0, 0);
    // main did not move.
    assert_eq!(git_raw(&p.ours, &["rev-parse", "main"]).1, git_raw(&p.twin, &["rev-parse", "main"]).1);
}

// ── identity ───────────────────────────────────────────────────────────────

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn var_prints_the_identity_in_gits_format() {
    let p = Pair::new(&base);
    for (name, git_name) in [("GIT_AUTHOR_IDENT", "GIT_AUTHOR_IDENT"), ("GIT_COMMITTER_IDENT", "GIT_COMMITTER_IDENT")] {
        let r = p.ours("", &["var", name]).await;
        assert_eq!(r.code, 0, "{}", r.err);
        let want = git_raw(&p.twin, &["var", git_name]).1;
        assert_eq!(r.text_out(), want, "{name}");
    }
    let r = p.ours("", &["var", "GIT_EDITOR"]).await;
    assert_eq!(r.code, 2);
    assert!(r.err.contains("GIT_AUTHOR_IDENT") && r.err.trim_end().ends_with("Nothing was changed."), "{}", r.err);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn no_identity_refuses_with_exit_2_naming_both_variables_and_the_embedder() {
    let p = Pair::new(&|d| {
        base(d);
        support::write_file(d, "a.txt", "changed\n");
        support::git(d, &["add", "a.txt"]);
        // A config identity must not satisfy the requirement.
        support::git(d, &["config", "user.name", "Config Name"]);
        support::git(d, &["config", "user.email", "config@example.invalid"]);
    });
    let before = support::Fingerprint::take(&p.ours.join(".git"));
    let r = run_tool_with(&p.fx.root(), "/mnt/ours", &["commit", "-m", "x"], &[], GitConfig::read_only().with_commit_profile()).await;
    assert_eq!(r.code, 2, "{}", r.err);
    assert!(r.err.contains("GIT_AUTHOR_NAME") && r.err.contains("GIT_AUTHOR_EMAIL"), "{}", r.err);
    assert!(r.err.contains("pinned an identity"), "{}", r.err);
    assert!(r.err.trim_end().ends_with("Nothing was changed."), "{}", r.err);
    assert!(support::Fingerprint::take(&p.ours.join(".git")).differences_from(&before).is_empty());
    let r = run_tool_with(&p.fx.root(), "/mnt/ours", &["var", "GIT_AUTHOR_IDENT"], &[], GitConfig::read_only().with_commit_profile()).await;
    assert_eq!(r.code, 2, "{}", r.err);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_pinned_identity_wins_over_the_variables_and_over_a_config_file() {
    let p = Pair::new(&|d| {
        base(d);
        support::write_file(d, "a.txt", "changed\n");
        support::git(d, &["add", "a.txt"]);
        support::git(d, &["config", "user.name", "Config Name"]);
    });
    let config = GitConfig::read_only().with_commit_profile().with_identity(
        Identity::new("Pinned Author", "pa@example.org").expect("identity"),
        Identity::new("Pinned Committer", "pc@example.org").expect("identity"),
    );
    // Variables are ignored entirely when an identity is pinned.
    let mut vars = default_vars();
    vars.push(("GIT_AUTHOR_NAME", "Variable Name"));
    let r = run_tool_with(&p.fx.root(), "/mnt/ours", &["commit", "-m", "pinned"], &vars, config.clone()).await;
    assert_eq!(r.code, 0, "{}", r.err);
    let commit = git_raw(&p.ours, &["cat-file", "-p", "HEAD"]).1;
    assert!(commit.contains("author Pinned Author <pa@example.org> "), "{commit}");
    assert!(commit.contains("committer Pinned Committer <pc@example.org> "), "{commit}");
    // `--author` may still set the author.
    support::write_file(&p.ours, "a.txt", "changed again\n");
    support::git(&p.ours, &["add", "a.txt"]);
    let r = run_tool_with(&p.fx.root(), "/mnt/ours", &["commit", "-m", "x", "--author", "Flag Person <flag@example.org>"], &vars, config.clone()).await;
    assert_eq!(r.code, 0, "{}", r.err);
    let commit = git_raw(&p.ours, &["cat-file", "-p", "HEAD"]).1;
    assert!(commit.contains("author Flag Person <flag@example.org> "), "{commit}");
    assert!(commit.contains("committer Pinned Committer <pc@example.org> "), "{commit}");
    let var = run_tool_with(&p.fx.root(), "/mnt/ours", &["var", "GIT_COMMITTER_IDENT"], &vars, config).await;
    assert!(var.text_out().starts_with("Pinned Committer <pc@example.org> "), "{}", var.text_out());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn git_config_user_identity_refuses_naming_git_var() {
    let p = Pair::new(&base);
    for argv in [vec!["config", "user.name"], vec!["config", "user.email", "a@b.c"], vec!["config", "--get", "user.name"]] {
        let r = p.ours("", &argv).await;
        assert_eq!(r.code, 2, "{argv:?}: {}", r.err);
        assert!(r.err.contains("git var GIT_AUTHOR_IDENT"), "{}", r.err);
        assert!(r.err.trim_end().ends_with("Nothing was changed."), "{}", r.err);
    }
}

// ── refusals ───────────────────────────────────────────────────────────────

/// Every refused spelling: exit code and the text that must appear. Run on a
/// repository with a staged change; each must leave `.git` byte-identical.
const REFUSED: &[(&[&str], i64, &str)] = &[
    (&["commit", "--amend"], 2, "There is no editor in this shell; pass -m '<message>' or --no-edit."),
    (&["commit", "--amend", "-a"], 2, "or --no-edit."),
    (&["commit", "--no-edit"], 2, "only --amend has an old message"),
    (&["commit", "--reset-author", "-m", "x"], 2, "no --amend here"),
    (&["commit", "--fixup", "HEAD", "-m", "x"], 2, "--fixup"),
    (&["commit", "--squash=HEAD", "-m", "x"], 2, "--squash"),
    (&["commit", "-S", "-m", "x"], 2, "never signs"),
    (&["commit", "--gpg-sign", "-m", "x"], 2, "never signs"),
    (&["commit", "--gpg-sign=ABCD", "-m", "x"], 2, "never signs"),
    (&["commit", "-p", "-m", "x"], 2, "terminal"),
    (&["commit", "--interactive"], 2, "terminal"),
    (&["commit", "-e", "-m", "x"], 2, "no editor"),
    (&["commit", "--edit", "-m", "x"], 2, "no editor"),
    (&["commit", "-v", "-m", "x"], 2, "no editor"),
    (&["commit"], 2, "There is no editor in this shell; pass -m '<message>'."),
    (&["commit", "-a"], 2, "There is no editor in this shell; pass -m '<message>'."),
    (&["commit", "-i", "a.txt", "-m", "x"], 2, "-i stages"),
    (&["commit", "-m", "x", "-F", "msg.txt"], 2, "-m and -F"),
    (&["commit", "-F", "-"], 2, "standard input"),
    (&["commit", "-a", "a.txt", "-m", "x"], 2, "-a commits every tracked change"),
    (&["commit", "--only", "-m", "x"], 2, "--only needs the paths"),
    (&["commit", "-m", "x", "--trailer", "no separator"], 2, "has no ':' or '='"),
    (&["commit", "-m", "x", "--trailer", "Key:"], 2, "no value"),
    (&["commit", "-m", "x", "--author", "nobody"], 2, "Name <email>"),
    (&["commit", "-m", "   "], 1, "empty commit message"),
    (&["var"], 2, "No variable was named"),
    (&["var", "GIT_PAGER"], 2, "GIT_AUTHOR_IDENT and GIT_COMMITTER_IDENT only"),
];

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn every_refused_commit_spelling_leaves_git_byte_identical_and_has_five_parts() {
    let p = Pair::new(&|d| {
        base(d);
        support::write_file(d, "a.txt", "changed\n");
        support::git(d, &["add", "a.txt"]);
        support::write_file(d, "b.txt", "unstaged\n");
    });
    for (argv, code, text) in REFUSED {
        let before = support::Fingerprint::take(&p.ours.join(".git"));
        let r = p.ours("", argv).await;
        assert_eq!(r.code, *code, "{argv:?}: {}", r.err);
        assert!(r.err.contains(text), "{argv:?}: {}", r.err);
        assert!(r.err.contains(": refused."), "{argv:?}: {}", r.err);
        assert!(r.err.trim_end().ends_with("Nothing was changed."), "{argv:?}: {}", r.err);
        let diffs = support::Fingerprint::take(&p.ours.join(".git")).differences_from(&before);
        assert!(diffs.is_empty(), "{argv:?} changed .git: {diffs:?}");
    }
    // The control: the same repository commits when asked properly.
    let r = p.ours("", &["commit", "-m", "ok"]).await;
    assert_eq!(r.code, 0, "{}", r.err);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn repository_states_and_settings_that_change_the_commit_refuse_with_exit_4() {
    type Setup = fn(&Path);
    let staged = |d: &Path| {
        base(d);
        support::write_file(d, "a.txt", "changed\n");
        support::git(d, &["add", "a.txt"]);
    };
    let _ = staged;
    let cases: Vec<(&str, Setup, &str)> = vec![
        ("merge in progress", |d| { base(d); std::fs::write(d.join(".git/MERGE_HEAD"), "0".repeat(40) + "\n").expect("w"); }, "a merge is in progress"),
        ("cherry-pick", |d| { base(d); std::fs::write(d.join(".git/CHERRY_PICK_HEAD"), "0".repeat(40) + "\n").expect("w"); }, "a cherry-pick is in progress"),
        ("rebase-merge", |d| { base(d); std::fs::create_dir(d.join(".git/rebase-merge")).expect("w"); }, "a rebase is in progress"),
        ("bisect", |d| { base(d); std::fs::write(d.join(".git/BISECT_LOG"), "x\n").expect("w"); }, "a bisect is in progress"),
        ("commit.gpgSign", |d| { base(d); support::git(d, &["config", "commit.gpgSign", "true"]); }, "commit.gpgSign is true"),
        ("tag.gpgSign", |d| { base(d); support::git(d, &["config", "tag.gpgSign", "true"]); }, "tag.gpgSign is true"),
        ("commitEncoding", |d| { base(d); support::git(d, &["config", "i18n.commitEncoding", "latin1"]); }, "i18n.commitEncoding"),
        ("commit.cleanup=strip", |d| { base(d); support::git(d, &["config", "commit.cleanup", "strip"]); }, "commit.cleanup is 'strip'"),
        ("logAllRefUpdates=false", |d| { base(d); support::git(d, &["config", "core.logAllRefUpdates", "false"]); }, "core.logAllRefUpdates is off"),
        ("core.symlinks=false", |d| { base(d); support::git(d, &["config", "core.symlinks", "false"]); }, "core.symlinks=false"),
        ("hooksPath outside", |d| { base(d); support::git(d, &["config", "core.hooksPath", "/elsewhere"]); }, "core.hooksPath points outside"),
    ];
    for (label, setup, text) in cases {
        let p = Pair::new(&|d| {
            setup(d);
            support::write_file(d, "a.txt", "changed\n");
            let _ = Command::new("git").args(["add", "a.txt"]).current_dir(d).env("GIT_CONFIG_GLOBAL", "/dev/null").output();
        });
        let before = support::Fingerprint::take(&p.ours.join(".git"));
        let r = p.ours("", &["commit", "-a", "-m", "x"]).await;
        assert_eq!(r.code, 4, "[{label}] {}", r.err);
        assert!(r.err.contains(text), "[{label}] {}", r.err);
        assert!(r.err.trim_end().ends_with("Nothing was changed."), "[{label}] {}", r.err);
        assert!(support::Fingerprint::take(&p.ours.join(".git")).differences_from(&before).is_empty(), "[{label}] changed .git");
    }
}

// ── hooks ──────────────────────────────────────────────────────────────────

fn plant_hook(repo: &Path, name: &str, body: &str) {
    use std::os::unix::fs::PermissionsExt;
    let hooks = repo.join(".git/hooks");
    std::fs::create_dir_all(&hooks).expect("hooks dir");
    let path = hooks.join(name);
    std::fs::write(&path, body).expect("hook");
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).expect("chmod");
}

fn staged_change(d: &Path) {
    base(d);
    support::write_file(d, "a.txt", "changed\n");
    support::git(d, &["add", "a.txt"]);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_hook_without_a_provider_refuses_naming_it_and_runs_nothing() {
    for hook in ["pre-commit", "prepare-commit-msg", "commit-msg", "post-commit", "reference-transaction"] {
        let p = Pair::new(&staged_change);
        let sentinel = p.fx.path("sentinel");
        plant_hook(&p.ours, hook, &format!("#!/bin/sh\n: > '{}'\nexit 0\n", sentinel.display()));
        let before = support::Fingerprint::take(&p.ours.join(".git"));
        for argv in [vec!["commit", "-m", "x"], vec!["commit", "--no-verify", "-m", "x"]] {
            let r = p.ours("", &argv).await;
            assert_eq!(r.code, 4, "[{hook}] {argv:?}: {}", r.err);
            assert!(r.err.contains(hook), "[{hook}] {}", r.err);
            assert!(r.err.contains("runs no hooks") || r.err.contains("skipping would look like running"), "{}", r.err);
            assert!(r.err.trim_end().ends_with("Nothing was changed."), "{}", r.err);
        }
        assert!(!sentinel.exists(), "[{hook}] the hook ran");
        assert!(support::Fingerprint::take(&p.ours.join(".git")).differences_from(&before).is_empty(), "[{hook}] changed .git");
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_sample_a_directory_and_a_non_executable_file_are_not_hooks() {
    use std::os::unix::fs::PermissionsExt;
    let p = Pair::new(&staged_change);
    plant_hook(&p.ours, "pre-commit.sample", "#!/bin/sh\nexit 1\n");
    std::fs::create_dir_all(p.ours.join(".git/hooks/commit-msg")).expect("dir named like a hook");
    let plain = p.ours.join(".git/hooks/post-commit");
    std::fs::write(&plain, "#!/bin/sh\nexit 1\n").expect("file");
    std::fs::set_permissions(&plain, std::fs::Permissions::from_mode(0o644)).expect("chmod");
    let r = p.ours("", &["commit", "-m", "x"]).await;
    assert_eq!(r.code, 0, "{}", r.err);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn hooks_path_inside_the_tree_is_scanned_and_any_value_counts() {
    let p = Pair::new(&staged_change);
    support::git(&p.ours, &["config", "core.hooksPath", "githooks"]);
    std::fs::create_dir_all(p.ours.join("githooks")).expect("dir");
    let r = p.ours("", &["commit", "-m", "empty hooks dir"]).await;
    assert_eq!(r.code, 0, "an empty hooksPath directory has no hook: {}", r.err);
    let p = Pair::new(&staged_change);
    support::git(&p.ours, &["config", "core.hooksPath", "githooks"]);
    std::fs::create_dir_all(p.ours.join("githooks")).expect("dir");
    use std::os::unix::fs::PermissionsExt;
    std::fs::write(p.ours.join("githooks/pre-commit"), "#!/bin/sh\n").expect("hook");
    std::fs::set_permissions(p.ours.join("githooks/pre-commit"), std::fs::Permissions::from_mode(0o755)).expect("chmod");
    let r = p.ours("", &["commit", "-m", "x"]).await;
    assert_eq!(r.code, 4, "{}", r.err);
    assert!(r.err.contains("pre-commit"), "{}", r.err);
}

#[derive(Default)]
struct Recorder {
    calls: Mutex<Vec<String>>,
    reject_pre_commit: bool,
    rewrite_message: Option<String>,
    /// Moves the branch with real git, to race the commit.
    race: Option<PathBuf>,
}

impl GitHooks for Recorder {
    fn pre_commit(&self, ctx: &HookContext<'_>) -> Result<HookOutput, HookError> {
        self.calls.lock().expect("lock").push(format!("pre-commit tree={} hooks={:?} no_verify={}", ctx.tree, ctx.hooks_present, ctx.no_verify));
        if let Some(repo) = &self.race {
            git_raw(repo, &["update-ref", "refs/heads/main", "HEAD~0"]);
            let other = git_raw(repo, &["commit-tree", "-p", "HEAD", "-m", "raced", "HEAD^{tree}"]).1;
            git_raw(repo, &["update-ref", "refs/heads/main", other.trim()]);
        }
        if self.reject_pre_commit {
            return Err(HookError::new("pre-commit", "lint failed"));
        }
        Ok(HookOutput::with_message("pre-commit ran"))
    }

    fn commit_msg(&self, _ctx: &HookContext<'_>, message: &mut String) -> Result<HookOutput, HookError> {
        self.calls.lock().expect("lock").push(format!("commit-msg {message:?}"));
        if let Some(m) = &self.rewrite_message {
            *message = m.clone();
        }
        Ok(HookOutput::none())
    }

    fn post_commit(&self, ctx: &HookContext<'_>, commit: &str) -> HookOutput {
        self.calls.lock().expect("lock").push(format!("post-commit {commit} worktree={}", ctx.worktree.is_some()));
        HookOutput::with_message("post-commit ran")
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_provider_is_called_in_order_and_may_rewrite_the_message() {
    let p = Pair::new(&staged_change);
    plant_hook(&p.ours, "pre-commit", "#!/bin/sh\nexit 1\n");
    plant_hook(&p.ours, "commit-msg", "#!/bin/sh\nexit 1\n");
    let rec = Arc::new(Recorder { rewrite_message: Some("rewritten   \n\n\n\ntail  \n".to_string()), ..Recorder::default() });
    let config = GitConfig::read_only().with_commit_profile().with_hooks(rec.clone());
    let r = run_tool_with(&p.fx.root(), "/mnt/ours", &["commit", "--no-verify", "-m", "original"], &default_vars(), config).await;
    assert_eq!(r.code, 0, "{}", r.err);
    let calls = rec.calls.lock().expect("lock").clone();
    assert_eq!(calls.len(), 3, "{calls:?}");
    let tree = git_raw(&p.ours, &["rev-parse", "HEAD^{tree}"]).1.trim().to_string();
    assert_eq!(calls[0], format!("pre-commit tree={tree} hooks=[\"pre-commit\", \"commit-msg\"] no_verify=true"), "{calls:?}");
    assert_eq!(calls[1], "commit-msg \"original\\n\"", "{calls:?}");
    let head = git_raw(&p.ours, &["rev-parse", "HEAD"]).1.trim().to_string();
    assert_eq!(calls[2], format!("post-commit {head} worktree=true"), "{calls:?}");
    // The hook's rewrite is cleaned again, as git cleans the edited file.
    let msg = git_raw(&p.ours, &["log", "-1", "--format=%B"]).1;
    assert_eq!(msg.trim_end_matches('\n'), "rewritten\n\ntail");
    assert!(r.err.contains("pre-commit ran") && r.err.contains("post-commit ran"), "{}", r.err);
    // The scripts themselves never ran: a provider decides, this crate spawns nothing.
    assert_eq!(git_raw(&p.ours, &["fsck", "--strict"]).0, 0);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_rejecting_provider_stops_the_commit_and_the_index_and_branch_stay() {
    let p = Pair::new(&|d| {
        base(d);
        support::write_file(d, "a.txt", "tracked change\n");
    });
    plant_hook(&p.ours, "pre-commit", "#!/bin/sh\nexit 1\n");
    let head_before = git_raw(&p.ours, &["rev-parse", "HEAD"]).1;
    let index_before = std::fs::read(p.ours.join(".git/index")).expect("index");
    let rec = Arc::new(Recorder { reject_pre_commit: true, ..Recorder::default() });
    let config = GitConfig::read_only().with_commit_profile().with_hooks(rec.clone());
    let r = run_tool_with(&p.fx.root(), "/mnt/ours", &["commit", "-am", "x"], &default_vars(), config).await;
    assert_eq!(r.code, 1, "{}", r.err);
    assert!(r.err.contains("pre-commit hook refused the commit: lint failed"), "{}", r.err);
    assert_eq!(git_raw(&p.ours, &["rev-parse", "HEAD"]).1, head_before);
    assert_eq!(std::fs::read(p.ours.join(".git/index")).expect("index"), index_before, "-a's index change must not land when the hook rejects");
    assert!(lockfiles(&p.ours.join(".git")).is_empty());
    let calls = rec.calls.lock().expect("lock").clone();
    assert_eq!(calls.len(), 1, "no later hook runs after a rejection: {calls:?}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_branch_that_moves_between_plan_and_ref_update_fails_the_compare_and_swap() {
    let p = Pair::new(&staged_change);
    let rec = Arc::new(Recorder { race: Some(p.ours.clone()), ..Recorder::default() });
    let config = GitConfig::read_only().with_commit_profile().with_hooks(rec);
    let before = git_raw(&p.ours, &["rev-parse", "HEAD"]).1.trim().to_string();
    let r = run_tool_with(&p.fx.root(), "/mnt/ours", &["commit", "-m", "x"], &default_vars(), config).await;
    assert_eq!(r.code, 1, "{}", r.err);
    let raced = git_raw(&p.ours, &["rev-parse", "main"]).1.trim().to_string();
    assert_ne!(raced, before, "the test's race did not move the branch");
    assert!(r.err.contains("refs/heads/main"), "{}", r.err);
    assert!(r.err.contains(&before) && r.err.contains(&raced), "both oids: {}", r.err);
    assert!(r.err.contains("Run git status"), "{}", r.err);
    // The branch is where the other writer left it, and no half transaction remains.
    assert_eq!(git_raw(&p.ours, &["rev-parse", "main"]).1.trim(), raced);
    assert!(lockfiles(&p.ours.join(".git")).is_empty());
    let log = read_log(&p.ours, "logs/refs/heads/main");
    assert!(!log.contains("commit: x"), "a failed swap wrote a reflog line: {log}");
}

// ── containment ────────────────────────────────────────────────────────────

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_planted_link_or_hard_link_in_the_ref_write_set_refuses_the_commit_untouched() {
    for target in ["logs/refs/heads/main", "logs/HEAD", "refs/heads/main.lock", "packed-refs"] {
        let p = Pair::new(&staged_change);
        let outside = p.fx.path("outside");
        std::fs::create_dir_all(&outside).expect("outside");
        let victim = outside.join("victim");
        std::fs::write(&victim, "precious\n").expect("victim");
        let dest = p.ours.join(".git").join(target);
        std::fs::create_dir_all(dest.parent().expect("parent")).expect("parent");
        let _ = std::fs::remove_file(&dest);
        std::os::unix::fs::symlink(&victim, &dest).expect("plant");
        let before = support::Fingerprint::take(&p.ours.join(".git"));
        let r = p.ours("", &["commit", "-m", "x"]).await;
        // `packed-refs` is read by the read side first and refuses there (exit 1).
        let want = if target == "packed-refs" { 1 } else { 4 };
        assert_eq!(r.code, want, "[{target}] {}", r.err);
        assert!(!r.err.contains(outside.to_str().expect("utf8")), "[{target}] named a path outside the mount: {}", r.err);
        assert_eq!(std::fs::read_to_string(&victim).expect("victim"), "precious\n", "[{target}] wrote through the link");
        let diffs = support::Fingerprint::take(&p.ours.join(".git")).differences_from(&before);
        assert!(diffs.is_empty(), "[{target}] {diffs:?}");
    }

    // A hard link: `lstat` says regular file, and the append would reach the other name.
    for target in ["logs/refs/heads/main", "logs/HEAD"] {
        let p = Pair::new(&staged_change);
        let victim = p.fx.path("hardlinked");
        std::fs::write(&victim, "precious\n").expect("victim");
        let dest = p.ours.join(".git").join(target);
        std::fs::create_dir_all(dest.parent().expect("parent")).expect("parent");
        let _ = std::fs::remove_file(&dest);
        std::fs::hard_link(&victim, &dest).expect("link");
        let r = p.ours("", &["commit", "-m", "x"]).await;
        assert_eq!(r.code, 4, "[{target}] {}", r.err);
        assert_eq!(std::fs::read_to_string(&victim).expect("victim"), "precious\n", "[{target}] appended through a hard link");
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_stale_index_lock_is_reported_and_not_removed_by_commit_a() {
    let p = Pair::new(&|d| {
        base(d);
        support::write_file(d, "a.txt", "changed\n");
    });
    std::fs::write(p.ours.join(".git/index.lock"), "").expect("lock");
    let r = p.ours("", &["commit", "-am", "x"]).await;
    assert_eq!(r.code, 4, "{}", r.err);
    assert!(r.err.contains("index.lock"), "{}", r.err);
    assert!(p.ours.join(".git/index.lock").exists(), "a lock we did not take was removed");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_stale_ref_lock_fails_the_commit_and_is_left_alone() {
    let p = Pair::new(&staged_change);
    std::fs::write(p.ours.join(".git/refs/heads/main.lock"), "").expect("lock");
    let r = p.ours("", &["commit", "-m", "x"]).await;
    assert_eq!(r.code, 4, "{}", r.err);
    assert!(r.err.contains("refs/heads/main.lock"), "{}", r.err);
    assert!(p.ours.join(".git/refs/heads/main.lock").exists());
    // HEAD's own lock, and packed-refs's.
    for lock in ["HEAD.lock", "packed-refs.lock"] {
        let p = Pair::new(&staged_change);
        std::fs::write(p.ours.join(".git").join(lock), "").expect("lock");
        let before = support::Fingerprint::take(&p.ours.join(".git"));
        let r = p.ours("", &["commit", "-m", "x"]).await;
        assert_eq!(r.code, 4, "[{lock}] {}", r.err);
        assert!(r.err.contains(lock), "[{lock}] {}", r.err);
        assert!(support::Fingerprint::take(&p.ours.join(".git")).differences_from(&before).is_empty(), "[{lock}] left a mark");
    }
}

// ── the kernel path ────────────────────────────────────────────────────────

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn through_a_real_kernel_f_reads_the_message_from_the_vfs_and_variables_come_from_the_shell() {
    use kaish_kernel::{Kernel, KernelConfig};
    support::require_git();
    let fx = Fixture::empty();
    let root = fx.path("repo");
    std::fs::create_dir_all(&root).expect("dir");
    support::git(&root, &["init", "-q", "-b", "main"]);
    support::write_file(&root, "a", "a\n");
    support::git(&root, &["add", "a"]);

    let backend: Arc<dyn kaish_tool_api::KernelBackend> = Arc::new(StrictBackend::single(PathBuf::from("/mnt"), fx.root()));
    let git = kaish_tools_git::tool(GitConfig::read_only().with_commit_profile()).expect("config");
    let mut cfg = KernelConfig::transient()
        .with_var("GIT_AUTHOR_NAME", Value::String("Kernel Author".into()))
        .with_var("GIT_AUTHOR_EMAIL", Value::String("kernel@example.org".into()))
        .with_var("GIT_AUTHOR_DATE", Value::String("2026-08-01T10:00:00+00:00".into()))
        .with_var("GIT_COMMITTER_DATE", Value::String("2026-08-01T10:00:00+00:00".into()));
    cfg.cwd = PathBuf::from("/mnt/repo");
    let kernel = Kernel::with_backend(backend, cfg, |_| {}, |tools| tools.register(git)).expect("kernel");
    let w = kernel.execute("printf 'from a file  \\n\\n\\n\\nsecond\\n' > /v/blobs/msg.txt").await.expect("write");
    assert_eq!(w.code, 0, "{}", w.err);
    let r = kernel.execute("git commit -F /v/blobs/msg.txt").await.expect("commit");
    assert_eq!(r.code, 0, "{}", r.err);
    assert_eq!(git_raw(&root, &["log", "-1", "--format=%B"]).1.trim_end_matches('\n'), "from a file\n\nsecond");
    let commit = git_raw(&root, &["cat-file", "-p", "HEAD"]).1;
    assert!(commit.contains("author Kernel Author <kernel@example.org> 1785578400 +0000"), "{commit}");
    // The committer falls back to the author's name and email.
    assert!(commit.contains("committer Kernel Author <kernel@example.org> 1785578400 +0000"), "{commit}");
    let missing = kernel.execute("git commit -F /v/blobs/nope.txt").await.expect("commit");
    assert_eq!(missing.code, 2, "{}", missing.err);
    assert!(missing.err.contains("could not be read"), "{}", missing.err);
}

// ── non-UTF-8 paths survive every commit spelling (review T5) ──────────────

use std::ffi::OsStr;
use std::os::unix::ffi::OsStrExt;

fn tree_z(repo: &Path) -> Vec<u8> {
    let out = Command::new("git")
        .args(["ls-tree", "-r", "-z", "HEAD"])
        .current_dir(repo)
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_SYSTEM", "/dev/null")
        .output()
        .expect("run git");
    assert!(out.status.success());
    out.stdout
}

fn write_bytes_path(root: &Path, rel: &[u8], contents: &str) {
    let path = root.join(OsStr::from_bytes(rel));
    std::fs::create_dir_all(path.parent().expect("parent")).expect("mkdir");
    std::fs::write(path, contents).expect("write");
}

const LATIN1: &[u8] = b"caf\xe9.txt";
const LATIN1_IN_D: &[u8] = b"d/caf\xe9.txt";

fn base_with_latin1(dir: &Path) {
    base(dir);
    write_bytes_path(dir, LATIN1, "latin one\n");
    write_bytes_path(dir, LATIN1_IN_D, "latin two\n");
    support::git(dir, &["add", "-A"]);
    support::git(dir, &["commit", "-qm", "latin1"]);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_commit_keeps_a_non_utf8_entry_byte_for_byte() {
    type Setup = fn(&Path);
    let cases: Vec<(&str, Setup, Vec<&str>)> = vec![
        ("plain", |d| {
            base_with_latin1(d);
            support::write_file(d, "a.txt", "alpha two\n");
            support::git(d, &["add", "a.txt"]);
        }, vec!["-m", "x"]),
        ("-a edits", |d| {
            base_with_latin1(d);
            write_bytes_path(d, LATIN1, "latin edited\n");
            write_bytes_path(d, LATIN1_IN_D, "latin two edited\n");
        }, vec!["-a", "-m", "x"]),
        ("-a deletion", |d| {
            base_with_latin1(d);
            std::fs::remove_file(d.join(OsStr::from_bytes(LATIN1_IN_D))).expect("rm");
        }, vec!["-a", "-m", "x"]),
        ("--only glob", |d| {
            base_with_latin1(d);
            write_bytes_path(d, LATIN1, "latin edited\n");
            write_bytes_path(d, LATIN1_IN_D, "latin two edited\n");
            support::write_file(d, "a.txt", "alpha two\n");
        }, vec!["-m", "x", "*af?.txt"]),
    ];
    for (label, setup, args) in cases {
        let p = Pair::new(&setup);
        let mut ours = vec!["commit"];
        ours.extend(&args);
        let r = p.ours("", &ours).await;
        assert_eq!(r.code, 0, "[{label}] {}", r.err);
        let (code, _, err) = p.twin(&ours);
        assert_eq!(code, 0, "[{label}] git: {err}");
        let (a, b) = (tree_z(&p.ours), tree_z(&p.twin));
        assert!(a == b, "[{label}] ls-tree -r -z HEAD differs\nours: {:?}\ntwin: {:?}", String::from_utf8_lossy(&a), String::from_utf8_lossy(&b));
        p.assert_same(label);
    }
}

// ── an identity cannot forge a header line (review T6) ─────────────────────

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_author_flag_with_a_line_break_or_bracket_is_refused_pinned_or_not() {
    let hostile = [
        "A\ncommitter Evil <e@e>",
        "A <e\n@e>",
        "A\rB <e@e>",
        "A> <e@e>",
        "A <e@e\ncommitter Evil <x@x>>",
    ];
    let pinned = GitConfig::read_only().with_commit_profile().with_identity(
        Identity::new("Pinned Author", "pa@example.org").expect("identity"),
        Identity::new("Pinned Committer", "pc@example.org").expect("identity"),
    );
    let plain = GitConfig::read_only().with_commit_profile();
    for (label, config) in [("pinned", pinned), ("unpinned", plain)] {
        for author in hostile {
            let p = Pair::new(&|d| {
                base(d);
                support::write_file(d, "a.txt", "changed\n");
                support::git(d, &["add", "a.txt"]);
            });
            let fp = support::Fingerprint::take(&p.ours);
            let r = run_tool_with(&p.fx.root(), "/mnt/ours", &["commit", "-m", "x", "--author", author], &default_vars(), config.clone()).await;
            let after = git_raw(&p.ours, &["cat-file", "-p", "HEAD"]).1;
            let committers = after.lines().take_while(|l| !l.is_empty()).filter(|l| l.starts_with("committer ")).count();
            assert_eq!(committers, 1, "[{label}] {author:?}: a header line was forged:\n{after}");
            assert_eq!(r.code, 2, "[{label}] {author:?}: {}", r.err);
            assert!(support::Fingerprint::take(&p.ours).differences_from(&fp).is_empty(), "[{label}] {author:?}: a refusal changed the repository");
            assert!(git_raw(&p.ours, &["fsck", "--strict"]).0 == 0);
        }
    }
}

// ── the reflog message is git's copy_reflog_msg (review T4) ────────────────

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn reflog_messages_collapse_whitespace_as_git_does() {
    for (label, message) in [
        ("carriage return", "sub\rject"),
        ("tabs", "sub\tject\twith\t\ttabs"),
        ("runs of spaces", "a    b  c"),
        ("form feed and vertical tab are not whitespace to git", "a\u{b}b\u{c}c"),
        ("two lines", "first line\nsecond line"),
        ("trailing carriage return", "subject\r"),
        ("leading tab", "\tindented subject"),
        ("many", "a \t\r b"),
    ] {
        let p = Pair::new(&|d| {
            base(d);
            support::write_file(d, "a.txt", "changed\n");
            support::git(d, &["add", "a.txt"]);
        });
        let r = p.ours("", &["commit", "-m", message]).await;
        assert_eq!(r.code, 0, "[{label}] {}", r.err);
        let (code, _, err) = p.twin(&["commit", "-m", message]);
        assert_eq!(code, 0, "[{label}] git: {err}");
        for log in ["logs/HEAD", "logs/refs/heads/main"] {
            let (a, b) = (std::fs::read(p.ours.join(".git").join(log)).expect("log"), std::fs::read(p.twin.join(".git").join(log)).expect("log"));
            assert!(a == b, "[{label}] {log} differs\nours: {:?}\ntwin: {:?}", String::from_utf8_lossy(&a), String::from_utf8_lossy(&b));
        }
        p.assert_same(label);
    }
}

// ── what a refusal says it wrote is what it wrote (review T1-T3) ───────────

/// Every file under `.git` except the object database, by path, with bytes.
fn state_outside_objects(repo: &Path) -> std::collections::BTreeMap<String, Vec<u8>> {
    fn walk(root: &Path, dir: &Path, out: &mut std::collections::BTreeMap<String, Vec<u8>>) {
        for e in std::fs::read_dir(dir).expect("read_dir") {
            let p = e.expect("entry").path();
            let rel = p.strip_prefix(root).expect("under").display().to_string();
            if rel == "objects" {
                continue;
            }
            if p.is_dir() {
                walk(root, &p, out);
            } else {
                out.insert(rel, std::fs::read(&p).unwrap_or_default());
            }
        }
    }
    let mut out = std::collections::BTreeMap::new();
    let git = repo.join(".git");
    walk(&git, &git, &mut out);
    out
}

fn loose_objects(repo: &Path) -> Vec<String> {
    let mut out = Vec::new();
    let root = repo.join(".git/objects");
    for e in std::fs::read_dir(&root).expect("objects") {
        let p = e.expect("entry").path();
        if p.is_dir() && p.file_name().is_some_and(|n| n.len() == 2) {
            for f in std::fs::read_dir(&p).expect("fan-out") {
                out.push(format!("{}/{}", p.file_name().expect("n").to_string_lossy(), f.expect("f").file_name().to_string_lossy()));
            }
        }
    }
    out.sort();
    out
}

/// A refusal after the commit's objects exist says so, and the claim holds:
/// objects were written, and no ref, reflog, index or HEAD changed.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_refusal_after_objects_were_written_does_not_say_nothing_changed() {
    // pre-commit rejection (the hook is handed the tree id, so the tree exists).
    let p = Pair::new(&|d| {
        base(d);
        support::write_file(d, "a.txt", "tracked change\n");
    });
    let before_state = state_outside_objects(&p.ours);
    let before_objects = loose_objects(&p.ours);
    let rec = Arc::new(Recorder { reject_pre_commit: true, ..Recorder::default() });
    let config = GitConfig::read_only().with_commit_profile().with_hooks(rec);
    let r = run_tool_with(&p.fx.root(), "/mnt/ours", &["commit", "-am", "x"], &default_vars(), config).await;
    assert_eq!(r.code, 1, "{}", r.err);
    assert!(!r.err.contains("Nothing was changed"), "it wrote a tree: {}", r.err);
    assert!(r.err.contains("no ref or index changed") && r.err.contains("git gc prunes"), "{}", r.err);
    assert!(state_outside_objects(&p.ours) == before_state, "a ref, reflog, the index or HEAD changed");
    assert!(loose_objects(&p.ours).len() > before_objects.len(), "the claim is that objects were written, and none were");

    // A lost compare-and-swap.
    let p = Pair::new(&staged_change);
    let rec = Arc::new(Recorder { race: Some(p.ours.clone()), ..Recorder::default() });
    let config = GitConfig::read_only().with_commit_profile().with_hooks(rec);
    let r = run_tool_with(&p.fx.root(), "/mnt/ours", &["commit", "-m", "x"], &default_vars(), config).await;
    assert_eq!(r.code, 1, "{}", r.err);
    assert!(!r.err.contains("Nothing was changed"), "{}", r.err);
    assert!(r.err.contains("no ref or index changed") && r.err.contains("git gc prunes"), "{}", r.err);
    assert!(!r.err.contains("The commit object is on disk"), "the sentence is now the Wrote property: {}", r.err);

    // The same three on the amend paths: the amended commit's tree is written
    // before the hook runs and before the compare-and-swap, so each claims the
    // objects and no more.
    for (label, rec, argv) in [
        ("amend, pre-commit rejection", Recorder { reject_pre_commit: true, ..Recorder::default() }, vec!["commit", "--amend", "-am", "x"]),
        ("amend, lost swap", Recorder { race: Some(PathBuf::new()), ..Recorder::default() }, vec!["commit", "--amend", "-m", "x"]),
    ] {
        let p = Pair::new(&|d| {
            base(d);
            support::write_file(d, "a.txt", "tracked change\n");
        });
        let mut rec = rec;
        if rec.race.is_some() {
            rec.race = Some(p.ours.clone());
        }
        let before_state = state_outside_objects(&p.ours);
        let before_objects = loose_objects(&p.ours);
        let config = GitConfig::read_only().with_commit_profile().with_hooks(Arc::new(rec));
        let r = run_tool_with(&p.fx.root(), "/mnt/ours", &argv, &default_vars(), config).await;
        assert_eq!(r.code, 1, "[{label}] {}", r.err);
        assert!(!r.err.contains("Nothing was changed"), "[{label}] it wrote a tree: {}", r.err);
        assert!(r.err.contains("no ref or index changed") && r.err.contains("git gc prunes"), "[{label}] {}", r.err);
        if label.ends_with("rejection") {
            assert!(state_outside_objects(&p.ours) == before_state, "[{label}] a ref, reflog, the index or HEAD changed");
        }
        assert!(loose_objects(&p.ours).len() > before_objects.len(), "[{label}] the claim is that objects were written, and none were");
    }
}

/// The refusals the tree writer raises come before any blob is written, so
/// they keep the plain claim, held to the bytes: `.git` is identical, objects
/// included.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_tree_refusal_in_commit_a_comes_before_the_first_blob() {
    let p = Pair::new(&|d| {
        base(d);
        // An entry the index trusts (assume-unchanged) whose blob is missing.
        support::git(d, &["update-index", "--add", "--info-only", "--cacheinfo", "100644,1111111111111111111111111111111111111111,ghost.txt"]);
        support::git(d, &["update-index", "--assume-unchanged", "ghost.txt"]);
        support::write_file(d, "ghost.txt", "ghost\n");
        support::write_file(d, "a.txt", "alpha changed\n");
    });
    let fp = support::Fingerprint::take(&p.ours);
    let r = p.ours("", &["commit", "-a", "-m", "x"]).await;
    assert_eq!(r.code, 4, "{}", r.err);
    assert!(r.err.contains("ghost.txt") && r.err.contains("does not hold"), "{}", r.err);
    assert!(r.err.trim_end().ends_with("Nothing was changed."), "{}", r.err);
    let diffs = support::Fingerprint::take(&p.ours).differences_from(&fp);
    assert!(diffs.is_empty(), "the refusal claims nothing changed: {diffs:?}");
}

/// Hook rejection, lost swap and tree refusal in one place for the claim in
/// docs/embedding-git.md: a refusal that says "Nothing was changed." leaves
/// `.git` byte-identical.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn every_nothing_was_changed_refusal_of_commit_leaves_git_byte_identical() {
    for (label, setup, argv) in [
        ("empty message", staged_change as fn(&Path), vec!["commit", "-m", ""]),
        ("nothing to commit", base, vec!["commit", "-m", "x"]),
        ("--only unknown path", staged_change, vec!["commit", "-m", "x", "nomatch"]),
        ("author with a line break", staged_change, vec!["commit", "-m", "x", "--author", "A\nB <a@b.c>"]),
    ] {
        let p = Pair::new(&setup);
        let fp = support::Fingerprint::take(&p.ours);
        let r = p.ours("", &argv).await;
        assert_ne!(r.code, 0, "[{label}] {}", r.err);
        assert!(r.err.trim_end().ends_with("Nothing was changed."), "[{label}] {}", r.err);
        let diffs = support::Fingerprint::take(&p.ours).differences_from(&fp);
        assert!(diffs.is_empty(), "[{label}] {diffs:?}");
    }
}

// ── pins from the second review (GLM) ──────────────────────────────────────

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_planted_link_at_a_later_blob_stops_commit_a_before_any_blob_is_written() {
    let first = (0..).map(|n| format!("first {n}\n")).find(|c| blob_hex(c)[..2] != blob_hex("second\n")[..2]).expect("content");
    let p = Pair::new(&|d| {
        base(d);
    });
    support::write_file(&p.ours, "a.txt", &first);
    support::write_file(&p.ours, "b.txt", "second\n");
    let (first_hex, second_hex) = (blob_hex(&first), blob_hex("second\n"));
    let outside = p.fx.path("elsewhere-7f3a");
    std::fs::create_dir_all(&outside).expect("outside");
    std::os::unix::fs::symlink(&outside, p.ours.join(format!(".git/objects/{}", &second_hex[..2]))).expect("plant");
    let r = p.ours("", &["commit", "-a", "-m", "x"]).await;
    assert_eq!(r.code, 4, "{}", r.err);
    assert!(r.err.contains("nothing was written"), "{}", r.err);
    assert!(!p.ours.join(format!(".git/objects/{}/{}", &first_hex[..2], &first_hex[2..])).exists(), "the first blob was written before the second was vetted");
    assert_eq!(std::fs::read_dir(&outside).expect("outside").count(), 0, "wrote through the link");
}

fn blob_hex(content: &str) -> String {
    use std::io::Write as _;
    let mut child = Command::new("git")
        .args(["hash-object", "--stdin"])
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .spawn()
        .expect("git");
    child.stdin.take().expect("stdin").write_all(content.as_bytes()).expect("write");
    String::from_utf8(child.wait_with_output().expect("wait").stdout).expect("utf8").trim().to_string()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_branch_ref_that_is_a_link_out_of_the_repository_refuses_the_commit() {
    let p = Pair::new(&|d| {
        staged_change(d);
        support::git(d, &["branch", "feat"]);
        support::git(d, &["checkout", "-q", "feat"]);
        support::git(d, &["checkout", "-q", "main"]);
        support::git(d, &["symbolic-ref", "HEAD", "refs/heads/feat"]);
    });
    let outside = p.fx.path("elsewhere-feat");
    std::fs::write(&outside, git_raw(&p.ours, &["rev-parse", "main"]).1).expect("outside ref");
    let feat = p.ours.join(".git/refs/heads/feat");
    std::fs::remove_file(&feat).expect("rm");
    std::os::unix::fs::symlink(&outside, &feat).expect("plant");
    let before = std::fs::read(&outside).expect("outside");
    let r = p.ours("", &["commit", "-m", "x"]).await;
    assert_eq!(r.code, 4, "{}", r.err);
    assert!(!r.err.contains("elsewhere-feat"), "named the link target: {}", r.err);
    assert_eq!(std::fs::read(&outside).expect("outside"), before, "wrote through the link");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_first_commit_on_an_unborn_nested_branch_matches_git_byte_for_byte() {
    let p = Pair::new(&|d| {
        support::git(d, &["symbolic-ref", "HEAD", "refs/heads/feat/x"]);
        support::write_file(d, "a", "a\n");
        support::git(d, &["add", "a"]);
    });
    assert_eq!(p.ours("", &["commit", "-m", "first"]).await.code, 0);
    assert_eq!(p.twin(&["commit", "-q", "-m", "first"]).0, 0);
    assert_eq!(git_raw(&p.ours, &["rev-parse", "HEAD"]).1, git_raw(&p.twin, &["rev-parse", "HEAD"]).1);
    for log in ["logs/HEAD", "logs/refs/heads/feat/x"] {
        assert_eq!(read_log(&p.ours, log), read_log(&p.twin, log), "{log}");
    }
    assert_eq!(std::fs::read(p.ours.join(".git/refs/heads/feat/x")).expect("ref"), std::fs::read(p.twin.join(".git/refs/heads/feat/x")).expect("ref"));
    assert_eq!(git_raw(&p.ours, &["fsck", "--strict"]).0, 0);
}

// ── pins from the first review ─────────────────────────────────────────────

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_plain_commit_does_not_rewrite_the_index() {
    let p = Pair::new(&staged_change);
    let before = std::fs::read(p.ours.join(".git/index")).expect("index");
    let mtime = std::fs::metadata(p.ours.join(".git/index")).expect("meta").modified().expect("mtime");
    assert_eq!(p.ours("", &["commit", "-m", "x"]).await.code, 0);
    assert!(std::fs::read(p.ours.join(".git/index")).expect("index") == before, "a plain commit rewrote the index");
    assert_eq!(std::fs::metadata(p.ours.join(".git/index")).expect("meta").modified().expect("mtime"), mtime);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn allow_empty_with_a_parent_matches_git() {
    let p = Pair::new(&base);
    assert_eq!(p.ours("", &["commit", "--allow-empty", "-m", "again"]).await.code, 0);
    assert_eq!(p.twin(&["commit", "-q", "--allow-empty", "-m", "again"]).0, 0);
    p.assert_same("--allow-empty with a parent");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_commit_on_a_packed_branch_leaves_packed_refs_alone_and_matches_git() {
    let setup = |d: &Path| {
        staged_change(d);
        support::git(d, &["pack-refs", "--all"]);
    };
    let p = Pair::new(&setup);
    assert!(p.ours.join(".git/packed-refs").exists());
    assert!(!p.ours.join(".git/refs/heads/main").exists(), "the branch must be packed only");
    let packed_before = std::fs::read(p.ours.join(".git/packed-refs")).expect("packed");
    let r = p.ours("", &["commit", "-m", "x"]).await;
    assert_eq!(r.code, 0, "{}", r.err);
    assert_eq!(p.twin(&["commit", "-q", "-m", "x"]).0, 0);
    // The new value is a loose ref, as git writes it; the packed entry is the
    // old one in git too, and the two repositories' files are byte-equal.
    assert_eq!(std::fs::read(p.ours.join(".git/packed-refs")).expect("packed"), packed_before);
    assert!(std::fs::read(p.ours.join(".git/packed-refs")).expect("p") == std::fs::read(p.twin.join(".git/packed-refs")).expect("p"), "packed-refs differs from git's");
    assert_eq!(std::fs::read(p.ours.join(".git/refs/heads/main")).expect("loose"), std::fs::read(p.twin.join(".git/refs/heads/main")).expect("loose"));
    assert_eq!(git_raw(&p.ours, &["show-ref"]).1, git_raw(&p.twin, &["show-ref"]).1);
    p.assert_same("packed branch");
}

// ── a gitlink replaced by a file (review T9) ───────────────────────────────

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_gitlink_replaced_by_a_regular_file_is_committed_as_a_file() {
    let setup = |dir: &Path| {
        base(dir);
        support::git(dir, &["init", "-q", "-b", "main", "inner"]);
        let inner = dir.join("inner");
        support::write_file(&inner, "z.txt", "z\n");
        support::git(&inner, &["add", "."]);
        support::git(&inner, &["commit", "-qm", "inner one"]);
        support::git(dir, &["add", "inner"]);
        support::git(dir, &["commit", "-qm", "add inner"]);
        std::fs::remove_dir_all(&inner).expect("rm inner");
        support::write_file(dir, "inner", "now a plain file\n");
    };
    for args in [vec!["commit", "-a", "-m", "x"], vec!["commit", "-m", "x", "inner"]] {
        let p = Pair::new(&setup);
        let r = p.ours("", &args).await;
        assert_eq!(r.code, 0, "{args:?}: {}", r.err);
        let (code, _, err) = p.twin(&args);
        assert_eq!(code, 0, "{args:?}: {err}");
        assert!(git_raw(&p.ours, &["ls-tree", "HEAD", "inner"]).1.starts_with("100644 blob"), "{args:?}");
        p.assert_same(&args.join(" "));
    }
}

// ── git's binary rule looks at the first 8000 bytes only (review T10) ──────

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_nul_after_the_first_8000_bytes_does_not_make_a_file_binary() {
    let mut late = "line\n".repeat(1700).into_bytes(); // 8500 bytes
    late.extend_from_slice(b"after\0nul\nmore\n");
    let mut early = b"nul\0here\n".to_vec();
    early.extend_from_slice("line\n".repeat(1700).as_bytes());
    for (label, bytes) in [("NUL after 8000", late), ("NUL in the first bytes", early)] {
        let bytes2 = bytes.clone();
        let setup = move |d: &Path| {
            base(d);
            std::fs::write(d.join("data.bin"), &bytes2).expect("write");
            support::git(d, &["add", "data.bin"]);
        };
        let p = Pair::new(&setup);
        let r = p.ours("", &["commit", "-m", "x"]).await;
        assert_eq!(r.code, 0, "[{label}] {}", r.err);
        let (code, gout, err) = p.twin(&["commit", "-m", "x"]);
        assert_eq!(code, 0, "[{label}] {err}");
        assert_eq!(r.text_out(), gout, "[{label}] the summary differs from git's");
    }
}

// ── the --only refusal does not claim a thing this shell does (review T13) ──

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn committing_an_untracked_path_points_at_git_add_and_does_not_say_never() {
    let p = Pair::new(&|d| {
        staged_change(d);
        support::write_file(d, "u.txt", "untracked\n");
    });
    let r = p.ours("", &["commit", "-m", "x", "u.txt"]).await;
    assert_eq!(r.code, 1, "{}", r.err);
    assert!(r.err.contains("git add u.txt"), "{}", r.err);
    assert!(!r.err.contains("never does it"), "git add then commit does exactly this: {}", r.err);
    // And that route works.
    assert_eq!(p.ours("", &["add", "u.txt"]).await.code, 0);
    assert_eq!(p.ours("", &["commit", "-m", "x", "u.txt"]).await.code, 0);
}

// ── core.hooksPath is resolved through the lstat walk (review T15) ─────────

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_hooks_path_through_a_link_refuses_naming_the_setting_and_reads_nothing() {
    use std::os::unix::fs::PermissionsExt;
    for (label, hooks_path, plant) in [
        ("linked directory", "link/hooks", "dir"),
        ("linked leaf", "hooks-link", "leaf"),
        ("leaves the tree", "../elsewhere", "none"),
        ("absolute", "/nonexistent-hooks-dir", "none"),
    ] {
        let p = Pair::new(&staged_change);
        let outside = p.fx.path("outside-hooks");
        std::fs::create_dir_all(outside.join("hooks")).expect("outside");
        // A hook outside, which would be read if the link were followed.
        let hook = outside.join("hooks/pre-commit");
        std::fs::write(&hook, "#!/bin/sh\nexit 1\n").expect("hook");
        std::fs::set_permissions(&hook, std::fs::Permissions::from_mode(0o755)).expect("chmod");
        match plant {
            "dir" => std::os::unix::fs::symlink(&outside, p.ours.join("link")).expect("link"),
            "leaf" => std::os::unix::fs::symlink(outside.join("hooks"), p.ours.join("hooks-link")).expect("link"),
            _ => {}
        }
        support::git(&p.ours, &["config", "core.hooksPath", hooks_path]);
        let fp = support::Fingerprint::take(&p.ours.join(".git"));
        let r = p.ours("", &["commit", "-m", "x"]).await;
        assert_eq!(r.code, 4, "[{label}] {}", r.err);
        assert!(r.err.contains("core.hooksPath"), "[{label}] {}", r.err);
        assert!(!r.err.contains("pre-commit hook"), "[{label}] read the hook through the link: {}", r.err);
        assert!(!r.err.contains("outside-hooks"), "[{label}] named the target: {}", r.err);
        assert!(support::Fingerprint::take(&p.ours.join(".git")).differences_from(&fp).is_empty(), "[{label}]");
    }
    // Negative controls: a real directory inside the tree, and one that does not exist.
    for hooks_path in ["githooks", "missing/dir"] {
        let p = Pair::new(&staged_change);
        std::fs::create_dir_all(p.ours.join("githooks")).expect("dir");
        support::git(&p.ours, &["config", "core.hooksPath", hooks_path]);
        let r = p.ours("", &["commit", "-m", "x"]).await;
        assert_eq!(r.code, 0, "[{hooks_path}] {}", r.err);
    }
}
