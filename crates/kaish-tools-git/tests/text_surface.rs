//! The text surface of the table verbs, held against real git.
//!
//! `log --oneline` is asserted exactly in `spellings.rs`. The others (status,
//! branch, tag, worktree list) used to be asserted only through the typed
//! table, so a string in `render.rs` could change with every test green. Here
//! each is compared, byte for byte, with text built from real git's own output
//! for the same fixture. Where this build's layout deliberately differs from
//! git's (a tab between columns, an oid column, a `lightweight`/`annotated`
//! kind), the expected text is built from git's fields and the difference is
//! the join, so the test fails on a wrong field and on a wrong layout.

#[path = "support.rs"]
mod support;

use std::path::{Path, PathBuf};
use std::sync::Arc;

use kaish_kernel::{Kernel, KernelConfig};
use kaish_tool_api::KernelBackend;

use kaish_tools_git::GitConfig;

use support::{git, require_git, write_file, Fixture, StrictBackend};

/// Two branches, a remote-tracking ref, tags of both kinds, and one change of
/// each status class.
fn repo() -> (Fixture, PathBuf) {
    require_git();
    let fixture = Fixture::empty();
    let root = fixture.path("repo");
    std::fs::create_dir_all(&root).expect("create repo dir");
    git(&root, &["init", "--initial-branch=main", "--quiet"]);
    write_file(&root, "a.txt", "one\n");
    write_file(&root, "src/lib.rs", "pub fn a() {}\n");
    write_file(&root, "docs/readme.md", "docs\n");
    write_file(&root, "gone.txt", "bye\n");
    git(&root, &["add", "."]);
    git(&root, &["commit", "-m", "first", "--quiet"]);
    write_file(&root, "a.txt", "two\n");
    git(&root, &["commit", "-am", "second", "--quiet"]);
    git(&root, &["branch", "docs", "HEAD~1"]);
    git(&root, &["update-ref", "refs/remotes/origin/main", "HEAD~1"]);
    git(&root, &["config", "remote.origin.url", "/nonexistent/remote.git"]);
    git(&root, &["config", "remote.origin.fetch", "+refs/heads/*:refs/remotes/origin/*"]);
    git(&root, &["config", "branch.docs.remote", "origin"]);
    git(&root, &["config", "branch.docs.merge", "refs/heads/main"]);
    git(&root, &["config", "branch.main.remote", "origin"]);
    git(&root, &["config", "branch.main.merge", "refs/heads/main"]);
    git(&root, &["tag", "v1"]);
    git(&root, &["tag", "-a", "v2", "-m", "second release"]);
    // Staged, unstaged, deleted, untracked.
    write_file(&root, "docs/readme.md", "docs, staged\n");
    git(&root, &["add", "docs/readme.md"]);
    write_file(&root, "src/lib.rs", "pub fn c() {}\n");
    std::fs::remove_file(root.join("gone.txt")).expect("delete");
    write_file(&root, "untracked.txt", "u\n");
    (fixture, root)
}

async fn text(root: &Path, script: &str) -> String {
    let backend: Arc<dyn KernelBackend> =
        Arc::new(StrictBackend::single(PathBuf::from("/mnt"), root.to_path_buf()));
    let git = kaish_tools_git::tool(GitConfig::read_only()).expect("valid config");
    let mut cfg = KernelConfig::transient();
    cfg.cwd = PathBuf::from("/mnt");
    let k = Kernel::with_backend(backend, cfg, |_| {}, |tools| tools.register(git))
        .expect("kernel assembles");
    let r = k.execute(script).await.unwrap_or_else(|e| panic!("{script:?}: {e}"));
    assert_eq!(r.code, 0, "{script:?} failed: {}", r.err);
    r.text_out().to_string()
}

fn short7(root: &Path, rev: &str) -> String {
    git(root, &["rev-parse", "--short=7", rev])
}

#[tokio::test]
async fn status_text_is_gits_porcelain_with_a_tab_after_the_two_letters() {
    let (_f, root) = repo();
    let porcelain = git(&root, &["status", "--porcelain=v1", "--untracked-files=normal"]);
    assert!(porcelain.lines().count() >= 4, "negative control: every class is present: {porcelain}");
    // XY, then the path; porcelain separates them with a space and this build
    // with a tab, and its order is path order within a class.
    let mut expected: Vec<String> = porcelain.lines().map(|l| format!("{}\t{}", &l[..2], &l[3..])).collect();
    let mut ours: Vec<String> = text(&root, "git status").await.lines().map(String::from).collect();
    expected.sort();
    ours.sort();
    assert_eq!(ours, expected);
}

#[tokio::test]
async fn branch_text_is_marker_name_oid_upstream() {
    let (_f, root) = repo();
    let fmt = "%(HEAD) %(refname:short)\t%(objectname:short=7)\t%(upstream:short)";
    let theirs = git(&root, &["branch", &format!("--format={fmt}")]);
    assert!(theirs.contains("\torigin/main"), "negative control: an upstream is shown: {theirs}");
    // An empty last column is not printed, so a line ends where its content does.
    let theirs: Vec<&str> = theirs.lines().map(str::trim_end).collect();
    assert_eq!(text(&root, "git branch").await.lines().collect::<Vec<_>>(), theirs);
}

#[tokio::test]
async fn branch_all_and_remote_text_match_gits_names() {
    let (_f, root) = repo();
    let col0 = |t: &str| t.lines().map(|l| l.split('\t').next().unwrap().to_string()).collect::<Vec<_>>();
    let all = text(&root, "git branch --all").await;
    assert_eq!(col0(&all), git(&root, &["branch", "-a", "--no-color"]).lines().map(String::from).collect::<Vec<_>>());
    let remote = text(&root, "git branch --remote").await;
    assert_eq!(col0(&remote), git(&root, &["branch", "-r", "--no-color"]).lines().map(String::from).collect::<Vec<_>>());
    assert_eq!(remote.trim_end(), format!("  origin/main\t{}", short7(&root, "origin/main")));
}

#[tokio::test]
async fn tag_text_is_name_target_kind_summary() {
    let (_f, root) = repo();
    let target = short7(&root, "HEAD");
    // Lightweight `v1` has no message; annotated `v2` shows its subject.
    assert_eq!(git(&root, &["cat-file", "-t", "v1"]), "commit");
    assert_eq!(git(&root, &["cat-file", "-t", "v2"]), "tag");
    let expected = format!("v1\t{target}\tlightweight\t\nv2\t{target}\tannotated\tsecond release");
    assert_eq!(text(&root, "git tag").await.trim_end(), expected);
}

#[tokio::test]
async fn worktree_list_text_is_path_branch_state() {
    let (_f, root) = repo();
    let branch = git(&root, &["branch", "--show-current"]);
    let listed = git(&root, &["worktree", "list", "--porcelain"]);
    assert!(listed.contains(&format!("branch refs/heads/{branch}")), "{listed}");
    // The path is the VFS path the caller can use, not the host's.
    assert_eq!(text(&root, "git worktree list").await.trim_end(), format!("/mnt\t{branch}"));
}
