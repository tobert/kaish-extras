//! A pathspec glob matches as git's does, for `status`, `log` and `diff`
//! (`PathFilter` is shared), with real git as the oracle.
//!
//! Git matches a non-magic pathspec as a literal (the whole path, or the
//! spec as a directory prefix) or as a wildmatch against the WHOLE path,
//! anchored at the start, where `*` and `?` cross `/`. The basename rule of
//! `.gitignore` (a pattern without a slash matches the last component) is not
//! part of it: `ca*` matches `ca.txt` and `cab/x`, and not `d/ca.txt`.
//! Every spec below is held against `git ls-files`, `git status --porcelain`,
//! `git log --format=%H` and `git diff --name-only` on the same repository.

#[path = "support.rs"]
mod support;

use std::path::{Path, PathBuf};
use std::sync::Arc;

use kaish_tool_api::Tool;
use kaish_types::ExecResult;
use kaish_tools_git::GitConfig;

use support::{git, git_as, require_git, write_file, Fixture, StrictBackend, TestCtx};

const FILES: [&str; 8] = ["ca.txt", "cb.txt", "cab/x", "d/ca.txt", "d/e/ca.txt", "a1.txt", "a[1].txt", "d/other.md"];

/// Specs from the repository root. Each one pins a different clause of the rule.
const SPECS: [&str; 15] = [
    "*.txt", "ca*", "d/*", "d", "d/", "?a.txt", "**/ca.txt", "*/ca.txt", "a[1].txt", "[ac]1.txt", "a\\[1\\].txt", "c*b", "cab", "d*/ca*",
    "nomatch*",
];

/// Specs from inside `d`, where git prepends the prefix `d/` to the spec.
const SUB_SPECS: [&str; 6] = ["ca*", "*.txt", "e/ca*", "../ca*", "../*.txt", "."];

struct Repo {
    fixture: Fixture,
    root: PathBuf,
}

impl Repo {
    fn build() -> Self {
        require_git();
        let fixture = Fixture::empty();
        let root = fixture.path("repo");
        std::fs::create_dir_all(&root).expect("repo dir");
        git(&root, &["init", "-q", "-b", "main"]);
        for f in FILES {
            write_file(&root, f, &format!("{f}\n"));
        }
        let author = ("Fixture Author", "author@example.invalid");
        git(&root, &["add", "."]);
        git_as(&root, author, "2026-08-01T10:00:00+00:00", &["commit", "-q", "-m", "base"]);
        // One commit per file, so a log spec selects a distinct set of commits.
        for (n, f) in FILES.iter().enumerate() {
            write_file(&root, f, &format!("{f}\nsecond\n"));
            git(&root, &["add", "--", f]);
            let date = format!("2026-08-0{}T10:00:00+00:00", n + 2);
            git_as(&root, author, &date, &["commit", "-q", "-m", &format!("touch {f}")]);
        }
        // Unstaged edits and an untracked file for status and diff.
        for f in ["ca.txt", "cab/x", "d/ca.txt", "a1.txt", "a[1].txt"] {
            write_file(&root, f, &format!("{f}\nthird\n"));
        }
        write_file(&root, "newca.txt", "new\n");
        write_file(&root, "d/newca.txt", "new\n");
        Repo { fixture, root }
    }

    fn mount(&self) -> PathBuf {
        self.fixture.root()
    }

    async fn run(&self, verb: &str, cwd: &str, argv: &[&str]) -> ExecResult {
        let backend = Arc::new(StrictBackend::single(PathBuf::from("/mnt"), self.mount()));
        let mut ctx = TestCtx::new(backend, cwd);
        let tool = kaish_tools_git::tool(GitConfig::read_only()).expect("config");
        tool.execute(support::verb_words(verb, argv), &mut ctx).await
    }
}

fn model(result: &ExecResult, what: &str) -> serde_json::Value {
    assert_eq!(result.code, 0, "{what}: {}", result.err);
    result.output().and_then(|o| o.rich_json.clone()).expect("--json carries the typed model")
}

fn sorted(mut v: Vec<String>) -> Vec<String> {
    v.sort();
    v
}

fn lines(text: &str) -> Vec<String> {
    text.lines().map(str::to_string).collect()
}

/// Every check for one spec, run from `cwd` (a VFS path) and `dir` (its host twin).
async fn check(repo: &Repo, cwd: &str, dir: &Path, spec: &str) {
    let at = format!("spec {spec:?} from {cwd}");

    // status: the path column of git's porcelain rows (always root-relative).
    let ours = repo.run("status", cwd, &["--path", spec, "--untracked", "all", "--json"]).await;
    let m = model(&ours, &at);
    let mut got: Vec<String> = Vec::new();
    for e in m["entries"].as_array().expect("entries") {
        got.push(e["path"].as_str().expect("path").to_string());
    }
    got.sort();
    got.dedup();
    let want: Vec<String> = sorted(
        lines(&git(dir, &["status", "--porcelain", "-uall", "--", spec]))
            .into_iter()
            .map(|l| l[3..].to_string())
            .collect(),
    );
    assert_eq!(got, want, "status, {at}");

    // log: the commits that touched a matching path.
    let ours = repo.run("log", cwd, &["--path", spec, "--json"]).await;
    let got: Vec<String> =
        model(&ours, &at)["commits"].as_array().expect("commits").iter().map(|c| c["oid"].as_str().expect("oid").to_string()).collect();
    let want = lines(&git(dir, &["log", "--format=%H", "--", spec]));
    assert_eq!(got, want, "log, {at}");

    // diff: the unstaged change to a matching path.
    let ours = repo.run("diff", cwd, &["--path", spec, "--json"]).await;
    let got: Vec<String> =
        model(&ours, &at)["files"].as_array().expect("files").iter().map(|f| f["path"].as_str().expect("path").to_string()).collect();
    let want = lines(&git(dir, &["diff", "--name-only", "--", spec]));
    assert_eq!(sorted(got), sorted(want), "diff, {at}");
}

#[tokio::test]
async fn a_glob_selects_the_same_paths_as_git_in_status_log_and_diff() {
    let repo = Repo::build();
    for spec in SPECS {
        check(&repo, "/mnt/repo", &repo.root, spec).await;
    }
}

#[tokio::test]
async fn a_glob_from_a_subdirectory_is_the_prefix_and_the_spec_matched_whole() {
    let repo = Repo::build();
    let sub = repo.root.join("d");
    for spec in SUB_SPECS {
        check(&repo, "/mnt/repo/d", &sub, spec).await;
    }
}

/// Negative controls: the table must tell the two rules apart, or a
/// basename matcher would pass it.
#[tokio::test]
async fn the_table_tells_a_whole_path_match_from_a_basename_match() {
    let repo = Repo::build();
    let ls = |spec: &str| lines(&git(&repo.root, &["ls-files", "--", spec]));
    assert_eq!(ls("ca*"), ["ca.txt", "cab/x"], "git: a glob without a slash is not a basename match");
    assert!(ls("*.txt").contains(&"d/ca.txt".to_string()), "git: `*` crosses `/`");
    assert!(!ls("**/ca.txt").contains(&"ca.txt".to_string()), "git: `**/` needs a `/`");
    assert!(ls("a[1].txt").contains(&"a[1].txt".to_string()) && ls("a[1].txt").contains(&"a1.txt".to_string()), "git: both the literal and the class");
    let ours = repo.run("log", "/mnt/repo", &["--path", "ca*", "--json"]).await;
    let touched: Vec<String> = model(&ours, "ca*")["commits"]
        .as_array()
        .expect("commits")
        .iter()
        .map(|c| c["summary"].as_str().unwrap_or_default().to_string())
        .collect();
    assert!(touched.iter().all(|s| s != "touch d/ca.txt"), "ca* matched d/ca.txt: {touched:?}");
    assert!(touched.iter().any(|s| s == "touch ca.txt"), "ca* missed ca.txt: {touched:?}");
}
