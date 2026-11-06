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

pub async fn run_tool(root: &Path, cwd: &str, argv: &[&str]) -> kaish_types::ExecResult {
    let backend = Arc::new(StrictBackend::single(PathBuf::from("/mnt"), root));
    let mut ctx = TestCtx::new(backend, cwd);
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
