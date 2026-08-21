//! `git show` behavior, with real git as the oracle (architecture.md B.5, H
//! PR 4).
//!
//! The phasing gate for this PR names one proof explicitly: a blob byte-cap,
//! and `show HEAD:path` round-tripping binary content. `binary_content_round_trips_byte_for_byte`
//! is that test. Everything else here cross-checks the other three object
//! kinds against real git the same way.

#[path = "support.rs"]
mod support;

use std::path::{Path, PathBuf};
use std::sync::Arc;

use kaish_tool_api::Tool;
use kaish_types::{ExecResult, ToolArgs, Value};

use kaish_tools_git::{GitConfig, Limits};

use support::{git, StrictBackend, TestCtx, TreeRepo};

const MOUNT: &str = "/mnt";

const VALUE_FLAGS: &[&str] = &["repo", "limit"];
const BOOL_FLAGS: &[&str] = &["json"];

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

async fn show(mount_real: &Path, cwd: &str, argv: &[&str]) -> ExecResult {
    show_with(GitConfig::read_only(), mount_real, cwd, argv).await
}

async fn show_with(config: GitConfig, mount_real: &Path, cwd: &str, argv: &[&str]) -> ExecResult {
    let backend = Arc::new(StrictBackend::single(PathBuf::from(MOUNT), mount_real.to_path_buf()));
    let mut ctx = TestCtx::new(backend, cwd);
    let tool = kaish_tools_git::tool(config).expect("config");
    tool.execute(tool_args("show", argv), &mut ctx).await
}

fn json(result: &ExecResult) -> serde_json::Value {
    assert_eq!(result.code, 0, "show failed: {}", result.err);
    result
        .output()
        .and_then(|o| o.rich_json.clone())
        .expect("--json carries the typed model")
}

/// `git log -1 --format=<fmt> <rev>` — one field of commit metadata, the
/// oracle for `show`'s commit form.
fn git_commit_field(root: &Path, rev: &str, fmt: &str) -> String {
    git(root, &["log", "-1", &format!("--format={fmt}"), rev])
}

// ═══════════════════════════════════════════════════════════════════════════
// D8: the blob form, and the phasing gate's named proof
// ═══════════════════════════════════════════════════════════════════════════

/// The phasing gate, verbatim: `show HEAD:path` round-trips binary content.
/// The fixture blob carries NUL bytes and genuinely invalid UTF-8, so this
/// exercises `OutputPayload::Bytes`, not the lossy `Text` path — a build that
/// silently mangled either class would fail here, not pass by accident.
#[tokio::test]
async fn binary_content_round_trips_byte_for_byte() {
    let repo = TreeRepo::build();
    let result = show(&repo.scratch(), "/mnt/repo", &["HEAD:data.bin"]).await;
    assert_eq!(result.code, 0, "show failed: {}", result.err);
    assert!(result.is_bytes(), "non-UTF-8 content must not be coerced to text");
    assert_eq!(
        result.out_bytes().expect("binary payload"),
        TreeRepo::binary_bytes().as_slice(),
        "the blob must come back byte-identical"
    );

    // And against the oracle directly: `git cat-file blob HEAD:data.bin`
    // reads the exact same object.
    let oracle = std::process::Command::new("git")
        .args(["cat-file", "blob", "HEAD:data.bin"])
        .current_dir(&repo.root)
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_SYSTEM", "/dev/null")
        .output()
        .expect("git cat-file blob");
    assert_eq!(oracle.stdout, TreeRepo::binary_bytes());
}

/// A text blob at a revision agrees with `git cat-file blob`, byte for byte.
#[tokio::test]
async fn a_text_blob_matches_git_cat_file_blob() {
    let repo = TreeRepo::build();
    let result = show(&repo.scratch(), "/mnt/repo", &["HEAD:README.md"]).await;
    assert_eq!(result.code, 0, "show failed: {}", result.err);
    let expected = git(&repo.root, &["cat-file", "blob", "HEAD:README.md"]);
    assert_eq!(result.text_out(), format!("{expected}\n"));
}

/// A blob over the embedder's `max_blob_bytes` is capped: `size` is the real
/// size, content is withheld (not a truncated prefix), and a stderr note
/// says so by name.
#[tokio::test]
async fn a_blob_over_the_cap_is_declined_and_says_so() {
    let repo = TreeRepo::build();
    let config = GitConfig::read_only().with_limits(Limits {
        max_blob_bytes: 3,
        ..Default::default()
    });
    let result = show_with(config, &repo.scratch(), "/mnt/repo", &["HEAD:README.md"]).await;
    assert_eq!(result.code, 0, "a capped blob is still a successful read");
    assert_eq!(result.text_out(), "", "content must be withheld, not partially served");
    assert_eq!(result.baggage.get("git.size").map(String::as_str), Some("6"));
    assert!(
        result.err.contains("max_blob_bytes") && result.err.contains('6') && result.err.contains('3'),
        "the notice names the real size and the cap: {}",
        result.err
    );
}

// ═══════════════════════════════════════════════════════════════════════════
// D8: commit, tag, and tree forms
// ═══════════════════════════════════════════════════════════════════════════

#[tokio::test]
async fn commit_metadata_matches_git_log() {
    let repo = TreeRepo::build();
    let result = show(&repo.scratch(), "/mnt/repo", &["HEAD", "--json"]).await;
    let commit = json(&result);
    assert_eq!(commit["kind"], "commit");
    assert_eq!(commit["oid"], repo.rev_parse("HEAD"));
    assert_eq!(commit["author"]["name"], git_commit_field(&repo.root, "HEAD", "%an"));
    assert_eq!(commit["author"]["email"], git_commit_field(&repo.root, "HEAD", "%ae"));
    assert_eq!(
        commit["committer"]["name"],
        git_commit_field(&repo.root, "HEAD", "%cn")
    );
    assert_eq!(commit["summary"], git_commit_field(&repo.root, "HEAD", "%s"));
    // `show` always carries the body, unlike `log` (which gates it on
    // `--body`) — a single commit is the whole answer `show` gives.
    assert!(commit["body"].is_string(), "show always carries the body: {commit}");
}

#[tokio::test]
async fn tag_metadata_matches_git_cat_file_tag() {
    let repo = TreeRepo::build();
    let result = show(&repo.scratch(), "/mnt/repo", &["v1.0.0", "--json"]).await;
    let tag = json(&result);
    assert_eq!(tag["kind"], "tag");
    assert_eq!(tag["oid"], repo.rev_parse("v1.0.0"));
    assert_eq!(tag["name"], "v1.0.0");
    assert_eq!(tag["tagger"]["name"], "Tag Tagger");
    assert_eq!(tag["tagger"]["email"], "tagger@example.invalid");
    assert!(
        tag["message"].as_str().unwrap().starts_with("release notes"),
        "the tag's own message: {tag}"
    );
    // "then the tagged object" — the commit this tag points at, described
    // the same way `show` would describe it directly.
    assert_eq!(tag["target_oid"], repo.rev_parse("HEAD"));
    assert_eq!(tag["target"]["kind"], "commit");
    assert_eq!(tag["target"]["oid"], repo.rev_parse("HEAD"));
}

/// `(mode, oid, path)` rows from a JSON `entries` array, sorted so set
/// comparison doesn't depend on traversal order.
fn tree_rows(entries: &serde_json::Value) -> std::collections::BTreeSet<(String, String, String)> {
    entries
        .as_array()
        .expect("entries is an array")
        .iter()
        .map(|e| {
            (
                e["mode"].as_str().unwrap().to_string(),
                e["oid"].as_str().unwrap().to_string(),
                e["path"].as_str().unwrap().to_string(),
            )
        })
        .collect()
}

/// The tree form shares its row shape with `ls` — same fields, cross-checked
/// against `git ls-tree` directly, the same oracle `ls`'s own tests use.
#[tokio::test]
async fn the_tree_form_matches_git_ls_tree() {
    let repo = TreeRepo::build();
    let result = show(&repo.scratch(), "/mnt/repo", &["HEAD:src", "--json"]).await;
    let tree = json(&result);
    assert_eq!(tree["kind"], "tree");

    let oracle = git(&repo.root, &["ls-tree", "HEAD", "--", "src/"]);
    let expected: std::collections::BTreeSet<(String, String, String)> = oracle
        .lines()
        .map(|line| {
            let (meta, path) = line.split_once('\t').expect("ls-tree line has a tab");
            let mut parts = meta.split_whitespace();
            let mode = parts.next().unwrap().to_string();
            let _type = parts.next().unwrap();
            let oid = parts.next().unwrap().to_string();
            (mode, oid, path.to_string())
        })
        .collect();
    assert_eq!(tree_rows(&tree["entries"]), expected);
}

/// A `--limit` on the tree form is reported the same way `ls` reports it.
#[tokio::test]
async fn tree_form_limit_truncates_and_reports_it() {
    let repo = TreeRepo::build();
    let result = show(&repo.scratch(), "/mnt/repo", &["HEAD:", "--limit", "1", "--json"]).await;
    let tree = json(&result);
    assert_eq!(tree["kind"], "tree");
    assert_eq!(tree["truncated"], true);
    assert_eq!(tree["entries"].as_array().unwrap().len(), 1);
}

/// A linked worktree resolves its own HEAD.
#[tokio::test]
async fn show_from_a_linked_worktree_reads_its_own_head() {
    let repo = TreeRepo::build();
    let result = show(&repo.scratch(), "/mnt/wt-side", &["--json"]).await;
    let commit = json(&result);
    assert_eq!(commit["kind"], "commit");
    assert_eq!(commit["oid"], git(&repo.linked_worktree, &["rev-parse", "HEAD"]));
}

// ═══════════════════════════════════════════════════════════════════════════
// Revspec grammar: D2, D3
// ═══════════════════════════════════════════════════════════════════════════

/// D3: `@` resolves like `HEAD` for `show` too, including with a colon path —
/// the case that makes the alias bite (`show @:file`).
#[tokio::test]
async fn bare_at_sign_resolves_like_head() {
    let repo = TreeRepo::build();
    let head = show(&repo.scratch(), "/mnt/repo", &["HEAD", "--json"]).await;
    let at = show(&repo.scratch(), "/mnt/repo", &["@", "--json"]).await;
    assert_eq!(json(&at)["oid"], json(&head)["oid"]);

    let head_blob = show(&repo.scratch(), "/mnt/repo", &["HEAD:README.md"]).await;
    let at_blob = show(&repo.scratch(), "/mnt/repo", &["@:README.md"]).await;
    assert_eq!(at_blob.text_out(), head_blob.text_out());
}

/// `@{...}` reflog syntax stays refused — the alias is a substitution for
/// the bare token `@` only, not a new door into a form this crate has always
/// refused.
#[tokio::test]
async fn at_brace_syntax_is_still_refused() {
    let repo = TreeRepo::build();
    let result = show(&repo.scratch(), "/mnt/repo", &["@{upstream}"]).await;
    assert_eq!(result.code, 2, "@{{...}} must still be refused");
    assert!(result.err.contains("@{"), "the error names the form: {}", result.err);
}

/// `:/text` (git's find-commit-by-message form) is refused by name, checked
/// before the generic colon split — splitting first would read it as an
/// empty revision plus a `/text` path.
#[tokio::test]
async fn find_by_message_syntax_is_refused_by_name() {
    let repo = TreeRepo::build();
    let result = show(&repo.scratch(), "/mnt/repo", &[":/initial"]).await;
    assert_eq!(result.code, 2);
    assert!(result.err.contains(":/"), "the error names the form: {}", result.err);
}

/// An empty revision half (`:path`, `::`) is a loud usage error, not treated
/// as a resolvable (empty) revision.
#[tokio::test]
async fn an_empty_revision_before_the_colon_is_a_usage_error() {
    let repo = TreeRepo::build();
    for spec in [":README.md", "::"] {
        let result = show(&repo.scratch(), "/mnt/repo", &[spec]).await;
        assert_eq!(result.code, 2, "{spec} must be a usage error: {}", result.err);
    }
}

/// A path this tree does not contain is a git-level "no" (exit 1).
#[tokio::test]
async fn a_nonexistent_path_is_a_git_level_no() {
    let repo = TreeRepo::build();
    let result = show(&repo.scratch(), "/mnt/repo", &["HEAD:nonesuch"]).await;
    assert_eq!(result.code, 1, "stderr was: {}", result.err);
    assert!(result.err.contains("nonesuch"));
}

/// `HEAD:` naming a blob directly (a revision that resolves to a blob, not a
/// tree or commit) is refused — there is no tree to descend into. Building
/// one requires a hand-built oid `<rev>:` navigation, so this is exercised
/// through `--rev`-style resolution: a tree entry itself, walked one level
/// too far, since a blob's own hash has no path grammar to combine with.
#[tokio::test]
async fn a_blob_revision_with_a_path_suffix_is_refused() {
    let repo = TreeRepo::build();
    let blob_oid = git(&repo.root, &["rev-parse", "HEAD:README.md"]);
    let result = show(&repo.scratch(), "/mnt/repo", &[&format!("{blob_oid}:x")]).await;
    assert_eq!(result.code, 1, "a blob has no tree to read a path from: {}", result.err);
}

/// D7: a subtracted verb is unroutable, not merely refused at runtime.
#[tokio::test]
async fn a_disabled_show_verb_is_unroutable() {
    let repo = TreeRepo::build();
    let config = GitConfig::read_only().without_verb(kaish_tools_git::Verb::Show);
    let result = show_with(config, &repo.scratch(), "/mnt/repo", &[]).await;
    assert_ne!(result.code, 0, "a disabled verb cannot succeed");
}
