//! `git ls` behavior, with real git as the oracle (architecture.md B.6, H
//! PR 4).
//!
//! `git ls-tree` is the oracle for every shape here: oid, mode, kind and path
//! must all agree (D8 in the PR4 design notes). Mode and oid are compared
//! byte for byte; `kind` is ours, so it is derived from git's own mode rather
//! than compared against git's `type` column directly — git's `ls-tree`
//! calls a symlink `blob` too, and only the mode marks it special.

#[path = "support.rs"]
mod support;

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use kaish_tool_api::Tool;
use kaish_types::{ExecResult, ToolArgs, Value};

use kaish_tools_git::{GitConfig, Limits};

use support::{git, StrictBackend, TestCtx, TreeRepo};

const MOUNT: &str = "/mnt";

/// Flags that take a value — see `readonly_fingerprint.rs`'s harness for why
/// this has to be explicit once bare positionals are in the mix.
const VALUE_FLAGS: &[&str] = &["repo", "limit"];
const BOOL_FLAGS: &[&str] = &["json", "recursive"];

fn tool_args(verb: &str, argv: &[&str]) -> ToolArgs {
    let mut args = ToolArgs::new();
    args.positional.push(Value::String(verb.to_string()));
    let mut i = 0;
    while i < argv.len() {
        let token = argv[i];
        let Some(name) = token.strip_prefix("--") else {
            args.positional.push(Value::String(token.to_string()));
            i += 1;
            continue;
        };
        if VALUE_FLAGS.contains(&name) {
            let value = argv
                .get(i + 1)
                .unwrap_or_else(|| panic!("'--{name}' takes a value, none followed in {argv:?}"));
            args.named.insert(name.to_string(), Value::String((*value).to_string()));
            i += 2;
        } else if BOOL_FLAGS.contains(&name) {
            args.flags.insert(name.to_string());
            i += 1;
        } else {
            panic!("'--{name}' is not classified in VALUE_FLAGS or BOOL_FLAGS");
        }
    }
    args
}

async fn ls(mount_real: &Path, cwd: &str, argv: &[&str]) -> ExecResult {
    ls_with(GitConfig::read_only(), mount_real, cwd, argv).await
}

async fn ls_with(config: GitConfig, mount_real: &Path, cwd: &str, argv: &[&str]) -> ExecResult {
    let backend = Arc::new(StrictBackend::single(PathBuf::from(MOUNT), mount_real.to_path_buf()));
    let mut ctx = TestCtx::new(backend, cwd);
    let tool = kaish_tools_git::tool(config).expect("config");
    tool.execute(tool_args("ls", argv), &mut ctx).await
}

fn json(result: &ExecResult) -> serde_json::Value {
    assert_eq!(result.code, 0, "ls failed: {}", result.err);
    result
        .output()
        .and_then(|o| o.rich_json.clone())
        .expect("--json carries the typed model")
}

/// `(mode, kind, oid, path)` rows from our tool's `--json`, sorted so set
/// comparison doesn't depend on traversal order.
fn rows(result: &ExecResult) -> BTreeSet<(String, String, String, String)> {
    json(result)["entries"]
        .as_array()
        .expect("entries is an array")
        .iter()
        .map(|e| {
            (
                e["mode"].as_str().unwrap().to_string(),
                e["kind"].as_str().unwrap().to_string(),
                e["oid"].as_str().unwrap().to_string(),
                e["path"].as_str().unwrap().to_string(),
            )
        })
        .collect()
}

/// The kind our vocabulary uses for a git tree mode — the mapping `git
/// ls-tree`'s oracle rows are translated through, since git's own `type`
/// column calls a symlink `blob` too (only the mode marks it special).
fn kind_from_mode(mode: &str) -> &'static str {
    match mode {
        "040000" => "dir",
        "120000" => "symlink",
        "160000" => "commit",
        _ => "file",
    }
}

/// The oracle: `git ls-tree [-r] <rev> [-- <path>]`, translated into the same
/// `(mode, kind, oid, path)` shape our tool reports.
fn git_ls_tree(root: &Path, rev: &str, path: Option<&str>, recursive: bool) -> BTreeSet<(String, String, String, String)> {
    let mut argv: Vec<&str> = vec!["ls-tree"];
    if recursive {
        argv.push("-r");
    }
    argv.push(rev);
    if let Some(p) = path {
        argv.push("--");
        argv.push(p);
    }
    let out = git(root, &argv);
    out.lines()
        .map(|line| {
            let (meta, path) = line.split_once('\t').expect("ls-tree line has a tab");
            let mut parts = meta.split_whitespace();
            let mode = parts.next().expect("mode").to_string();
            let _git_type = parts.next().expect("type");
            let oid = parts.next().expect("oid").to_string();
            let kind = kind_from_mode(&mode).to_string();
            (mode, kind, oid, path.to_string())
        })
        .collect()
}

// ═══════════════════════════════════════════════════════════════════════════
// D8: cross-checked against real git
// ═══════════════════════════════════════════════════════════════════════════

#[tokio::test]
async fn bare_ls_at_head_matches_git_ls_tree() {
    let repo = TreeRepo::build();
    let result = ls(&repo.scratch(), "/mnt/repo", &["--json"]).await;
    assert_eq!(rows(&result), git_ls_tree(&repo.root, "HEAD", None, false));
}

#[tokio::test]
async fn recursive_ls_matches_git_ls_tree_dash_r() {
    let repo = TreeRepo::build();
    let result = ls(&repo.scratch(), "/mnt/repo", &["--recursive", "--json"]).await;
    assert_eq!(rows(&result), git_ls_tree(&repo.root, "HEAD", None, true));
}

#[tokio::test]
async fn a_subdirectory_matches_git_ls_tree_with_a_path() {
    let repo = TreeRepo::build();
    let result = ls(&repo.scratch(), "/mnt/repo", &["HEAD", "src", "--json"]).await;
    assert_eq!(rows(&result), git_ls_tree(&repo.root, "HEAD", Some("src/"), false));
}

#[tokio::test]
async fn a_recursive_subdirectory_matches_git_ls_tree() {
    let repo = TreeRepo::build();
    let result = ls(&repo.scratch(), "/mnt/repo", &["HEAD", "src", "--recursive", "--json"]).await;
    assert_eq!(rows(&result), git_ls_tree(&repo.root, "HEAD", Some("src/"), true));
}

/// A path naming a single file reports one row — the same row `git ls-tree
/// <rev> -- <path>` reports for a non-directory pathspec.
#[tokio::test]
async fn a_path_naming_a_file_reports_one_row() {
    let repo = TreeRepo::build();
    let result = ls(&repo.scratch(), "/mnt/repo", &["HEAD", "README.md", "--json"]).await;
    let got = rows(&result);
    let want = git_ls_tree(&repo.root, "HEAD", Some("README.md"), false);
    assert_eq!(got, want);
    assert_eq!(got.len(), 1, "a file path is exactly one row: {got:?}");
}

/// A symlink's mode and kind agree with git — the one case where git's own
/// `type` column (`blob`) is not our vocabulary's word for it.
#[tokio::test]
async fn a_symlink_reports_the_symlink_kind() {
    let repo = TreeRepo::build();
    let result = ls(&repo.scratch(), "/mnt/repo", &["HEAD", "link.txt", "--json"]).await;
    let entries = json(&result)["entries"].as_array().unwrap().clone();
    assert_eq!(entries.len(), 1);
    assert_eq!(entries[0]["kind"], "symlink");
    assert_eq!(entries[0]["mode"], "120000");
    assert!(entries[0]["size"].is_number(), "a symlink has a size: {entries:?}");
}

/// A tag revision peels to its commit's tree, exactly as `git ls-tree
/// v1.0.0` does.
#[tokio::test]
async fn ls_at_a_tag_matches_git_ls_tree_at_the_tag() {
    let repo = TreeRepo::build();
    let result = ls(&repo.scratch(), "/mnt/repo", &["v1.0.0", "--json"]).await;
    assert_eq!(rows(&result), git_ls_tree(&repo.root, "v1.0.0", None, false));
}

/// A linked worktree reads its own HEAD, not the main repository's.
#[tokio::test]
async fn ls_from_a_linked_worktree_reads_its_own_head() {
    let repo = TreeRepo::build();
    let result = ls(&repo.scratch(), "/mnt/wt-side", &["--json"]).await;
    let git_root = repo.linked_worktree.clone();
    assert_eq!(rows(&result), git_ls_tree(&git_root, "HEAD", None, false));
}

/// A tree's own row reports `size: null` — a made-up zero would be a lie.
#[tokio::test]
async fn a_directory_row_has_a_null_size() {
    let repo = TreeRepo::build();
    let result = ls(&repo.scratch(), "/mnt/repo", &["--json"]).await;
    let entries = json(&result)["entries"].as_array().unwrap().clone();
    let src_row = entries
        .iter()
        .find(|e| e["path"] == "src")
        .expect("src is a top-level row");
    assert_eq!(src_row["kind"], "dir");
    assert!(src_row["size"].is_null(), "a tree's size must be null: {src_row}");
}

// ═══════════════════════════════════════════════════════════════════════════
// Limits and refusals
// ═══════════════════════════════════════════════════════════════════════════

#[tokio::test]
async fn limit_truncates_and_reports_it() {
    let repo = TreeRepo::build();
    let result = ls(&repo.scratch(), "/mnt/repo", &["--limit", "1", "--json"]).await;
    assert_eq!(result.code, 0);
    assert_eq!(json(&result)["truncated"], true);
    assert_eq!(json(&result)["entries"].as_array().unwrap().len(), 1);
    assert!(
        result.err.contains("truncated") && result.err.contains("--limit"),
        "a stderr note must accompany the JSON flag: {}",
        result.err
    );
}

/// The embedder's `max_rows` is a hard cap; `--limit` may only lower it.
#[tokio::test]
async fn embedder_row_cap_outranks_the_flag() {
    let repo = TreeRepo::build();
    let config = GitConfig::read_only().with_limits(Limits {
        max_rows: 1,
        ..Default::default()
    });
    let result = ls_with(config, &repo.scratch(), "/mnt/repo", &["--limit", "50", "--json"]).await;
    assert_eq!(json(&result)["entries"].as_array().unwrap().len(), 1);
    assert_eq!(json(&result)["truncated"], true);
}

/// A path this tree does not contain is a git-level "no" (exit 1) — the same
/// class git returns for a path outside a tree.
#[tokio::test]
async fn a_nonexistent_path_is_a_git_level_no() {
    let repo = TreeRepo::build();
    let result = ls(&repo.scratch(), "/mnt/repo", &["HEAD", "nonesuch"]).await;
    assert_eq!(result.code, 1, "stderr was: {}", result.err);
    assert!(result.err.contains("nonesuch"), "the error names the path: {}", result.err);
}

/// A path that walks through a *file* (not a directory) as a non-final
/// component is refused the same way — there is nothing beneath it to
/// descend into.
#[tokio::test]
async fn a_path_through_a_file_is_a_git_level_no() {
    let repo = TreeRepo::build();
    let result = ls(&repo.scratch(), "/mnt/repo", &["HEAD", "README.md/nonesuch"]).await;
    assert_eq!(result.code, 1, "stderr was: {}", result.err);
}

/// A tree deeper than `MAX_TREE_DEPTH` is refused loudly rather than walked
/// without limit or silently truncated.
#[tokio::test]
async fn a_tree_deeper_than_the_cap_is_refused() {
    let repo = TreeRepo::build();
    // 70 nested single-entry directories — comfortably past the 64-level cap.
    let mut rel = PathBuf::new();
    for i in 0..70 {
        rel.push(format!("d{i}"));
    }
    rel.push("leaf.txt");
    support::write_file(&repo.root, rel.to_str().unwrap(), "deep\n");
    git(&repo.root, &["add", "."]);
    git(&repo.root, &["commit", "-m", "a very deep tree", "--quiet"]);

    let result = ls(&repo.scratch(), "/mnt/repo", &["HEAD", "d0", "--recursive"]).await;
    assert_eq!(result.code, 1, "a too-deep tree is a git-level refusal, not a silent walk");
    assert!(
        result.err.contains("64"),
        "the refusal names the depth limit: {}",
        result.err
    );
}

/// D7: a subtracted verb is unroutable, not merely refused at runtime.
#[tokio::test]
async fn a_disabled_ls_verb_is_unroutable() {
    let repo = TreeRepo::build();
    let config = GitConfig::read_only().without_verb(kaish_tools_git::Verb::Ls);
    let result = ls_with(config, &repo.scratch(), "/mnt/repo", &[]).await;
    assert_ne!(result.code, 0, "a disabled verb cannot succeed");
}

// ═══════════════════════════════════════════════════════════════════════════
// A tree mode outside git's own vocabulary (docs/issues.md P2)
// ═══════════════════════════════════════════════════════════════════════════

/// Run `git` with `input` on stdin and return its trimmed stdout.
///
/// `support::git` has no stdin, and `git mktree` is the one writer that
/// stores the mode it is handed rather than canonicalizing it.
fn git_stdin(cwd: &Path, args: &[&str], input: &str) -> String {
    use std::io::Write;
    let out = std::process::Command::new("git")
        .args(args)
        .current_dir(cwd)
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_SYSTEM", "/dev/null")
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .and_then(|mut child| {
            child.stdin.take().expect("stdin").write_all(input.as_bytes())?;
            child.wait_with_output()
        })
        .unwrap_or_else(|e| panic!("git {args:?}: {e}"));
    assert!(
        out.status.success(),
        "git {args:?} failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8_lossy(&out.stdout).trim().to_string()
}

/// A tree entry recorded as `100664` lists as `100644` — and so does
/// `git ls-tree`'s own row for it.
///
/// Old repositories carry these: git wrote the umask into the mode before it
/// settled on `100644`/`100755`, and `git fsck` still reports them
/// (`badFilemode`). `git mktree` stores the mode it is handed, so this
/// fixture is a real tree object with `100664` in its bytes. Git's tree
/// reader canonicalizes on the way out (`canon_mode`, tree-walk.c), which is
/// what `treewalk::mode_of` does by reading the mode through `kind()` — so
/// the two agree, and the `mode` column stays a five-value vocabulary
/// (`100644`, `100755`, `120000`, `160000`, `040000`) an agent can match on.
#[tokio::test]
async fn a_noncanonical_tree_mode_prints_the_mode_git_ls_tree_prints() {
    support::require_git();
    let fixture = support::Fixture::empty();
    let root = fixture.path("repo");
    std::fs::create_dir_all(&root).expect("create repo dir");
    git(&root, &["init", "--initial-branch=main", "--quiet"]);
    git(&root, &["config", "gc.writeCommitGraph", "false"]);
    support::write_file(&root, "f.txt", "hi\n");
    git(&root, &["add", "f.txt"]);
    git(&root, &["commit", "-m", "init", "--quiet"]);
    let blob = git(&root, &["rev-parse", "HEAD:f.txt"]);
    let tree = git_stdin(&root, &["mktree"], &format!("100664 blob {}\tf.txt\n", blob.trim()));
    let commit = git(&root, &["commit-tree", tree.trim(), "-m", "odd mode"]);
    let commit = commit.trim();

    // The negative control: the tree object really does carry `100664`. Every
    // git writer except `mktree` folds it to `100644` on the way in, so
    // without this the test could pass on a canonical fixture and prove
    // nothing.
    let raw = git(&root, &["cat-file", "tree", tree.trim()]);
    assert!(
        raw.starts_with("100664 f.txt"),
        "the fixture tree must hold the raw mode: {raw:?}"
    );

    let result = ls(&fixture.root(), "/mnt/repo", &[commit, "--json"]).await;
    let ours = rows(&result);
    assert_eq!(
        ours,
        git_ls_tree(&root, commit, None, false),
        "our mode column must be git ls-tree's, byte for byte"
    );
    assert!(
        ours.iter().all(|(mode, ..)| mode == "100644"),
        "git prints 100644 for this entry, and so do we: {ours:?}"
    );
}
