//! Phase 1b: the index verbs (`add`, `rm`, `mv`, `restore --staged`, `reset`)
//! against real git as the oracle.
//!
//! Every test builds a TWIN: two repositories made by the same commands, so
//! their object ids agree. Real git runs a command in the twin, this crate
//! runs the same command in the other, and the two are compared on:
//!
//! - `git ls-files -s --debug`: path, mode, id, stage, and the debug `size`
//!   and `flags` lines. The `ctime`, `mtime`, `dev`, `ino`, `uid` and `gid`
//!   lines are excluded because they come from the filesystem and differ
//!   between two directories by construction. `size` stays because git zeroes
//!   it for entries it restores from a tree and for racy entries, so a
//!   difference there is a behavior difference. Entries a verb leaves alone
//!   are separately asserted to keep their stat byte for byte.
//! - `git status --porcelain=v2`.
//! - `git fsck --strict` on ours.

#![cfg(feature = "commit")]

#[path = "support.rs"]
mod support;

use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Arc;

use kaish_tool_api::Tool;
use kaish_tools_git::GitConfig;

use support::{Fixture, StrictBackend, TestCtx};

/// Run real git, hermetically, returning (exit code, stdout, stderr).
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
        .env("GIT_AUTHOR_DATE", "2026-08-01T10:00:00+00:00")
        .env("GIT_COMMITTER_DATE", "2026-08-01T10:00:00+00:00")
        .output()
        .expect("run git");
    (
        out.status.code().unwrap_or(-1),
        String::from_utf8_lossy(&out.stdout).into_owned(),
        String::from_utf8_lossy(&out.stderr).into_owned(),
    )
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

fn status_v2(repo: &Path) -> String {
    support::git(repo, &["status", "--porcelain=v2", "--untracked-files=all"])
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

    /// Run `git <argv>` through this crate in the `ours` repository.
    pub async fn ours(&self, cwd: &str, argv: &[&str]) -> kaish_types::ExecResult {
        run_tool(&self.fx.root(), &format!("/mnt/ours/{cwd}"), argv).await
    }

    /// Run real git in the `twin`.
    pub fn twin(&self, args: &[&str]) -> (i32, String, String) {
        git_raw(&self.twin, args)
    }

    /// Compare index, status and fsck.
    pub fn assert_same(&self, label: &str) {
        assert_eq!(index_view(&self.ours), index_view(&self.twin), "[{label}] ls-files -s --debug differs\nours:\n{}\ntwin:\n{}",
            index_view(&self.ours).join("\n"), index_view(&self.twin).join("\n"));
        assert_eq!(status_v2(&self.ours), status_v2(&self.twin), "[{label}] status differs");
        let (code, _, err) = git_raw(&self.ours, &["fsck", "--strict"]);
        assert_eq!(code, 0, "[{label}] fsck --strict failed on ours: {err}");
        assert_eq!(tree_listing(&self.ours), tree_listing(&self.twin), "[{label}] working trees differ");
        let leftovers = lockfiles(&self.ours.join(".git"));
        assert!(leftovers.is_empty(), "[{label}] stray lock or temp files: {leftovers:?}");
    }
}

/// Every path in the working tree outside `.git`, directories included.
fn tree_listing(root: &Path) -> Vec<String> {
    fn walk(root: &Path, dir: &Path, out: &mut Vec<String>) {
        for e in std::fs::read_dir(dir).expect("read_dir") {
            let e = e.expect("entry");
            let p = e.path();
            if p.file_name().is_some_and(|n| n == ".git") {
                continue;
            }
            let m = std::fs::symlink_metadata(&p).expect("lstat");
            let rel = p.strip_prefix(root).expect("under root").display().to_string();
            out.push(if m.is_dir() { format!("{rel}/") } else { rel });
            if m.is_dir() {
                walk(root, &p, out);
            }
        }
    }
    let mut out = Vec::new();
    walk(root, root, &mut out);
    out.sort();
    out
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

pub async fn run_tool(root: &Path, cwd: &str, argv: &[&str]) -> kaish_types::ExecResult {
    let backend = Arc::new(StrictBackend::single(PathBuf::from("/mnt"), root));
    let mut ctx = TestCtx::new(backend, cwd);
    // A plain `reset` writes ORIG_HEAD and a reflog line, so it needs a
    // committer (W30). The other verbs here ignore it.
    for (k, v) in [("GIT_COMMITTER_NAME", "Fixture Committer"), ("GIT_COMMITTER_EMAIL", "committer@example.invalid")] {
        kaish_tool_api::ToolCtx::set_var(&mut ctx, k, kaish_types::Value::String(v.to_string()));
    }
    let git = kaish_tools_git::tool(GitConfig::read_only().with_commit_profile()).expect("config builds");
    let (verb, rest) = argv.split_first().expect("a verb");
    git.execute(support::verb_words(verb, rest), &mut ctx).await
}

/// `git add`-ing is used for the base commit because the fixture is built by
/// real git; the verbs under test never build it.
fn base(dir: &Path) {
    support::write_file(dir, "a.txt", "alpha\n");
    support::write_file(dir, "b.txt", "bravo\n");
    support::write_file(dir, "d/c.txt", "charlie\n");
    support::write_file(dir, "d/e/f.txt", "foxtrot\n");
    support::git(dir, &["add", "."]);
    support::git(dir, &["commit", "-qm", "base"]);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn add_paths_matches_git() {
    let edit = |dir: &Path| {
        base(dir);
        support::write_file(dir, "a.txt", "alpha two\n");
        support::write_file(dir, "new.txt", "new\n");
        support::write_file(dir, "d/new2.txt", "new2\n");
    };
    let p = Pair::new(&edit);
    let r = p.ours("", &["add", "a.txt", "new.txt"]).await;
    assert_eq!(r.code, 0, "{}", r.err);
    assert_eq!(p.twin(&["add", "a.txt", "new.txt"]).0, 0);
    p.assert_same("add paths");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn add_all_update_and_dot_match_git() {
    for (name, ours_args, twin_args, cwd) in [
        ("-A", vec!["add", "-A"], vec!["add", "-A"], ""),
        ("--all", vec!["add", "--all"], vec!["add", "--all"], ""),
        ("-u", vec!["add", "-u"], vec!["add", "-u"], ""),
        (".", vec!["add", "."], vec!["add", "."], ""),
        ("dir .", vec!["add", "."], vec!["add", "."], "d"),
        ("dir name", vec!["add", "d"], vec!["add", "d"], ""),
        ("glob", vec!["add", "d/*.txt"], vec!["add", "d/*.txt"], ""),
    ] {
        let setup = |dir: &Path| {
            base(dir);
            support::write_file(dir, "a.txt", "alpha two\n");
            support::write_file(dir, "new.txt", "new\n");
            support::write_file(dir, "d/new2.txt", "new2\n");
            support::write_file(dir, "d/e/new3.txt", "new3\n");
            std::fs::remove_file(dir.join("b.txt")).expect("rm b");
            std::fs::remove_file(dir.join("d/c.txt")).expect("rm c");
        };
        let p = Pair::new(&setup);
        let r = p.ours(cwd, &ours_args).await;
        assert_eq!(r.code, 0, "[{name}] {}", r.err);
        let twin_dir = if cwd.is_empty() { p.twin.clone() } else { p.twin.join(cwd) };
        assert_eq!(git_raw(&twin_dir, &twin_args).0, 0, "[{name}]");
        p.assert_same(name);
    }
}

#[cfg(unix)]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn add_stores_exec_bit_and_symlinks_as_git_does() {
    use std::os::unix::fs::PermissionsExt;
    let setup = |dir: &Path| {
        base(dir);
        support::write_file(dir, "run.sh", "#!/bin/sh\n");
        std::fs::set_permissions(dir.join("run.sh"), std::fs::Permissions::from_mode(0o755)).expect("chmod");
        std::os::unix::fs::symlink("a.txt", dir.join("link")).expect("symlink");
        // A link whose target does not exist, and one that leaves the repo.
        std::os::unix::fs::symlink("nowhere", dir.join("dangling")).expect("symlink");
        std::os::unix::fs::symlink("/etc/passwd", dir.join("abs")).expect("symlink");
    };
    let p = Pair::new(&setup);
    let r = p.ours("", &["add", "-A"]).await;
    assert_eq!(r.code, 0, "{}", r.err);
    assert_eq!(p.twin(&["add", "-A"]).0, 0);
    p.assert_same("exec and symlinks");
    let view = index_view(&p.ours).join("\n");
    assert!(view.contains("100755"), "{view}");
    assert!(view.contains("120000"), "{view}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn add_replaces_files_with_directories_and_back() {
    let setup = |dir: &Path| {
        base(dir);
        // `a.txt` becomes a directory; `d` (a directory) becomes a file.
        std::fs::remove_file(dir.join("a.txt")).expect("rm");
        support::write_file(dir, "a.txt/inner.txt", "inner\n");
        std::fs::remove_dir_all(dir.join("d")).expect("rm");
        support::write_file(dir, "d", "now a file\n");
    };
    let p = Pair::new(&setup);
    let r = p.ours("", &["add", "-A"]).await;
    assert_eq!(r.code, 0, "{}", r.err);
    assert_eq!(p.twin(&["add", "-A"]).0, 0);
    p.assert_same("D/F replacement");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn add_with_nothing_specified_says_so_and_changes_nothing() {
    let p = Pair::new(&|d| {
        base(d);
        support::write_file(d, "a.txt", "alpha two\n");
    });
    let before = std::fs::read(p.ours.join(".git/index")).expect("index");
    let r = p.ours("", &["add"]).await;
    assert_eq!(r.code, 0, "{}", r.err);
    assert!(r.text_out().contains("Nothing specified, nothing added."), "{}", r.text_out());
    assert_eq!(before, std::fs::read(p.ours.join(".git/index")).expect("index"));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn add_refuses_an_unmatched_pathspec_and_changes_nothing() {
    let p = Pair::new(&|d| {
        base(d);
        support::write_file(d, "a.txt", "alpha two\n");
    });
    let fp = support::Fingerprint::take(&p.fx.root());
    let r = p.ours("", &["add", "a.txt", "nomatch"]).await;
    assert_eq!(r.code, 1, "{}", r.err);
    assert!(r.err.contains("refused.") && r.err.contains("'nomatch' did not match"), "{}", r.err);
    assert!(r.err.trim_end().ends_with("Nothing was changed."), "{}", r.err);
    // Git agrees that this is an error and stages nothing.
    assert_eq!(p.twin(&["add", "a.txt", "nomatch"]).0, 128);
    assert!(support::Fingerprint::take(&p.fx.root()).differences_from(&fp).iter().all(|d| d.contains("twin")), "ours changed");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn add_of_an_ignored_path_refuses_with_git_s_hint_and_force_overrides() {
    let setup = |dir: &Path| {
        base(dir);
        support::write_file(dir, ".gitignore", "*.log\nbuild/\n");
        support::git(dir, &["add", ".gitignore"]);
        support::git(dir, &["commit", "-qm", "ignore"]);
        support::write_file(dir, "x.log", "log\n");
        support::write_file(dir, "d/y.log", "log\n");
        support::write_file(dir, "build/out.o", "obj\n");
        support::write_file(dir, "ok.txt", "ok\n");
    };
    let p = Pair::new(&setup);
    let fp = support::Fingerprint::take(&p.ours);
    let r = p.ours("", &["add", "x.log", "ok.txt"]).await;
    assert_eq!(r.code, 1, "{}", r.err);
    assert!(r.err.contains("ignored by one of your .gitignore files: x.log"), "{}", r.err);
    assert!(r.err.contains("git add -f x.log"), "{}", r.err);
    assert!(r.err.contains("Nothing was changed."), "{}", r.err);
    assert!(support::Fingerprint::take(&p.ours).differences_from(&fp).is_empty(), "a refusal changed the repository");

    // A path inside an ignored directory is ignored too.
    let r = p.ours("", &["add", "build/out.o"]).await;
    assert_eq!(r.code, 1, "{}", r.err);
    assert!(r.err.contains("build/out.o"), "{}", r.err);

    // Directories and -A skip ignored files silently, as git does.
    let r = p.ours("", &["add", "-A"]).await;
    assert_eq!(r.code, 0, "{}", r.err);
    assert_eq!(p.twin(&["add", "-A"]).0, 0);
    p.assert_same("add -A skips ignored");
    assert!(!index_view(&p.ours).join("\n").contains("x.log"));

    // -f stages them.
    let r = p.ours("", &["add", "-f", "x.log", "build/out.o"]).await;
    assert_eq!(r.code, 0, "{}", r.err);
    assert_eq!(p.twin(&["add", "-f", "x.log", "build/out.o"]).0, 0);
    p.assert_same("add -f");
    assert!(index_view(&p.ours).join("\n").contains("x.log"));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn add_p_and_i_refuse_with_all_five_parts() {
    let p = Pair::new(&|d| {
        base(d);
        support::write_file(d, "a.txt", "alpha two\n");
    });
    for flag in ["-p", "-i", "--patch", "--interactive"] {
        let fp = support::Fingerprint::take(&p.ours);
        let r = p.ours("", &["add", flag, "a.txt"]).await;
        assert_eq!(r.code, 2, "{flag}: {}", r.err);
        assert!(r.err.starts_with(&format!("git add {flag} a.txt: refused.")), "{}", r.err);
        assert!(r.err.contains("no flag that overrides") || r.err.contains("overrides this"), "{}", r.err);
        assert!(r.err.contains("git add <path>"), "{}", r.err);
        assert!(r.err.trim_end().ends_with("Nothing was changed."), "{}", r.err);
        assert!(support::Fingerprint::take(&p.ours).differences_from(&fp).is_empty());
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn add_refuses_a_nested_repository_that_is_not_a_gitlink() {
    let setup = |dir: &Path| {
        base(dir);
        support::git(dir, &["init", "-q", "-b", "main", "inner"]);
        support::write_file(&dir.join("inner"), "z.txt", "z\n");
    };
    let p = Pair::new(&setup);
    let fp = support::Fingerprint::take(&p.ours);
    for args in [vec!["add", "-A"], vec!["add", "."], vec!["add", "inner"]] {
        let r = p.ours("", &args).await;
        assert_eq!(r.code, 4, "{args:?}: {}", r.err);
        assert!(r.err.contains("'inner' is a git repository of its own"), "{}", r.err);
        assert!(r.err.trim_end().ends_with("Nothing was changed."), "{}", r.err);
    }
    assert!(support::Fingerprint::take(&p.ours).differences_from(&fp).is_empty());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn add_updates_an_existing_gitlink_to_the_nested_head() {
    let setup = |dir: &Path| {
        base(dir);
        support::git(dir, &["init", "-q", "-b", "main", "inner"]);
        let inner = dir.join("inner");
        support::write_file(&inner, "z.txt", "z\n");
        support::git(&inner, &["add", "."]);
        support::git(&inner, &["commit", "-qm", "inner one"]);
        support::git(dir, &["add", "inner"]);
        support::git(dir, &["commit", "-qm", "add inner"]);
        support::write_file(&inner, "z.txt", "z2\n");
        support::git(&inner, &["commit", "-qam", "inner two"]);
    };
    let p = Pair::new(&setup);
    let r = p.ours("", &["add", "-A"]).await;
    assert_eq!(r.code, 0, "{}", r.err);
    assert_eq!(p.twin(&["add", "-A"]).0, 0);
    p.assert_same("gitlink update");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn add_in_a_new_repository_with_no_index_matches_git() {
    let p = Pair::new(&|d| {
        support::write_file(d, "one.txt", "1\n");
        support::write_file(d, "sub/two.txt", "2\n");
    });
    let r = p.ours("", &["add", "-A"]).await;
    assert_eq!(r.code, 0, "{}", r.err);
    assert_eq!(p.twin(&["add", "-A"]).0, 0);
    p.assert_same("unborn add");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_stale_index_lock_refuses_and_is_not_removed() {
    let p = Pair::new(&|d| {
        base(d);
        support::write_file(d, "a.txt", "alpha two\n");
    });
    std::fs::write(p.ours.join(".git/index.lock"), "other process").expect("plant lock");
    let fp = support::Fingerprint::take(&p.ours);
    let r = p.ours("", &["add", "a.txt"]).await;
    assert_eq!(r.code, 4, "{}", r.err);
    assert!(r.err.contains("index.lock already exists"), "{}", r.err);
    assert!(r.err.trim_end().ends_with("Nothing was changed."), "{}", r.err);
    assert!(support::Fingerprint::take(&p.ours).differences_from(&fp).is_empty());
    assert_eq!(std::fs::read(p.ours.join(".git/index.lock")).expect("lock"), b"other process");
}


// ── rm ──────────────────────────────────────────────────────────────────────

/// Run `args` in both repositories after `setup`, and compare.
async fn both(setup: &dyn Fn(&Path), args: &[&str], label: &str) -> (kaish_types::ExecResult, (i32, String, String), Pair) {
    let p = Pair::new(setup);
    let r = p.ours("", args).await;
    let t = p.twin(args);
    let _ = label;
    (r, t, p)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn rm_matches_git_for_clean_files() {
    for (label, args) in [
        ("file", vec!["rm", "a.txt"]),
        ("two files", vec!["rm", "a.txt", "b.txt"]),
        ("cached", vec!["rm", "--cached", "a.txt"]),
        ("dir -r", vec!["rm", "-r", "d"]),
        ("dir -r --cached", vec!["rm", "-r", "--cached", "d"]),
        ("nested file empties parents", vec!["rm", "d/e/f.txt"]),
        ("glob", vec!["rm", "d/*.txt"]),
    ] {
        let (r, t, p) = both(&base, &args, label).await;
        assert_eq!(r.code, 0, "[{label}] {}", r.err);
        assert_eq!(t.0, 0, "[{label}] {}", t.2);
        p.assert_same(label);
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn rm_prints_what_git_prints() {
    let (r, t, _p) = both(&base, &["rm", "a.txt", "b.txt"], "out").await;
    assert_eq!(r.text_out(), t.1);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn rm_of_a_directory_needs_dash_r() {
    let (r, t, p) = both(&base, &["rm", "d"], "no -r").await;
    assert_eq!(r.code, 1, "{}", r.err);
    assert_ne!(t.0, 0);
    assert!(r.err.contains("git rm -r d"), "{}", r.err);
    assert!(r.err.trim_end().ends_with("Nothing was changed."), "{}", r.err);
    p.assert_same("rm d refused");
}

/// Review finding 1: an empty or `.` pathspec at the repository root used to
/// match every tracked path, and `rm .` deleted the whole tree and exited 0.
/// Git refuses all of these; a refusal must change nothing.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn rm_of_the_repository_root_without_dash_r_refuses_and_changes_nothing() {
    for (label, args, needle) in [
        ("dot", vec!["rm", "."], "git rm -r ."),
        ("dot cached", vec!["rm", "--cached", "."], "git rm -r ."),
        ("dot slash", vec!["rm", "./"], "git rm -r ."),
        ("dot -f", vec!["rm", "-f", "."], "git rm -r ."),
        ("empty", vec!["rm", ""], "empty string"),
        ("empty cached", vec!["rm", "--cached", ""], "empty string"),
        ("empty -r", vec!["rm", "-r", ""], "empty string"),
        ("empty next to a file", vec!["rm", "a.txt", ""], "empty string"),
    ] {
        let p = Pair::new(&base);
        let fp = support::Fingerprint::take(&p.ours);
        let r = p.ours("", &args).await;
        let t = p.twin(&args);
        assert_eq!(t.0, 128, "[{label}] git refuses: {}", t.2);
        assert_eq!(r.code, 1, "[{label}] {}", r.err);
        assert!(r.err.contains(needle), "[{label}] {}", r.err);
        assert!(r.err.trim_end().ends_with("Nothing was changed."), "[{label}] {}", r.err);
        let diffs = support::Fingerprint::take(&p.ours).differences_from(&fp);
        assert!(diffs.is_empty(), "[{label}] a refusal changed the repository: {diffs:?}");
        p.assert_same(label);
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn rm_dash_r_of_the_repository_root_still_works() {
    for (label, args) in [("dot", vec!["rm", "-r", "."]), ("dot cached", vec!["rm", "-r", "--cached", "."]), ("dot slash", vec!["rm", "-r", "./"])] {
        let (r, t, p) = both(&base, &args, label).await;
        assert_eq!(r.code, 0, "[{label}] {}", r.err);
        assert_eq!(t.0, 0, "[{label}] {}", t.2);
        p.assert_same(label);
    }
}

/// The other write verbs that take a pathspec refuse the empty string as git
/// does, and `add .` keeps working.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_empty_pathspec_is_refused_by_every_write_verb_that_takes_one() {
    let edit = |d: &Path| {
        base(d);
        support::write_file(d, "a.txt", "alpha two\n");
        support::git(d, &["add", "a.txt"]);
    };
    for (label, args) in [
        ("add", vec!["add", ""]),
        ("restore", vec!["restore", "--staged", ""]),
        ("reset", vec!["reset", "--", ""]),
        ("commit", vec!["commit", "-m", "x", "--", ""]),
    ] {
        let p = Pair::new(&edit);
        let fp = support::Fingerprint::take(&p.ours);
        let r = p.ours("", &args).await;
        let t = p.twin(&args);
        assert_eq!(t.0, 128, "[{label}] git refuses: {}", t.2);
        assert_eq!(r.code, 1, "[{label}] {}", r.err);
        assert!(r.err.contains("empty string"), "[{label}] {}", r.err);
        let diffs = support::Fingerprint::take(&p.ours).differences_from(&fp);
        assert!(diffs.is_empty(), "[{label}] a refusal changed the repository: {diffs:?}");
    }
    let p = Pair::new(&|d| {
        base(d);
        support::write_file(d, "n.txt", "new\n");
    });
    assert_eq!(p.ours("", &["add", "."]).await.code, 0);
    assert_eq!(p.twin(&["add", "."]).0, 0);
    p.assert_same("add .");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn rm_keeps_gits_safety_checks_and_force_overrides_them() {
    // (label, setup, args that git refuses, text our refusal must carry)
    type Setup = fn(&Path);
    let cases: Vec<(&str, Setup, &str)> = vec![
        ("local modification", |d| {
            base(d);
            support::write_file(d, "a.txt", "changed\n");
        }, "local modifications: a.txt"),
        ("staged change", |d| {
            base(d);
            support::write_file(d, "a.txt", "changed\n");
            support::git(d, &["add", "a.txt"]);
        }, "changes staged in the index: a.txt"),
        ("staged and different from the file", |d| {
            base(d);
            support::write_file(d, "a.txt", "changed\n");
            support::git(d, &["add", "a.txt"]);
            support::write_file(d, "a.txt", "changed again\n");
        }, "staged content different from both the file and the HEAD: a.txt"),
        ("new file staged", |d| {
            base(d);
            support::write_file(d, "n.txt", "n\n");
            support::git(d, &["add", "n.txt"]);
        }, "changes staged in the index: n.txt"),
    ];
    for (label, setup, text) in cases {
        let target = if label == "new file staged" { "n.txt" } else { "a.txt" };
        let p = Pair::new(&setup);
        let fp = support::Fingerprint::take(&p.ours);
        let r = p.ours("", &["rm", target]).await;
        assert_eq!(r.code, 1, "[{label}] {}", r.err);
        assert!(r.err.contains(text), "[{label}] {}", r.err);
        assert!(r.err.contains(&format!("git rm -f {target}")), "[{label}] {}", r.err);
        assert!(r.err.trim_end().ends_with("Nothing was changed."), "[{label}] {}", r.err);
        assert_ne!(p.twin(&["rm", target]).0, 0, "[{label}] git must refuse too");
        assert!(support::Fingerprint::take(&p.ours).differences_from(&fp).is_empty(), "[{label}] a refusal changed the repository");
        p.assert_same(&format!("{label}: both refused"));

        // -f removes in both.
        let r = p.ours("", &["rm", "-f", target]).await;
        assert_eq!(r.code, 0, "[{label}] {}", r.err);
        assert_eq!(p.twin(&["rm", "-f", target]).0, 0);
        p.assert_same(&format!("{label}: -f"));
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn rm_cached_allows_an_index_that_matches_head_and_refuses_one_that_does_not() {
    // File modified in the worktree only: --cached is allowed (index == HEAD).
    let (r, t, p) = both(&|d| {
        base(d);
        support::write_file(d, "a.txt", "changed\n");
    }, &["rm", "--cached", "a.txt"], "cached local").await;
    assert_eq!((r.code, t.0), (0, 0), "{}", r.err);
    p.assert_same("rm --cached with a local edit");

    // Staged change with the file equal to the index: --cached is allowed.
    let (r, t, p) = both(&|d| {
        base(d);
        support::write_file(d, "a.txt", "changed\n");
        support::git(d, &["add", "a.txt"]);
    }, &["rm", "--cached", "a.txt"], "cached staged").await;
    assert_eq!((r.code, t.0), (0, 0), "{}", r.err);
    p.assert_same("rm --cached with a staged change");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn rm_of_an_untracked_path_is_a_pathspec_error() {
    let (r, t, _p) = both(&|d| {
        base(d);
        support::write_file(d, "u.txt", "u\n");
    }, &["rm", "u.txt"], "untracked").await;
    assert_eq!(r.code, 1, "{}", r.err);
    assert_ne!(t.0, 0);
    assert!(r.err.contains("'u.txt' did not match"), "{}", r.err);
}

/// `git reset -- <pathspec>` exits 0 and changes nothing when the pathspec
/// matches no entry; this shell refuses (exit 1). docs/issues.md W51.
#[ignore = "BUG: reset -- <unmatched pathspec> refuses here; git exits 0 and unstages nothing"]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn reset_with_an_unmatched_pathspec_is_not_an_error_in_git() {
    let (r, t, p) = both(&staged_edits, &["reset", "--", "nomatch"], "reset -- nomatch").await;
    assert_eq!(t.0, 0, "git: {}", t.2);
    assert_eq!(r.code, 0, "{}", r.err);
    p.assert_same("reset -- nomatch");
}

/// `rm d/` needs `-r` as `rm d` does.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn rm_of_a_directory_spelled_with_a_slash_needs_dash_r() {
    let (r, t, p) = both(&base, &["rm", "d/"], "rm d/").await;
    assert_ne!(t.0, 0, "git: {}", t.2);
    assert_eq!(r.code, 1, "{}", r.err);
    assert!(r.err.contains("git rm -r d/"), "{}", r.err);
    p.assert_same("rm d/");
}

/// A trailing `/` in a pathspec names a directory only. `a.txt/` is
/// refused where `a.txt` is a file; `d/` and `d//` reach into the directory.
/// The row for a file spelled with a slash is held to git's exit status
/// (nonzero) and an unchanged index; the directory rows to git's result.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_trailing_slash_in_a_pathspec_matches_a_directory_only_in_the_index_verbs() {
    let setup = |d: &Path| {
        staged_edits(d);
        support::write_file(d, "d/e/f.txt", "foxtrot two\n");
        support::git(d, &["add", "d/e/f.txt"]);
        support::write_file(d, "d/c.txt", "charlie three\n");
    };
    // Refused by both: the file is not a directory.
    for (label, args) in [
        ("add file/", vec!["add", "a.txt/"]),
        ("rm file/", vec!["rm", "b.txt/"]),
        ("rm --cached file/", vec!["rm", "--cached", "b.txt/"]),
        ("restore --staged file/", vec!["restore", "--staged", "a.txt/"]),
    ] {
        let (r, t, p) = both(&setup, &args, label).await;
        assert_ne!(t.0, 0, "[{label}] git must refuse (negative control): {}", t.2);
        assert_ne!(r.code, 0, "[{label}] we must refuse like git");
        p.assert_same(label);
    }
    // Accepted by both, with the same result.
    for (label, args) in [
        ("add dir/", vec!["add", "d/"]),
        ("add dir//", vec!["add", "d//"]),
        ("add -u dir/", vec!["add", "-u", "d/"]),
        ("rm -r dir/", vec!["rm", "-r", "-f", "d/e/"]),
        ("rm -r --cached dir/", vec!["rm", "-r", "-f", "--cached", "d/"]),
        ("restore --staged dir/", vec!["restore", "--staged", "d/"]),
        ("reset -- dir/", vec!["reset", "--", "d/"]),
    ] {
        let (r, t, p) = both(&setup, &args, label).await;
        assert_eq!(t.0, 0, "[{label}] git: {}", t.2);
        assert_eq!(r.code, 0, "[{label}] {}", r.err);
        p.assert_same(label);
    }
}

// ── mv ──────────────────────────────────────────────────────────────────────

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn mv_matches_git() {
    for (label, args) in [
        ("rename", vec!["mv", "a.txt", "z.txt"]),
        ("into a directory", vec!["mv", "a.txt", "d"]),
        ("several into a directory", vec!["mv", "a.txt", "b.txt", "d"]),
        ("rename a directory", vec!["mv", "d", "dd"]),
        ("directory into a directory", vec!["mv", "d/e", "."]),
        ("a file out of a directory", vec!["mv", "d/c.txt", "c.txt"]),
    ] {
        let (r, t, p) = both(&base, &args, label).await;
        assert_eq!(r.code, 0, "[{label}] {}", r.err);
        assert_eq!(t.0, 0, "[{label}] {}", t.2);
        p.assert_same(label);
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn mv_keeps_staged_changes_and_untracked_files_of_a_directory() {
    let setup = |d: &Path| {
        base(d);
        support::write_file(d, "d/c.txt", "staged edit\n");
        support::git(d, &["add", "d/c.txt"]);
        support::write_file(d, "d/untracked.txt", "u\n");
    };
    let (r, t, p) = both(&setup, &["mv", "d", "moved"], "dir").await;
    assert_eq!((r.code, t.0), (0, 0), "{}", r.err);
    p.assert_same("mv keeps staged content");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn mv_refuses_what_git_refuses_and_changes_nothing() {
    let setup = |d: &Path| {
        base(d);
        support::write_file(d, "u.txt", "untracked\n");
    };
    for (label, args, text) in [
        ("untracked source", vec!["mv", "u.txt", "x.txt"], "not under version control"),
        ("missing source", vec!["mv", "gone.txt", "x.txt"], "does not exist"),
        ("existing destination", vec!["mv", "a.txt", "b.txt"], "'b.txt' exists"),
        ("into itself", vec!["mv", "d", "d/e/d"], "into itself"),
        ("several sources, destination is a file", vec!["mv", "a.txt", "b.txt", "u.txt"], "not a directory"),
        ("missing parent", vec!["mv", "a.txt", "nodir/a.txt"], "'nodir' does not exist"),
    ] {
        let p = Pair::new(&setup);
        let fp = support::Fingerprint::take(&p.ours);
        let r = p.ours("", &args).await;
        assert_eq!(r.code, 1, "[{label}] {}", r.err);
        assert!(r.err.contains(text), "[{label}] {}", r.err);
        assert!(r.err.trim_end().ends_with("Nothing was changed."), "[{label}] {}", r.err);
        assert_ne!(p.twin(&args).0, 0, "[{label}] git must refuse too");
        assert!(support::Fingerprint::take(&p.ours).differences_from(&fp).is_empty(), "[{label}] a refusal changed the repository");
        p.assert_same(label);
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn mv_force_replaces_an_existing_file() {
    let (r, t, p) = both(&base, &["mv", "-f", "a.txt", "b.txt"], "force").await;
    assert_eq!((r.code, t.0), (0, 0), "{} / {}", r.err, t.2);
    p.assert_same("mv -f");
}

// ── restore --staged, reset ─────────────────────────────────────────────────

fn staged_edits(d: &Path) {
    base(d);
    support::write_file(d, "a.txt", "alpha two\n");
    support::write_file(d, "d/c.txt", "charlie two\n");
    support::write_file(d, "new.txt", "new\n");
    support::git(d, &["add", "-A"]);
    // An unstaged edit on top, which unstaging must leave alone.
    support::write_file(d, "a.txt", "alpha three\n");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn restore_staged_and_reset_match_git() {
    for (label, args) in [
        ("restore --staged file", vec!["restore", "--staged", "a.txt"]),
        ("restore --staged .", vec!["restore", "--staged", "."]),
        ("restore --staged dir", vec!["restore", "--staged", "d"]),
        ("restore -S new file", vec!["restore", "-S", "new.txt"]),
        ("reset", vec!["reset"]),
        ("reset --mixed", vec!["reset", "--mixed"]),
        ("reset HEAD", vec!["reset", "HEAD"]),
        ("reset -- path", vec!["reset", "--", "a.txt"]),
        ("reset path", vec!["reset", "a.txt", "new.txt"]),
        ("reset HEAD path", vec!["reset", "HEAD", "d/c.txt"]),
        ("reset HEAD -- path", vec!["reset", "HEAD", "--", "d"]),
    ] {
        let (r, t, p) = both(&staged_edits, &args, label).await;
        assert_eq!(r.code, 0, "[{label}] {}", r.err);
        assert_eq!(t.0, 0, "[{label}] {}", t.2);
        p.assert_same(label);
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn unstaging_leaves_untouched_entries_with_their_stat() {
    let p = Pair::new(&staged_edits);
    let before = support::git(&p.ours, &["ls-files", "-s", "--debug", "b.txt"]);
    let r = p.ours("", &["restore", "--staged", "a.txt"]).await;
    assert_eq!(r.code, 0, "{}", r.err);
    assert_eq!(before, support::git(&p.ours, &["ls-files", "-s", "--debug", "b.txt"]), "an untouched entry lost its stat");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn unstaging_in_a_repository_with_no_commits_matches_git() {
    let setup = |d: &Path| {
        support::write_file(d, "one.txt", "1\n");
        support::write_file(d, "sub/two.txt", "2\n");
        support::git(d, &["add", "-A"]);
    };
    for args in [vec!["reset"], vec!["reset", "--", "one.txt"], vec!["restore", "--staged", "."]] {
        let (r, t, p) = both(&setup, &args, "unborn").await;
        assert_eq!(r.code == 0, t.0 == 0, "{args:?}: ours {} {} / git {} {}", r.code, r.err, t.0, t.2);
        if t.0 == 0 {
            p.assert_same(&format!("unborn {args:?}"));
        }
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn restore_without_staged_and_reset_revision_forms_refuse() {
    let p = Pair::new(&staged_edits);
    for (args, code, text, next) in [
        (vec!["restore", "a.txt"], 2, "overwrites file content", "git restore --staged <path>"),
        (vec!["restore", "--worktree", "a.txt"], 2, "overwrites file content", "git restore --staged <path>"),
        (vec!["restore", "--staged", "--worktree", "a.txt"], 2, "overwrites file content", "git restore --staged <path>"),
        (vec!["reset", "--hard"], 2, "discards working-tree changes", "git reset -- <path>"),
        (vec!["reset", "--hard", "HEAD~1"], 2, "discards working-tree changes", "git reset --soft <rev>"),
        (vec!["reset", "--merge", "HEAD~1"], 2, "writes working-tree files", "git reset --soft <rev>"),
        (vec!["reset", "--keep", "HEAD~1"], 2, "writes working-tree files", "git reset --soft <rev>"),
        (vec!["reset", "--soft", "--mixed", "HEAD"], 2, "cannot be used together", "git reset --soft <rev>"),
        (vec!["reset", "--soft", "HEAD", "a.txt"], 2, "cannot be done together", "git reset -- <path>"),
        (vec!["reset", "-p"], 2, "terminal", "git reset -- <path>"),
    ] {
        let fp = support::Fingerprint::take(&p.ours);
        let r = p.ours("", &args).await;
        assert_eq!(r.code, code, "{args:?}: {}", r.err);
        assert!(r.err.contains(": refused."), "{args:?}: {}", r.err);
        assert!(r.err.contains(text), "{args:?}: {}", r.err);
        assert!(r.err.contains(next), "{args:?}: {}", r.err);
        assert!(r.err.trim_end().ends_with("Nothing was changed."), "{args:?}: {}", r.err);
        assert!(support::Fingerprint::take(&p.ours).differences_from(&fp).is_empty(), "{args:?} changed the repository");
    }
    // `--hard` says it never happens; the others name the embedder.
    let r = p.ours("", &["reset", "--hard"]).await;
    assert!(r.err.contains("this shell never does it"), "{}", r.err);
}

// ── repository states ───────────────────────────────────────────────────────

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn writes_refuse_in_repository_states_they_must_not_run_in() {
    type Setup = fn(&Path);
    let cases: Vec<(&str, Setup, &str)> = vec![
        ("merge in progress", |d| { base(d); std::fs::write(d.join(".git/MERGE_HEAD"), "0".repeat(40) + "\n").expect("w"); }, "a merge is in progress"),
        ("cherry-pick", |d| { base(d); std::fs::write(d.join(".git/CHERRY_PICK_HEAD"), "0".repeat(40) + "\n").expect("w"); }, "a cherry-pick is in progress"),
        ("revert", |d| { base(d); std::fs::write(d.join(".git/REVERT_HEAD"), "0".repeat(40) + "\n").expect("w"); }, "a revert is in progress"),
        ("rebase-merge", |d| { base(d); std::fs::create_dir(d.join(".git/rebase-merge")).expect("w"); }, "a rebase is in progress"),
        ("rebase-apply", |d| { base(d); std::fs::create_dir(d.join(".git/rebase-apply")).expect("w"); }, "a rebase or patch application is in progress"),
        ("sequencer", |d| { base(d); std::fs::create_dir(d.join(".git/sequencer")).expect("w"); }, "a cherry-pick or revert sequence is in progress"),
        ("bisect", |d| { base(d); std::fs::write(d.join(".git/BISECT_LOG"), "x\n").expect("w"); }, "a bisect is in progress"),
        ("core.symlinks=false", |d| { base(d); support::git(d, &["config", "core.symlinks", "false"]); }, "core.symlinks=false"),
        ("core.ignorecase=true", |d| { base(d); support::git(d, &["config", "core.ignorecase", "true"]); }, "core.ignorecase=true"),
        ("core.splitIndex", |d| { base(d); support::git(d, &["config", "core.splitIndex", "true"]); }, "core.splitIndex=true"),
        ("core.sparseCheckout", |d| { base(d); support::git(d, &["config", "core.sparseCheckout", "true"]); }, "core.sparseCheckout=true"),
        ("core.attributesFile", |d| { base(d); support::git(d, &["config", "core.attributesFile", "/tmp/x"]); }, "core.attributesFile"),
        ("post-index-change hook", |d| {
            base(d);
            let h = d.join(".git/hooks/post-index-change");
            std::fs::write(&h, "#!/bin/sh\n").expect("hook");
            // Git runs a hook that is an executable file, and no other.
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&h, std::fs::Permissions::from_mode(0o755)).expect("chmod");
        }, "post-index-change hook"),
        ("hooksPath outside the tree", |d| { base(d); support::git(d, &["config", "core.hooksPath", "/elsewhere"]); }, "core.hooksPath points outside"),
    ];
    for (label, setup, text) in cases {
        let p = Pair::new(&|d| { setup(d); support::write_file(d, "a.txt", "changed\n"); });
        for args in [vec!["add", "a.txt"], vec!["rm", "-f", "b.txt"], vec!["mv", "b.txt", "bb.txt"], vec!["restore", "--staged", "a.txt"], vec!["reset"]] {
            let fp = support::Fingerprint::take(&p.ours);
            let r = p.ours("", &args).await;
            assert_eq!(r.code, 4, "[{label}] {args:?}: {}", r.err);
            assert!(r.err.contains(text), "[{label}] {args:?}: {}", r.err);
            assert!(r.err.trim_end().ends_with("Nothing was changed."), "[{label}] {}", r.err);
            assert!(support::Fingerprint::take(&p.ours).differences_from(&fp).is_empty(), "[{label}] {args:?} changed the repository");
        }
    }
}

/// Git runs `post-index-change` only when it is an executable file (or a link
/// to one); a mode 0644 file is ignored with a hint and `git add` succeeds.
/// This build refuses only what git would run.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_post_index_change_file_git_would_not_run_does_not_refuse() {
    use std::os::unix::fs::PermissionsExt;
    let p = Pair::new(&|d| {
        base(d);
        let h = d.join(".git/hooks/post-index-change");
        std::fs::write(&h, "#!/bin/sh\nexit 1\n").expect("hook");
        std::fs::set_permissions(&h, std::fs::Permissions::from_mode(0o644)).expect("chmod");
        support::write_file(d, "a.txt", "changed\n");
    });
    let (code, _, err) = p.twin(&["add", "a.txt"]);
    assert_eq!(code, 0, "git ignores a hook that is not executable: {err}");
    let r = p.ours("", &["add", "a.txt"]).await;
    assert_eq!(r.code, 0, "{}", r.err);
    p.assert_same("0644 post-index-change");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_split_index_and_unmerged_entries_refuse() {
    // Split index: created by real git, then written to by us would lose entries.
    let p = Pair::new(&|d| {
        base(d);
        support::git(d, &["update-index", "--split-index"]);
        support::write_file(d, "a.txt", "changed\n");
    });
    let fp = support::Fingerprint::take(&p.ours);
    let r = p.ours("", &["add", "a.txt"]).await;
    assert_eq!(r.code, 4, "{}", r.err);
    assert!(r.err.contains("split index"), "{}", r.err);
    assert!(support::Fingerprint::take(&p.ours).differences_from(&fp).is_empty());

    // Unmerged entries: a real conflict, then the merge state removed so only
    // the index says so.
    let p = Pair::new(&|d| {
        base(d);
        support::git(d, &["checkout", "-qb", "side"]);
        support::write_file(d, "a.txt", "side\n");
        support::git(d, &["commit", "-qam", "side"]);
        support::git(d, &["checkout", "-q", "main"]);
        support::write_file(d, "a.txt", "main\n");
        support::git(d, &["commit", "-qam", "main"]);
        let (code, _, _) = git_raw(d, &["merge", "side"]);
        assert_ne!(code, 0, "the merge must conflict");
        std::fs::remove_file(d.join(".git/MERGE_HEAD")).expect("rm MERGE_HEAD");
    });
    let fp = support::Fingerprint::take(&p.ours);
    let r = p.ours("", &["add", "b.txt"]).await;
    assert_eq!(r.code, 4, "{}", r.err);
    assert!(r.err.contains("'a.txt' has unresolved merge conflict entries"), "{}", r.err);
    assert!(support::Fingerprint::take(&p.ours).differences_from(&fp).is_empty());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_sha256_repository_refuses_before_anything_is_written() {
    let fx = Fixture::empty();
    let dir = fx.path("ours");
    std::fs::create_dir_all(&dir).expect("dir");
    support::git(&dir, &["init", "-q", "--object-format=sha256", "-b", "main"]);
    support::write_file(&dir, "a.txt", "a\n");
    let fp = support::Fingerprint::take(&dir);
    let r = run_tool(&fx.root(), "/mnt/ours", &["add", "a.txt"]).await;
    assert_eq!(r.code, 4, "{}", r.err);
    assert!(support::Fingerprint::take(&dir).differences_from(&fp).is_empty());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_bare_repository_has_no_worktree_to_write() {
    let fx = Fixture::empty();
    let dir = fx.path("bare.git");
    std::fs::create_dir_all(&dir).expect("dir");
    support::git(&dir, &["init", "-q", "--bare", "-b", "main"]);
    let r = run_tool(&fx.root(), "/mnt/bare.git", &["add", "-A"]).await;
    assert_eq!(r.code, 1, "{}", r.err);
    assert!(r.err.contains("bare"), "{}", r.err);
}

// ── content conversion: blob ids against `git hash-object` ──────────────────

#[derive(Clone, Copy, PartialEq, Debug)]
enum Expect {
    /// Staged, with git's blob id. A refusal here fails the test: the profile
    /// would be unusable for this common configuration.
    Pass,
    /// Refused with exit 4. Staging fails the test: git's blob would differ.
    Refuse,
    /// Either; a staged blob must still equal git's.
    Either,
}

struct Case {
    name: &'static str,
    /// `.gitattributes` at the root.
    attrs: &'static str,
    /// Extra `git config` pairs.
    config: &'static [(&'static str, &'static str)],
    /// (path, bytes) of extra attribute files: `sub/.gitattributes` etc.
    extra: &'static [(&'static str, &'static str)],
    path: &'static str,
    bytes: &'static [u8],
    expect: Expect,
}

const LF: &[u8] = b"one\ntwo\n";
const CRLF: &[u8] = b"one\r\ntwo\r\n";

const CASES: &[Case] = &[
    Case { name: "text=auto with LF", attrs: "* text=auto\n", config: &[], extra: &[], path: "f.txt", bytes: LF, expect: Expect::Pass },
    Case { name: "text=auto with CRLF", attrs: "* text=auto\n", config: &[], extra: &[], path: "f.txt", bytes: CRLF, expect: Expect::Refuse },
    Case { name: "no attributes with CRLF", attrs: "", config: &[], extra: &[], path: "f.txt", bytes: CRLF, expect: Expect::Pass },
    Case { name: "text eol=crlf with LF", attrs: "* text eol=crlf\n", config: &[], extra: &[], path: "f.txt", bytes: LF, expect: Expect::Pass },
    Case { name: "text with CRLF", attrs: "* text\n", config: &[], extra: &[], path: "f.txt", bytes: CRLF, expect: Expect::Refuse },
    Case { name: "-text overrides text=auto", attrs: "* text=auto\n*.bin -text\n", config: &[], extra: &[], path: "x.bin", bytes: CRLF, expect: Expect::Pass },
    Case { name: "binary macro overrides text=auto", attrs: "* text=auto\n*.bin binary\n", config: &[], extra: &[], path: "x.bin", bytes: CRLF, expect: Expect::Pass },
    Case { name: "filter on LF bytes", attrs: "* filter=lfs\n", config: &[], extra: &[], path: "f.txt", bytes: LF, expect: Expect::Refuse },
    Case { name: "filter that does not match", attrs: "*.txt filter=x\n", config: &[], extra: &[], path: "f.md", bytes: LF, expect: Expect::Pass },
    Case { name: "-filter", attrs: "* filter=x\n*.md -filter\n", config: &[], extra: &[], path: "f.md", bytes: LF, expect: Expect::Pass },
    Case { name: "working-tree-encoding", attrs: "* working-tree-encoding=utf-16\n", config: &[], extra: &[], path: "f.txt", bytes: LF, expect: Expect::Refuse },
    Case { name: "ident with $Id", attrs: "* ident\n", config: &[], extra: &[], path: "f.txt", bytes: b"$Id: 1 $\n", expect: Expect::Refuse },
    Case { name: "ident without $Id", attrs: "* ident\n", config: &[], extra: &[], path: "f.txt", bytes: LF, expect: Expect::Pass },
    Case { name: "autocrlf=true with CRLF", attrs: "", config: &[("core.autocrlf", "true")], extra: &[], path: "f.txt", bytes: CRLF, expect: Expect::Refuse },
    Case { name: "autocrlf=true with LF", attrs: "", config: &[("core.autocrlf", "true")], extra: &[], path: "f.txt", bytes: LF, expect: Expect::Pass },
    Case { name: "autocrlf=input with CRLF", attrs: "", config: &[("core.autocrlf", "input")], extra: &[], path: "f.txt", bytes: CRLF, expect: Expect::Refuse },
    Case { name: "autocrlf=false with CRLF", attrs: "", config: &[("core.autocrlf", "false")], extra: &[], path: "f.txt", bytes: CRLF, expect: Expect::Pass },
    Case { name: "core.eol with CRLF", attrs: "", config: &[("core.eol", "crlf")], extra: &[], path: "f.txt", bytes: CRLF, expect: Expect::Either },
    Case { name: "macro that sets text", attrs: "[attr]conv text\n* conv\n", config: &[], extra: &[], path: "f.txt", bytes: CRLF, expect: Expect::Refuse },
    Case { name: "nested attributes file", attrs: "", config: &[], extra: &[("sub/.gitattributes", "* text\n")], path: "sub/f.txt", bytes: CRLF, expect: Expect::Refuse },
    Case { name: "nested attributes file, file outside it", attrs: "", config: &[], extra: &[("sub/.gitattributes", "* text\n")], path: "f.txt", bytes: CRLF, expect: Expect::Pass },
    Case { name: "info/attributes", attrs: "", config: &[], extra: &[(".git/info/attributes", "* text=auto\n")], path: "f.txt", bytes: CRLF, expect: Expect::Refuse },
    Case { name: "anchored pattern, same level", attrs: "/top.txt text\n", config: &[], extra: &[], path: "top.txt", bytes: CRLF, expect: Expect::Refuse },
    Case { name: "anchored pattern, deeper file", attrs: "/top.txt text\n", config: &[], extra: &[], path: "sub/top.txt", bytes: CRLF, expect: Expect::Pass },
    Case { name: "later line wins", attrs: "*.txt -text\n*.txt text\n", config: &[], extra: &[], path: "f.txt", bytes: CRLF, expect: Expect::Refuse },
    Case { name: "later -text wins", attrs: "*.txt text\n*.txt -text\n", config: &[], extra: &[], path: "f.txt", bytes: CRLF, expect: Expect::Pass },
    Case { name: "directory-only pattern", attrs: "sub/ text\n", config: &[], extra: &[], path: "sub/f.txt", bytes: CRLF, expect: Expect::Pass },
    Case { name: "double star", attrs: "**/deep.txt text\n", config: &[], extra: &[], path: "a/b/deep.txt", bytes: CRLF, expect: Expect::Refuse },
    Case { name: "eol=lf", attrs: "* eol=lf\n", config: &[], extra: &[], path: "f.txt", bytes: CRLF, expect: Expect::Either },
    Case { name: "crlf attribute", attrs: "* crlf\n", config: &[], extra: &[], path: "f.txt", bytes: CRLF, expect: Expect::Refuse },
    Case { name: "text=auto and a file with a lone CR", attrs: "* text=auto\n", config: &[], extra: &[], path: "f.txt", bytes: b"a\rb\n", expect: Expect::Refuse },
    Case { name: "quoted pattern", attrs: "\"a b.txt\" text\n", config: &[], extra: &[], path: "a b.txt", bytes: CRLF, expect: Expect::Refuse },
    Case { name: "negated pattern is ignored by git", attrs: "!*.txt text\n", config: &[], extra: &[], path: "f.txt", bytes: CRLF, expect: Expect::Pass },
    Case { name: "comment line", attrs: "# * text\n", config: &[], extra: &[], path: "f.txt", bytes: CRLF, expect: Expect::Pass },
    Case { name: "bracket class", attrs: "[ab].txt text\n", config: &[], extra: &[], path: "a.txt", bytes: CRLF, expect: Expect::Refuse },
    Case { name: "text on a symlink-free binary with NUL", attrs: "* text=auto\n", config: &[], extra: &[], path: "f.bin", bytes: b"\0\r\n\0", expect: Expect::Refuse },
];

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn staged_blobs_equal_git_hash_object_over_the_attribute_matrix() {
    support::require_git();
    let mut staged = 0;
    let mut refused = 0;
    for case in CASES {
        let fx = Fixture::empty();
        let dir = fx.path("ours");
        std::fs::create_dir_all(&dir).expect("dir");
        support::git(&dir, &["init", "-q", "-b", "main"]);
        for (k, v) in case.config {
            support::git(&dir, &["config", k, v]);
        }
        if !case.attrs.is_empty() {
            std::fs::write(dir.join(".gitattributes"), case.attrs).expect("attrs");
        }
        for (path, text) in case.extra {
            let p = dir.join(path);
            std::fs::create_dir_all(p.parent().expect("parent")).expect("dir");
            std::fs::write(&p, text).expect("attrs");
        }
        let file = dir.join(case.path);
        std::fs::create_dir_all(file.parent().expect("parent")).expect("dir");
        std::fs::write(&file, case.bytes).expect("file");

        let (code, oracle, err) = git_raw(&dir, &["hash-object", case.path]);
        assert_eq!(code, 0, "[{}] hash-object: {err}", case.name);
        let oracle = oracle.trim().to_string();

        let r = run_tool(&fx.root(), "/mnt/ours", &["add", "--", case.path]).await;
        if r.code == 0 {
            staged += 1;
            let line = support::git(&dir, &["ls-files", "-s", "--", case.path]);
            let got = line.split_whitespace().nth(1).unwrap_or_default().to_string();
            assert_eq!(got, oracle, "[{}] the staged blob differs from `git hash-object`", case.name);
            assert_ne!(case.expect, Expect::Refuse, "[{}] must refuse, but staged", case.name);
        } else {
            refused += 1;
            assert_eq!(r.code, 4, "[{}] {}", case.name, r.err);
            assert_ne!(case.expect, Expect::Pass, "[{}] must stage, but refused: {}", case.name, r.err);
            assert!(r.err.contains("would be stored differently by real git") || r.err.contains("cannot match exactly"), "[{}] {}", case.name, r.err);
            // A refusal wrote nothing.
            assert!(support::git(&dir, &["ls-files"]).is_empty(), "[{}] a refusal staged something", case.name);
            assert!(lockfiles(&dir.join(".git")).is_empty());
        }
    }
    // The matrix has both outcomes, so a verb that refused (or staged)
    // everything cannot pass it.
    assert!(staged >= 10 && refused >= 10, "staged {staged}, refused {refused}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_symlinked_attributes_file_is_refused_not_followed() {
    let fx = Fixture::empty();
    let dir = fx.path("ours");
    std::fs::create_dir_all(&dir).expect("dir");
    support::git(&dir, &["init", "-q", "-b", "main"]);
    let outside = fx.path("outside-attrs");
    std::fs::write(&outside, "* text\n").expect("outside");
    std::os::unix::fs::symlink(&outside, dir.join(".gitattributes")).expect("link");
    support::write_file(&dir, "f.txt", "x\n");
    let r = run_tool(&fx.root(), "/mnt/ours", &["add", "f.txt"]).await;
    assert_eq!(r.code, 4, "{}", r.err);
    assert!(r.err.contains("'.gitattributes'") && r.err.contains("symbolic link"), "{}", r.err);
    assert!(!r.err.contains(&fx.root().display().to_string()), "{}", r.err);
}

// ── racy-git smudging ───────────────────────────────────────────────────────

/// The scenario `ce_smudge_racily_clean_entry` exists for: an entry recorded in
/// the same second as the index, whose file then changes without changing its
/// stat, and a later index write that moves the index's timestamp past it.
/// Git smudges the entry on that write; without it `git status` would call
/// the file clean.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_same_second_edit_is_not_hidden_by_a_later_index_write() {
    use filetime::FileTime;
    // The scenario needs the entry's mtime second to equal the index's; retry
    // if the clock ticked between the two writes.
    for attempt in 0..20 {
        let setup = |d: &Path| {
            base(d);
            support::git(d, &["config", "core.trustctime", "false"]);
            support::git(d, &["config", "core.checkstat", "minimal"]);
            support::write_file(d, "x.txt", "aaaa\n");
            support::git(d, &["add", "x.txt"]);
            // Same size, new content, and the stat the index recorded.
            let recorded = support::git(d, &["ls-files", "--debug", "x.txt"]);
            let line = recorded.lines().find(|l| l.trim_start().starts_with("mtime:")).expect("mtime line").to_string();
            let (secs, nanos) = line.trim_start().trim_start_matches("mtime:").trim().split_once(':').expect("s:n");
            support::write_file(d, "x.txt", "bbbb\n");
            filetime::set_file_mtime(d.join("x.txt"), FileTime::from_unix_time(secs.parse().expect("secs"), nanos.parse().expect("nanos"))).expect("set mtime");
            support::write_file(d, "other.txt", "other\n");
        };
        let p = Pair::new(&setup);
        let index_secs = FileTime::from_last_modification_time(&std::fs::metadata(p.ours.join(".git/index")).expect("index")).unix_seconds();
        let debug = support::git(&p.ours, &["ls-files", "--debug", "x.txt"]);
        let entry_secs: i64 = debug.lines().find(|l| l.trim_start().starts_with("mtime:")).expect("mtime").trim_start().trim_start_matches("mtime:").trim().split_once(':').expect("s:n").0.parse().expect("secs");
        if entry_secs != index_secs {
            assert!(attempt < 19, "never got the entry and the index into one second");
            continue;
        }
        // Move the clock past the recorded second, so the next index is newer.
        std::thread::sleep(std::time::Duration::from_millis(1100));
        let r = p.ours("", &["add", "other.txt"]).await;
        assert_eq!(r.code, 0, "{}", r.err);
        assert_eq!(p.twin(&["add", "other.txt"]).0, 0);

        let ours_x = index_view_of(&p.ours, "x.txt");
        let twin_x = index_view_of(&p.twin, "x.txt");
        assert!(twin_x.contains("size: 0"), "the scenario must make real git smudge, or the test proves nothing: {twin_x}");
        assert_eq!(ours_x, twin_x, "we must smudge exactly the entry git smudges");
        let status = support::git(&p.ours, &["status", "--porcelain"]);
        assert!(status.contains("M x.txt"), "git status must see the edit: {status}");
        p.assert_same("racy");
        return;
    }
}

fn index_view_of(repo: &Path, path: &str) -> String {
    index_view(repo).into_iter().find(|l| l.contains(&format!("\t{path} "))).unwrap_or_default()
}

/// The negative control for the smudge: an entry that is NOT racy keeps its
/// recorded size, so smudging everything would fail here.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_entry_older_than_the_index_is_not_smudged() {
    let p = Pair::new(&|d| {
        base(d);
        support::write_file(d, "other.txt", "other\n");
    });
    std::thread::sleep(std::time::Duration::from_millis(1100));
    // Rewrite the index so it is a second newer than every entry.
    support::git(&p.ours, &["update-index", "--really-refresh"]);
    support::git(&p.twin, &["update-index", "--really-refresh"]);
    let r = p.ours("", &["add", "other.txt"]).await;
    assert_eq!(r.code, 0, "{}", r.err);
    assert_eq!(p.twin(&["add", "other.txt"]).0, 0);
    let view = index_view(&p.ours).join("\n");
    assert!(view.contains("a.txt size: 6"), "{view}");
    p.assert_same("not racy");
}

// ── containment through the real verbs ──────────────────────────────────────

struct Hostile {
    fx: Fixture,
    repo: PathBuf,
    outside: PathBuf,
}

fn hostile() -> Hostile {
    support::require_git();
    let fx = Fixture::empty();
    let repo = fx.path("mounted/repo");
    let outside = fx.path("elsewhere-7f3a");
    std::fs::create_dir_all(&repo).expect("repo");
    std::fs::create_dir_all(&outside).expect("outside");
    support::write_file(&outside, "keep.txt", "keep\n");
    support::git(&repo, &["init", "-q", "-b", "main"]);
    base(&repo);
    Hostile { fx, repo, outside }
}

fn outside_snapshot(h: &Hostile) -> Vec<(String, Vec<u8>)> {
    let mut v: Vec<_> = std::fs::read_dir(&h.outside)
        .expect("read")
        .map(|e| {
            let e = e.expect("entry");
            (e.file_name().to_string_lossy().into_owned(), std::fs::read(e.path()).unwrap_or_default())
        })
        .collect();
    v.sort();
    v
}

async fn hostile_run(h: &Hostile, cwd: &str, argv: &[&str]) -> kaish_types::ExecResult {
    run_tool(&h.fx.path("mounted"), &format!("/mnt/repo/{cwd}"), argv).await
}

fn assert_contained(h: &Hostile, r: &kaish_types::ExecResult, what: &str, before: &[(String, Vec<u8>)]) {
    assert_eq!(r.code, 4, "[{what}] {}", r.err);
    assert!(!r.err.contains(&h.fx.root().display().to_string()), "[{what}] leaked a host path: {}", r.err);
    assert!(!r.err.contains("elsewhere-7f3a"), "[{what}] leaked the link target: {}", r.err);
    assert_eq!(outside_snapshot(h), before, "[{what}] the outside directory changed");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_planted_objects_fanout_link_stops_add() {
    let h = hostile();
    // Whatever the new blob's id starts with, its fan-out directory is a link.
    for n in 0..=255u32 {
        let dir = h.repo.join(format!(".git/objects/{n:02x}"));
        if !dir.exists() {
            std::os::unix::fs::symlink(&h.outside, &dir).expect("plant");
        }
    }
    support::write_file(&h.repo, "fresh.txt", "a brand new blob\n");
    let before = outside_snapshot(&h);
    let r = hostile_run(&h, "", &["add", "fresh.txt"]).await;
    assert_contained(&h, &r, "objects/xx", &before);
    assert!(r.err.contains("in the common git directory is a symbolic link"), "{}", r.err);
    assert!(!h.repo.join(".git/index.lock").exists(), "the lock must be released");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_planted_index_link_and_a_hard_link_stop_every_verb() {
    let h = hostile();
    support::write_file(&h.repo, "a.txt", "changed\n");
    let real = h.repo.join(".git/index");
    let saved = std::fs::read(&real).expect("index");

    // Symbolic link.
    std::fs::remove_file(&real).expect("rm");
    std::fs::write(h.outside.join("index"), &saved).expect("outside index");
    std::os::unix::fs::symlink(h.outside.join("index"), &real).expect("plant");
    let before = outside_snapshot(&h);
    for args in [vec!["add", "a.txt"], vec!["rm", "-f", "b.txt"], vec!["mv", "b.txt", "bb.txt"], vec!["restore", "--staged", "a.txt"], vec!["reset"]] {
        let r = hostile_run(&h, "", &args).await;
        assert_contained(&h, &r, &format!("index link {args:?}"), &before);
        assert!(r.err.contains("'index' in the git directory is a symbolic link"), "{}", r.err);
    }

    // Hard link: a second name for the same inode, outside the mount. The
    // index is replaced by renaming a new file over it, so the old inode (and
    // the outside name) is never written, and the verb works.
    std::fs::remove_file(&real).expect("rm");
    std::fs::write(h.outside.join("index"), &saved).expect("outside index");
    std::fs::hard_link(h.outside.join("index"), &real).expect("hard link");
    let before = outside_snapshot(&h);
    let r = hostile_run(&h, "", &["add", "a.txt"]).await;
    assert_eq!(r.code, 0, "{}", r.err);
    assert_eq!(outside_snapshot(&h), before, "the outside name of the old index inode changed");
    assert!(support::git(&h.repo, &["status", "--porcelain"]).contains("A  a.txt") || support::git(&h.repo, &["status", "--porcelain"]).contains("M  a.txt"));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_planted_index_lock_link_is_a_stale_lock_and_is_not_followed() {
    let h = hostile();
    support::write_file(&h.repo, "a.txt", "changed\n");
    std::os::unix::fs::symlink(h.outside.join("keep.txt"), h.repo.join(".git/index.lock")).expect("plant");
    let before = outside_snapshot(&h);
    let r = hostile_run(&h, "", &["add", "a.txt"]).await;
    // Refused by the walk (a link) or by the exclusive create (it exists); either way not written.
    assert_eq!(r.code, 4, "{}", r.err);
    assert_eq!(outside_snapshot(&h), before);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_symlinked_working_tree_directory_stops_add_rm_and_mv() {
    let h = hostile();
    support::write_file(&h.outside, "c.txt", "outside copy\n");
    std::fs::remove_dir_all(h.repo.join("d")).expect("rm d");
    std::os::unix::fs::symlink(&h.outside, h.repo.join("d")).expect("plant");
    let before = outside_snapshot(&h);
    for args in [
        vec!["add", "d/c.txt"],
        vec!["add", "-u", "d"],
        vec!["rm", "-f", "d/c.txt"],
        vec!["mv", "a.txt", "d/a.txt"],
        vec!["mv", "d/c.txt", "c.txt"],
    ] {
        let r = hostile_run(&h, "", &args).await;
        assert_contained(&h, &r, &format!("{args:?}"), &before);
        assert!(r.err.contains("'d' in the working tree is a symbolic link"), "{args:?}: {}", r.err);
    }
    assert!(h.repo.join("a.txt").exists(), "mv must not have moved the source");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_linked_worktree_checks_the_roots_it_writes_into() {
    let h = hostile();
    let linked = h.fx.path("mounted/wt");
    support::git(&h.repo, &["worktree", "add", "-q", "-b", "side", linked.to_str().expect("utf-8")]);
    support::write_file(&linked, "fresh.txt", "linked blob\n");
    // Objects live in the common directory: plant there.
    for n in 0..=255u32 {
        let dir = h.repo.join(format!(".git/objects/{n:02x}"));
        if !dir.exists() {
            std::os::unix::fs::symlink(&h.outside, &dir).expect("plant");
        }
    }
    let before = outside_snapshot(&h);
    let r = hostile_run(&h, "../wt", &["add", "fresh.txt"]).await;
    assert_contained(&h, &r, "linked objects", &before);
    assert!(r.err.contains("in the common git directory"), "{}", r.err);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_linked_worktree_writes_its_own_index() {
    let h = hostile();
    let linked = h.fx.path("mounted/wt");
    support::git(&h.repo, &["worktree", "add", "-q", "-b", "side", linked.to_str().expect("utf-8")]);
    support::write_file(&linked, "fresh.txt", "linked blob\n");
    let r = hostile_run(&h, "../wt", &["add", "fresh.txt"]).await;
    assert_eq!(r.code, 0, "{}", r.err);
    assert!(support::git(&linked, &["ls-files"]).contains("fresh.txt"));
    assert!(!support::git(&h.repo, &["ls-files"]).contains("fresh.txt"), "the main index must not change");
    assert_eq!(git_raw(&linked, &["fsck", "--strict"]).0, 0);
}

// ── through a real kernel ───────────────────────────────────────────────────

/// The verbs reached through `Kernel::execute`, so the kernel's own argv
/// binding (`-A`, `--`, `-r`) feeds them as an agent's command line would.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_index_verbs_work_through_a_real_kernel() {
    use kaish_kernel::{Kernel, KernelConfig};
    use kaish_tool_api::KernelBackend;
    support::require_git();
    let fx = Fixture::empty();
    let dir = fx.path("repo");
    std::fs::create_dir_all(&dir).expect("dir");
    support::git(&dir, &["init", "-q", "-b", "main"]);
    base(&dir);
    support::write_file(&dir, "a.txt", "alpha two\n");
    support::write_file(&dir, "new.txt", "new\n");

    let backend: Arc<dyn KernelBackend> = Arc::new(StrictBackend::single(PathBuf::from("/mnt"), fx.root()));
    let git = kaish_tools_git::tool(GitConfig::read_only().with_commit_profile()).expect("config");
    let mut cfg = KernelConfig::transient();
    cfg.cwd = PathBuf::from("/mnt/repo");
    cfg.skip_validation = true;
    let kernel = Kernel::with_backend(backend, cfg, |_| {}, |tools| tools.register(git)).expect("kernel");

    for (cmd, status_after) in [
        ("git add -A", "A  new.txt"),
        ("git restore --staged new.txt", "?? new.txt"),
        ("git add -- new.txt", "A  new.txt"),
        ("git reset -- new.txt", "?? new.txt"),
        ("git mv b.txt bb.txt", "R  b.txt -> bb.txt"),
        ("git rm -r d", "D  d/c.txt"),
        ("git rm --cached bb.txt", "D  b.txt"),
    ] {
        let r = kernel.execute(cmd).await.unwrap_or_else(|e| panic!("{cmd}: {e}"));
        assert_eq!(r.code, 0, "{cmd}: {}", r.err);
        let status = support::git(&dir, &["status", "--porcelain"]);
        assert!(status.contains(status_after), "{cmd}: wanted {status_after:?} in\n{status}");
    }
    // A refusal arrives as the command's own failure, with its five parts.
    let r = kernel.execute("git reset --hard").await.expect("runs");
    assert_eq!(r.code, 2);
    assert!(r.err.contains("refused.") && r.err.contains("Nothing was changed."), "{}", r.err);
    assert_eq!(git_raw(&dir, &["fsck", "--strict"]).0, 0);
}

// ── non-UTF-8 paths survive every index verb (review T5) ───────────────────
//
// Git paths are bytes. A name that is not UTF-8 must come out of every verb
// as the same bytes it went in with: a lossy round trip renames it to U+FFFD
// in the index, or leaves it behind as a second entry.

use std::ffi::OsStr;
use std::os::unix::ffi::OsStrExt;

/// `git ls-files -s -z`: mode, id, stage and path bytes, no stat.
fn ls_files_z(repo: &Path) -> Vec<u8> {
    let out = Command::new("git")
        .args(["ls-files", "-s", "-z"])
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

/// `base` plus a Latin-1 named file in the root and in `d/`, committed.
fn base_with_latin1(dir: &Path) {
    base(dir);
    write_bytes_path(dir, LATIN1, "latin one\n");
    write_bytes_path(dir, LATIN1_IN_D, "latin two\n");
    support::git(dir, &["add", "-A"]);
    support::git(dir, &["commit", "-qm", "latin1"]);
}

async fn assert_latin1_same(setup: &dyn Fn(&Path), args: &[&str], label: &str) {
    let (r, t, p) = both(setup, args, label).await;
    assert_eq!(r.code, 0, "[{label}] {}", r.err);
    assert_eq!(t.0, 0, "[{label}] {}", t.2);
    let (ours, twin) = (ls_files_z(&p.ours), ls_files_z(&p.twin));
    assert!(ours == twin, "[{label}] ls-files -s -z differs\nours:  {:?}\ntwin: {:?}", String::from_utf8_lossy(&ours), String::from_utf8_lossy(&twin));
    assert!(ours.windows(LATIN1.len()).any(|w| w == LATIN1), "[{label}] the entry vanished");
    p.assert_same(label);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn reset_keeps_a_non_utf8_entry_as_one_entry() {
    let setup = |d: &Path| {
        base_with_latin1(d);
        write_bytes_path(d, LATIN1, "latin edited\n");
        write_bytes_path(d, LATIN1_IN_D, "latin two edited\n");
        support::git(d, &["add", "-A"]);
    };
    assert_latin1_same(&setup, &["reset"], "reset").await;
    assert_latin1_same(&setup, &["restore", "--staged", "*af?.txt"], "restore --staged glob").await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn rm_and_mv_find_a_non_utf8_entry() {
    assert_latin1_same(&base_with_latin1, &["rm", "-r", "d"], "rm -r d").await;
    assert_latin1_same(&base_with_latin1, &["rm", "--cached", "-r", "d"], "rm --cached -r d").await;
    assert_latin1_same(&base_with_latin1, &["mv", "d", "dd"], "mv d dd").await;
    assert_latin1_same(&base_with_latin1, &["mv", "a.txt", "d"], "mv a.txt d").await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn add_stages_edits_to_a_non_utf8_entry() {
    let setup = |d: &Path| {
        base_with_latin1(d);
        write_bytes_path(d, LATIN1, "latin edited\n");
        write_bytes_path(d, LATIN1_IN_D, "latin two edited\n");
        support::write_file(d, "new.txt", "new\n");
    };
    for args in [vec!["add", "-u"], vec!["add", "-A"], vec!["add", "."], vec!["add", "*af?.txt"], vec!["add", "d"]] {
        assert_latin1_same(&setup, &args, &args.join(" ")).await;
    }
    // A deleted non-UTF-8 file is staged as a deletion, not left behind.
    let gone = |d: &Path| {
        base_with_latin1(d);
        std::fs::remove_file(d.join(OsStr::from_bytes(LATIN1_IN_D))).expect("rm");
    };
    let (r, t, p) = both(&gone, &["add", "-u"], "deleted").await;
    assert_eq!((r.code, t.0), (0, 0), "{}", r.err);
    assert!(ls_files_z(&p.ours) == ls_files_z(&p.twin), "ls-files -s -z differs");
    p.assert_same("deleted non-UTF-8 file");
}

// ── a gitlink replaced by a file (review T9) ───────────────────────────────

fn gitlink_replaced_by_file(dir: &Path) {
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
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_gitlink_replaced_by_a_regular_file_is_staged_as_a_file() {
    for args in [vec!["add", "-u"], vec!["add", "-A"], vec!["add", "inner"], vec!["add", "."]] {
        let (r, t, p) = both(&gitlink_replaced_by_file, &args, "T9").await;
        assert_eq!((r.code, t.0), (0, 0), "{args:?}: {} / {}", r.err, t.2);
        p.assert_same(&args.join(" "));
        assert!(index_view(&p.ours).join("\n").contains("100644"), "{args:?}: the entry is not a file");
    }
}

// ── ignored paths named to `add` (review T11, T12) ─────────────────────────

fn with_ignored(dir: &Path) {
    base(dir);
    support::write_file(dir, ".gitignore", "*.log\nbuild/\n");
    support::git(dir, &["add", ".gitignore"]);
    support::git(dir, &["commit", "-qm", "ignore"]);
    support::write_file(dir, "d/y.log", "log\n");
    support::write_file(dir, "build/out.o", "obj\n");
    support::write_file(dir, "build/deep/more.o", "obj\n");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn naming_an_ignored_directory_refuses_with_the_force_hint_as_git_does() {
    for spec in ["build", "build/"] {
        let p = Pair::new(&with_ignored);
        let fp = support::Fingerprint::take(&p.ours);
        let r = p.ours("", &["add", spec]).await;
        let (code, _, twin_err) = p.twin(&["add", spec]);
        assert_eq!(code, 1, "[{spec}] git: {twin_err}");
        assert!(twin_err.contains("ignored by one of your .gitignore files"), "[{spec}] {twin_err}");
        assert_eq!(r.code, 1, "[{spec}] {}", r.err);
        assert!(r.err.contains("ignored by one of your .gitignore files: build") && r.err.contains("git add -f build"), "[{spec}] {}", r.err);
        assert!(support::Fingerprint::take(&p.ours).differences_from(&fp).is_empty(), "[{spec}] a refusal changed the repository");
    }
    // -f stages what is below; so does git.
    let p = Pair::new(&with_ignored);
    let r = p.ours("", &["add", "-f", "build"]).await;
    assert_eq!((r.code, i64::from(p.twin(&["add", "-f", "build"]).0)), (0, 0), "{}", r.err);
    p.assert_same("add -f build");
    // Git names an ignored directory even when the index tracks files in it.
    let tracked_inside = |d: &Path| {
        with_ignored(d);
        support::git(d, &["add", "-f", "build/out.o"]);
        support::git(d, &["commit", "-qm", "force"]);
    };
    let p = Pair::new(&tracked_inside);
    let r = p.ours("", &["add", "build"]).await;
    let t = p.twin(&["add", "build"]);
    assert_eq!(r.code, i64::from(t.0), "ours: {} / git: {}", r.err, t.2);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_glob_that_matches_only_ignored_files_matched_nothing() {
    for spec in ["*.log", "d/*.log", "build/*"] {
        let p = Pair::new(&with_ignored);
        let fp = support::Fingerprint::take(&p.ours);
        let r = p.ours("", &["add", spec]).await;
        let (code, _, twin_err) = p.twin(&["add", spec]);
        assert_eq!(code, 128, "[{spec}] git: {twin_err}");
        assert!(twin_err.contains("did not match any files"), "[{spec}] {twin_err}");
        assert_eq!(r.code, 1, "[{spec}] {}", r.err);
        assert!(r.err.contains(&format!("The pathspec '{spec}' did not match any files.")), "[{spec}] {}", r.err);
        assert!(support::Fingerprint::take(&p.ours).differences_from(&fp).is_empty(), "[{spec}] a refusal changed the repository");
    }
}

// ── a partial `rm` failure says what is left and what to run (review T14) ───

/// Make `d` unwritable in both repositories, so no file in it can be
/// unlinked. Returns false (and restores) where the user ignores directory
/// permissions and the failure cannot be produced.
#[cfg(unix)]
fn lock_d(p: &Pair) -> bool {
    use std::os::unix::fs::PermissionsExt;
    for dir in [&p.ours, &p.twin] {
        std::fs::set_permissions(dir.join("d"), std::fs::Permissions::from_mode(0o555)).expect("chmod");
    }
    if std::fs::File::create(p.ours.join("d/probe")).is_ok() {
        std::fs::remove_file(p.ours.join("d/probe")).expect("probe");
        unlock_d(p);
        return false;
    }
    true
}

#[cfg(unix)]
fn unlock_d(p: &Pair) {
    use std::os::unix::fs::PermissionsExt;
    for dir in [&p.ours, &p.twin] {
        std::fs::set_permissions(dir.join("d"), std::fs::Permissions::from_mode(0o755)).expect("chmod");
    }
}

/// Measured on git 2.56: `git rm` removes in index order. A deletion that
/// fails before any has succeeded dies (128) with nothing removed and nothing
/// staged. A deletion that fails after one has succeeded is passed over: git
/// prints nothing, stages every path as deleted, exits 0 and leaves the file,
/// which then shows as untracked. This shell matches the state and the exit
/// code, and adds a warning on stderr that names the paths left on disk.
#[cfg(unix)]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_failed_deletion_stops_or_is_passed_over_as_git_does() {
    // (arguments, git dies on the first deletion)
    for (args, dies) in [
        (vec!["rm", "d/c.txt"], true),
        (vec!["rm", "d/c.txt", "d/e/f.txt"], true),
        (vec!["rm", "a.txt", "d/c.txt"], false),
        (vec!["rm", "a.txt", "d/c.txt", "b.txt"], false),
        (vec!["rm", "-r", "a.txt", "d"], false),
        (vec!["rm", "-f", "a.txt", "d/c.txt"], false),
    ] {
        let p = Pair::new(&base);
        if !lock_d(&p) {
            return;
        }
        let r = p.ours("", &args).await;
        let t = p.twin(&args);
        unlock_d(&p);
        let label = args.join(" ");
        if dies {
            assert_eq!(t.0, 128, "[{label}] git dies: {}", t.2);
            assert_eq!(r.code, 1, "[{label}] {}", r.err);
            assert!(r.err.contains("Nothing was removed or staged"), "[{label}] {}", r.err);
            assert!(r.err.contains("git rm d/c.txt"), "[{label}] {}", r.err);
            assert_eq!(support::git(&p.ours, &["status", "--porcelain"]), "", "[{label}] nothing may be staged");
        } else {
            assert_eq!(t.0, 0, "[{label}] git goes on: {}", t.2);
            assert_eq!(r.code, 0, "[{label}] {}", r.err);
            assert_eq!(t.2, "", "[{label}] git says nothing");
            assert!(r.err.contains("warning: could not delete d/c.txt"), "[{label}] {}", r.err);
            assert!(r.err.contains("still on disk"), "[{label}] {}", r.err);
            // The cause stays in the warning, because the warning tells the operator to fix it.
            assert!(r.err.contains("Permission denied"), "[{label}] the warning dropped the cause: {}", r.err);
            assert_eq!(r.text_out(), t.1, "[{label}] git prints rm '<path>' for every path");
            // The failed file is staged as deleted and left on disk: untracked.
            let status = support::git(&p.ours, &["status", "--porcelain"]);
            assert!(status.contains("D  d/c.txt"), "[{label}] {status}");
            assert!(p.ours.join("d/c.txt").exists(), "[{label}] the file is still on disk");
        }
        p.assert_same(&label);
    }
}

// ── a zero-length index is corrupt, not empty (review T7) ──────────────────

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_zero_length_index_file_refuses_like_git_and_writes_nothing() {
    let setup = |d: &Path| {
        base(d);
        support::write_file(d, "a.txt", "alpha two\n");
        std::fs::write(d.join(".git/index"), b"").expect("truncate the index");
    };
    for args in [
        vec!["add", "a.txt"],
        vec!["add", "-A"],
        vec!["rm", "--cached", "a.txt"],
        vec!["mv", "a.txt", "z.txt"],
        vec!["reset"],
        vec!["restore", "--staged", "a.txt"],
    ] {
        let p = Pair::new(&setup);
        let fp = support::Fingerprint::take(&p.ours);
        let r = p.ours("", &args).await;
        let (code, _, twin_err) = p.twin(&args);
        assert_eq!(code, 128, "{args:?} git: {twin_err}");
        assert!(twin_err.contains("index file smaller than expected"), "{twin_err}");
        assert_eq!(r.code, 1, "{args:?}: {}", r.err);
        assert!(r.err.contains("index file smaller than expected"), "{args:?}: {}", r.err);
        assert!(support::Fingerprint::take(&p.ours).differences_from(&fp).is_empty(), "{args:?}: the index was rewritten");
        assert_eq!(std::fs::read(p.ours.join(".git/index")).expect("index"), b"", "{args:?}");
    }
}

// ── an index in version 4 is read, and written back as version 2 (review T8) ─

fn index_version(repo: &Path) -> u32 {
    let bytes = std::fs::read(repo.join(".git/index")).expect("index");
    u32::from_be_bytes(bytes[4..8].try_into().expect("header"))
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_version_4_index_is_read_and_rewritten_as_version_2() {
    let setup = |d: &Path| {
        base(d);
        support::git(d, &["update-index", "--index-version", "4"]);
        support::write_file(d, "a.txt", "alpha two\n");
        support::write_file(d, "new.txt", "new\n");
    };
    let p = Pair::new(&setup);
    assert_eq!(index_version(&p.ours), 4);
    let r = p.ours("", &["add", "-A"]).await;
    assert_eq!(r.code, 0, "{}", r.err);
    assert_eq!(p.twin(&["add", "-A"]).0, 0);
    // Git keeps version 4; this shell writes 2 (docs/issues.md W24). Every
    // other comparison still holds: entries, modes, ids and status agree.
    assert_eq!(index_version(&p.twin), 4);
    assert_eq!(index_version(&p.ours), 2);
    p.assert_same("v4 index");
}

// ── a pathspec glob matches the whole path, as git does ────────────────────

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_glob_without_a_slash_matches_the_whole_path_as_git_does() {
    let setup = |d: &Path| {
        base(d);
        support::write_file(d, "ca.txt", "root\n");
        support::write_file(d, "cab/x", "dir\n");
        support::write_file(d, "d/ca.txt", "nested\n");
        support::write_file(d, "a1.txt", "one\n");
        support::write_file(d, "a[1].txt", "bracket\n");
    };
    // Each spec on a fresh pair, so what one stages cannot hide another.
    for spec in ["ca*", "*.txt", "d/*", "?a.txt", "**/ca.txt", "a[1].txt", "c*b"] {
        let (r, t, p) = both(&setup, &["add", spec], "glob").await;
        assert_eq!(r.code == 0, t.0 == 0, "add {spec}: ours {} / git {}", r.err, t.2);
        p.assert_same(&format!("add {spec}"));
    }
    // The cases the table cannot tell apart if the rule is a basename match.
    let (_, _, p) = both(&setup, &["add", "ca*"], "glob").await;
    let staged = support::git(&p.ours, &["ls-files"]);
    assert!(staged.contains("ca.txt") && !staged.contains("d/ca.txt"), "ca* staged: {staged}");
}

// ── pins from the review: behavior no test covered ─────────────────────────

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn add_with_both_all_and_update_is_refused_as_git_refuses_it() {
    let setup = |d: &Path| {
        base(d);
        support::write_file(d, "a.txt", "alpha two\n");
        support::write_file(d, "new.txt", "new\n");
    };
    for args in [vec!["add", "-A", "-u"], vec!["add", "--all", "--update"]] {
        let p = Pair::new(&setup);
        let fp = support::Fingerprint::take(&p.ours);
        let r = p.ours("", &args).await;
        let (code, _, twin_err) = p.twin(&args);
        assert_eq!(code, 128, "git: {twin_err}");
        assert_eq!(r.code, 2, "{args:?} must not run: {}", r.err);
        assert!(r.err.contains("-u/--update and -A/--all cannot be used together") && r.err.contains("Nothing was changed."), "{}", r.err);
        assert!(support::Fingerprint::take(&p.ours).differences_from(&fp).is_empty(), "{args:?} changed the repository");
        p.assert_same(&args.join(" "));
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn rm_r_leaves_an_untracked_file_and_its_directory_in_place() {
    let setup = |d: &Path| {
        base(d);
        support::write_file(d, "d/untracked.txt", "u\n");
    };
    let (r, t, p) = both(&setup, &["rm", "-r", "d"], "rm -r with an untracked file").await;
    assert_eq!((r.code, i64::from(t.0)), (0, 0), "{} / {}", r.err, t.2);
    assert!(p.ours.join("d/untracked.txt").exists(), "the untracked file was deleted");
    assert!(!p.ours.join("d/c.txt").exists());
    p.assert_same("rm -r leaves untracked");
}

// ── containment of every blob before the first is written (GLM G-T12) ──────

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
async fn a_planted_link_at_a_later_blob_stops_add_before_any_blob_is_written() {
    let h = hostile();
    // Two new files whose blobs fan out to different directories; only the
    // second one's directory is a link.
    let first = (0..).map(|n| format!("first {n}\n")).find(|c| blob_hex(c)[..2] != blob_hex("second\n")[..2]).expect("content");
    support::write_file(&h.repo, "a1.txt", &first);
    support::write_file(&h.repo, "z1.txt", "second\n");
    let (first_hex, second_hex) = (blob_hex(&first), blob_hex("second\n"));
    std::os::unix::fs::symlink(&h.outside, h.repo.join(format!(".git/objects/{}", &second_hex[..2]))).expect("plant");
    let before = outside_snapshot(&h);
    let r = hostile_run(&h, "", &["add", "a1.txt", "z1.txt"]).await;
    assert_contained(&h, &r, "second blob's fan-out", &before);
    let loose = h.repo.join(format!(".git/objects/{}/{}", &first_hex[..2], &first_hex[2..]));
    assert!(!loose.exists(), "the first blob was written before the second was vetted");
    assert!(!h.repo.join(".git/index.lock").exists());
}

// ── pins from the second review (GLM) ──────────────────────────────────────

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn mv_into_the_git_directory_changes_nothing_and_its_negative_controls_move() {
    for args in [vec!["mv", "a.txt", ".git/x"], vec!["mv", "a.txt", ".git"], vec!["mv", "d", ".git/d"], vec!["mv", "a.txt", "d/.git"]] {
        let p = Pair::new(&base);
        let fp = support::Fingerprint::take(&p.ours);
        let r = p.ours("", &args).await;
        assert_ne!(r.code, 0, "{args:?} must refuse: {}", r.err);
        assert!(support::Fingerprint::take(&p.ours).differences_from(&fp).is_empty(), "{args:?} changed the repository");
        // Git does not refuse: it prints "invalid path" and still moves the
        // file into .git and stages the deletion (docs/issues.md W28), so the
        // twin is not an oracle for the result here.
    }
    // Source inside .git is untracked.
    let p = Pair::new(&base);
    let r = p.ours("", &["mv", ".git/config", "x"]).await;
    assert_ne!(r.code, 0, "{}", r.err);
    // Negative controls: names that only look like .git move, as in git.
    for dst in [".gitattributes", ".gitignore"] {
        let (r, t, p) = both(&base, &["mv", "a.txt", dst], dst).await;
        assert_eq!((r.code, i64::from(t.0)), (0, 0), "{dst}: {} / {}", r.err, t.2);
        assert!(p.ours.join(dst).exists());
        p.assert_same(dst);
    }
}

#[cfg(unix)]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn mv_into_a_read_only_directory_leaves_the_index_and_the_source_intact() {
    use std::os::unix::fs::PermissionsExt;
    let p = Pair::new(&base);
    let ro = p.ours.join("d");
    std::fs::set_permissions(&ro, std::fs::Permissions::from_mode(0o555)).expect("chmod");
    if std::fs::File::create(ro.join("probe")).is_ok() {
        std::fs::remove_file(ro.join("probe")).expect("probe");
        std::fs::set_permissions(&ro, std::fs::Permissions::from_mode(0o755)).expect("chmod");
        return; // root ignores directory permissions
    }
    let index_before = std::fs::read(p.ours.join(".git/index")).expect("index");
    let r = p.ours("", &["mv", "a.txt", "d"]).await;
    std::fs::set_permissions(&ro, std::fs::Permissions::from_mode(0o755)).expect("chmod");
    assert_ne!(r.code, 0, "{}", r.err);
    assert!(p.ours.join("a.txt").exists(), "the source moved");
    assert_eq!(std::fs::read(p.ours.join(".git/index")).expect("index"), index_before, "the index changed");
    assert!(!p.ours.join(".git/index.lock").exists());
}

// Attribute files (GLM T4, T5, T6, T7, T8). Each compares what is staged with
// `git hash-object`, which applies the attributes; a refusal is also fine.

async fn attr_case(label: &str, setup: &dyn Fn(&Path, &Path), file: &str, bytes: &[u8]) -> (kaish_types::ExecResult, String, Fixture, PathBuf) {
    support::require_git();
    let fx = Fixture::empty();
    let dir = fx.path("ours");
    std::fs::create_dir_all(&dir).expect("dir");
    support::git(&dir, &["init", "-q", "-b", "main"]);
    setup(&dir, &fx.root());
    let path = dir.join(file);
    std::fs::create_dir_all(path.parent().expect("parent")).expect("dir");
    std::fs::write(&path, bytes).expect("file");
    let (code, oracle, err) = git_raw(&dir, &["hash-object", file]);
    assert_eq!(code, 0, "[{label}] hash-object: {err}");
    let r = run_tool(&fx.root(), "/mnt/ours", &["add", "--", file]).await;
    (r, oracle.trim().to_string(), fx, dir)
}

fn staged_id(dir: &Path, file: &str) -> String {
    support::git(dir, &["ls-files", "-s", "--", file]).split_whitespace().nth(1).unwrap_or_default().to_string()
}

const CRLF_BYTES: &[u8] = b"a\r\nb\r\n";

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_attributes_file_that_is_a_directory_is_named_as_one() {
    let (r, _, _, _) = attr_case("dir", &|d, _| std::fs::create_dir_all(d.join(".gitattributes")).expect("dir"), "f.txt", CRLF_BYTES).await;
    assert_eq!(r.code, 4, "{}", r.err);
    assert!(r.err.contains("'.gitattributes'") && r.err.contains("a directory"), "{}", r.err);
    assert!(!r.err.contains("symbolic link"), "the wrong thing was found: {}", r.err);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn info_attributes_as_a_link_is_never_followed_out_of_the_mount() {
    // Inside the mount, and outside it: both must stage what git hashes, or refuse.
    for (label, outside) in [("inside", false), ("outside", true)] {
        let setup = move |d: &Path, root: &Path| {
            let target = if outside { root.parent().expect("parent").join(format!("kaish-attrs-outside-{}", std::process::id())) } else { root.join("ours/real-attributes") };
            std::fs::write(&target, "* text\n").expect("target");
            std::fs::create_dir_all(d.join(".git/info")).expect("info");
            let _ = std::fs::remove_file(d.join(".git/info/attributes"));
            std::os::unix::fs::symlink(&target, d.join(".git/info/attributes")).expect("link");
        };
        let (r, oracle, fx, dir) = attr_case(label, &setup, "f.txt", CRLF_BYTES).await;
        if r.code == 0 {
            assert_eq!(staged_id(&dir, "f.txt"), oracle, "[{label}] staged a different blob from git's");
        } else {
            assert_eq!(r.code, 4, "[{label}] {}", r.err);
            // The refusal names the mount root it was given; it must not name
            // where the link points.
            assert!(!r.err.contains("kaish-attrs-outside") && !r.err.contains("real-attributes"), "[{label}] leaked the link target: {}", r.err);
        }
        let _ = std::fs::remove_file(fx.root().parent().expect("parent").join(format!("kaish-attrs-outside-{}", std::process::id())));
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_attributes_file_over_one_mib_refuses_and_exactly_one_mib_is_read() {
    let pad = |total: usize, tail: &str| {
        let mut s = String::from("# padding\n");
        while s.len() + tail.len() < total {
            s.push('#');
        }
        s.push_str(tail);
        s.truncate(total);
        s
    };
    // Exactly 1 MiB, ending in a rule.
    let exact = {
        let tail = "\n* text\n";
        let mut s = pad(1024 * 1024 - tail.len(), "");
        s.push_str(tail);
        s
    };
    assert_eq!(exact.len(), 1024 * 1024);
    let (r, oracle, _, dir) = attr_case("1MiB", &|d, _| std::fs::write(d.join(".gitattributes"), &exact).expect("attrs"), "f.txt", CRLF_BYTES).await;
    // The rule at the end is read: the CRLF file is refused, not staged raw.
    assert_eq!(r.code, 4, "a 1 MiB file is read in full: {}", r.err);
    assert!(r.err.contains("would be stored differently"), "{}", r.err);
    let _ = (oracle, dir);
    let over = format!("{exact}#\n");
    let (r, _, _, _) = attr_case("over", &|d, _| std::fs::write(d.join(".gitattributes"), &over).expect("attrs"), "f.txt", CRLF_BYTES).await;
    assert_eq!(r.code, 4, "{}", r.err);
    assert!(r.err.contains("over 1 MiB"), "{}", r.err);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn info_attributes_has_higher_precedence_than_gitattributes() {
    // Same file, opposite settings: info/attributes wins in git.
    for (label, work, info) in [
        ("info unsets text", "* text\n", "* -text\n"),
        ("info sets text", "* -text\n", "* text\n"),
    ] {
        let setup = move |d: &Path, _: &Path| {
            std::fs::write(d.join(".gitattributes"), work).expect("attrs");
            std::fs::create_dir_all(d.join(".git/info")).expect("info");
            std::fs::write(d.join(".git/info/attributes"), info).expect("info attrs");
        };
        let (r, oracle, _fx, dir) = attr_case(label, &setup, "f.txt", CRLF_BYTES).await;
        if r.code == 0 {
            assert_eq!(staged_id(&dir, "f.txt"), oracle, "[{label}] staged a different blob from git's");
        } else {
            assert_eq!(r.code, 4, "[{label}] {}", r.err);
        }
        if label == "info unsets text" {
            assert_eq!(r.code, 0, "[{label}] info/attributes says -text, so the bytes are stored raw: {}", r.err);
        } else {
            assert_ne!(r.code, 0, "[{label}] info/attributes says text, so the file converts: staged raw");
        }
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_macro_defined_in_another_attributes_file_is_applied_as_git_applies_it() {
    // Used in the root file, defined in a deeper one, and the reverse. Either
    // stage what git hashes, or refuse; never stage raw bytes git converts.
    for (label, root_attrs, sub_attrs) in [
        ("used first, defined below", "* conv\n", "[attr]conv text\n"),
        ("defined first, used below", "[attr]conv text\n", "* conv\n"),
    ] {
        let setup = move |d: &Path, _: &Path| {
            std::fs::write(d.join(".gitattributes"), root_attrs).expect("attrs");
            std::fs::create_dir_all(d.join("sub")).expect("sub");
            std::fs::write(d.join("sub/.gitattributes"), sub_attrs).expect("sub attrs");
        };
        let (r, oracle, _fx, dir) = attr_case(label, &setup, "sub/f.txt", CRLF_BYTES).await;
        if r.code == 0 {
            assert_eq!(staged_id(&dir, "sub/f.txt"), oracle, "[{label}] staged a different blob from git's");
        } else {
            assert_eq!(r.code, 4, "[{label}] {}", r.err);
        }
    }
}

/// GLM T9 claimed git smudges a racy entry even when its content is unchanged.
/// Measured: the twin decides. A racy entry whose file still hashes to the
/// entry's id keeps its size, in git and here.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_racy_entry_with_unchanged_content_is_smudged_exactly_when_git_smudges_it() {
    use filetime::FileTime;
    for attempt in 0..20 {
        let setup = |d: &Path| {
            base(d);
            support::write_file(d, "x.txt", "aaaa\n");
            support::git(d, &["add", "x.txt"]);
            support::write_file(d, "other.txt", "other\n");
        };
        let p = Pair::new(&setup);
        let index_secs = FileTime::from_last_modification_time(&std::fs::metadata(p.ours.join(".git/index")).expect("index")).unix_seconds();
        let debug = support::git(&p.ours, &["ls-files", "--debug", "x.txt"]);
        let entry_secs: i64 = debug.lines().find(|l| l.trim_start().starts_with("mtime:")).expect("mtime").trim_start().trim_start_matches("mtime:").trim().split_once(':').expect("s:n").0.parse().expect("secs");
        if entry_secs != index_secs {
            assert!(attempt < 19, "never got the entry and the index into one second");
            continue;
        }
        std::thread::sleep(std::time::Duration::from_millis(1100));
        let r = p.ours("", &["add", "other.txt"]).await;
        assert_eq!(r.code, 0, "{}", r.err);
        assert_eq!(p.twin(&["add", "other.txt"]).0, 0);
        let (ours_x, twin_x) = (index_view_of(&p.ours, "x.txt"), index_view_of(&p.twin, "x.txt"));
        assert_eq!(ours_x, twin_x, "ours and git disagree on a racy entry whose content did not change");
        p.assert_same("racy, unchanged");
        return;
    }
}
