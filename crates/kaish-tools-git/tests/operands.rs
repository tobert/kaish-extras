//! Positional operands, git's spelling (docs/issues.md X2).
//!
//! Every verb used to carry a hidden clap sink that swallowed positionals and
//! discarded them, so `git log side` returned **exit 0 reporting HEAD's
//! history**: the tool confidently answered a different question than the one
//! asked, and nothing in the output said so. That is the failure mode an agent
//! cannot detect, which is what made it worse than a crash.
//!
//! These tests hold the two halves of the fix: an operand a verb accepts is
//! honored, and an operand a verb does not accept is refused loudly.

#[path = "support.rs"]
mod support;

use std::path::{Path, PathBuf};
use std::sync::Arc;

use kaish_tool_api::Tool;
use kaish_types::{ExecResult, ToolArgs, Value};

use kaish_tools_git::GitConfig;

use support::{git, require_git, write_file, Fixture, StrictBackend, TestCtx};

/// Run a verb with raw positional operands, the way the kernel binds them —
/// a literal `--` included, since that is how it reaches a tool.
async fn run(mount_real: &Path, cwd: &str, verb: &str, operands: &[&str]) -> ExecResult {
    let backend = Arc::new(StrictBackend::single(
        PathBuf::from("/mnt"),
        mount_real.to_path_buf(),
    ));
    let mut ctx = TestCtx::new(backend, cwd);
    let tool = kaish_tools_git::tool(GitConfig::read_only()).expect("config");
    let mut args = ToolArgs::new();
    args.positional.push(Value::String(verb.to_string()));
    for op in operands {
        args.positional.push(Value::String((*op).to_string()));
    }
    tool.execute(args, &mut ctx).await
}

fn json(result: &ExecResult) -> serde_json::Value {
    assert_eq!(result.code, 0, "failed: {}", result.err);
    result
        .output()
        .and_then(|o| o.rich_json.clone())
        .expect("--json carries the typed model")
}

/// A repository with two branches whose histories differ, so answering about
/// the wrong one is visible rather than a coincidence.
fn two_branches() -> (Fixture, PathBuf) {
    require_git();
    let fixture = Fixture::empty();
    let root = fixture.path("repo");
    std::fs::create_dir_all(&root).expect("create repo");

    git(&root, &["init", "--initial-branch=main", "--quiet"]);
    write_file(&root, "a.txt", "one\n");
    git(&root, &["add", "a.txt"]);
    git(&root, &["commit", "-m", "on main", "--quiet"]);

    git(&root, &["checkout", "--quiet", "-b", "side"]);
    write_file(&root, "b.txt", "two\n");
    git(&root, &["add", "b.txt"]);
    git(&root, &["commit", "-m", "on side", "--quiet"]);
    git(&root, &["checkout", "--quiet", "main"]);

    (fixture, root)
}

/// THE regression test. `git log side` must report the side branch, not HEAD.
///
/// Before the fix this returned exit 0 with `rev: "HEAD"` and main's single
/// commit — a wrong answer wearing a success code.
#[tokio::test]
async fn a_positional_revision_is_honored_not_swallowed() {
    let (fixture, root) = two_branches();
    let result = run(&fixture.root(), "/mnt/repo", "log", &["side"]).await;
    assert_eq!(result.code, 0, "stderr: {}", result.err);

    let model = json(&result);
    assert_eq!(model["rev"], "side", "the report echoes what was asked for");

    let summaries: Vec<&str> = model["commits"]
        .as_array()
        .expect("commits")
        .iter()
        .map(|c| c["summary"].as_str().expect("summary"))
        .collect();
    assert!(
        summaries.contains(&"on side"),
        "the side branch's own commit must be there: {summaries:?}"
    );

    // And it genuinely differs from HEAD, so this cannot pass by accident.
    let head = run(&fixture.root(), "/mnt/repo", "log", &[]).await;
    let head_model = json(&head);
    let head_summaries: Vec<&str> = head_model["commits"]
        .as_array()
        .expect("commits")
        .iter()
        .map(|c| c["summary"].as_str().expect("summary"))
        .collect();
    assert!(
        !head_summaries.contains(&"on side"),
        "HEAD must not contain it, or the assertion above proves nothing"
    );
    let _ = root;
}

/// `git log -- <path>` binds pathspecs, and `git log <rev> -- <path>` binds
/// both halves at once.
#[tokio::test]
async fn operands_after_the_marker_are_pathspecs() {
    let (fixture, _root) = two_branches();

    let only_b = run(&fixture.root(), "/mnt/repo", "log", &["side", "--", "b.txt"]).await;
    let model = json(&only_b);
    assert_eq!(model["rev"], "side");
    let summaries: Vec<&str> = model["commits"]
        .as_array()
        .expect("commits")
        .iter()
        .map(|c| c["summary"].as_str().expect("summary"))
        .collect();
    assert_eq!(
        summaries,
        vec!["on side"],
        "only the commit that touched b.txt"
    );
}

/// `git status <path>` and `git status -- <path>` both mean pathspecs —
/// status takes no revision, so the marker changes nothing.
#[tokio::test]
async fn status_operands_are_pathspecs_on_either_side_of_the_marker() {
    let (fixture, root) = two_branches();
    write_file(&root, "src/tracked.rs", "fn a() {}\n");
    write_file(&root, "docs/note.md", "note\n");

    for operands in [vec!["src"], vec!["--", "src"]] {
        let result = run(&fixture.root(), "/mnt/repo", "status", &operands).await;
        assert_eq!(result.code, 0, "status {operands:?}: {}", result.err);
        let paths: Vec<String> = json(&result)["entries"]
            .as_array()
            .expect("entries")
            .iter()
            .map(|e| e["path"].as_str().expect("path").to_string())
            .collect();
        assert!(
            paths.iter().all(|p| p.starts_with("src")),
            "the filter must apply: {operands:?} gave {paths:?}"
        );
        assert!(!paths.is_empty(), "and must not filter everything away");
    }
}

/// A verb that takes no operands says so rather than ignoring them.
#[tokio::test]
async fn info_refuses_operands_instead_of_discarding_them() {
    let (fixture, _root) = two_branches();
    let result = run(&fixture.root(), "/mnt/repo", "info", &["some-branch"]).await;
    assert_eq!(result.code, 2, "stderr: {}", result.err);
    assert!(
        result.err.contains("some-branch"),
        "the refusal names what was given: {}",
        result.err
    );
    assert!(
        result.err.contains("--repo"),
        "and names the flag that does what they probably meant: {}",
        result.err
    );
}

/// Two revisions, or a revision given twice, are refused rather than one
/// being picked silently.
#[tokio::test]
async fn a_revision_given_twice_is_a_usage_error() {
    let (fixture, _root) = two_branches();

    let two = run(&fixture.root(), "/mnt/repo", "log", &["main", "side"]).await;
    assert_eq!(two.code, 2, "stderr: {}", two.err);

    // The positional and the flag disagreeing is the same class of mistake.
    let backend = Arc::new(StrictBackend::single(
        PathBuf::from("/mnt"),
        fixture.root(),
    ));
    let mut ctx = TestCtx::new(backend, "/mnt/repo");
    let tool = kaish_tools_git::tool(GitConfig::read_only()).expect("config");
    let mut args = ToolArgs::new();
    args.positional.push(Value::String("log".into()));
    args.positional.push(Value::String("side".into()));
    args.named
        .insert("rev".to_string(), Value::String("main".to_string()));
    let both = tool.execute(args, &mut ctx).await;
    assert_eq!(both.code, 2, "stderr: {}", both.err);
    assert!(
        both.err.contains("side") && both.err.contains("main"),
        "the refusal names both spellings: {}",
        both.err
    );
}

/// `--rev HEAD` written explicitly alongside a positional is still a
/// conflict — but `--rev` left at its default is not, or every positional
/// revision would be refused.
#[tokio::test]
async fn the_default_rev_does_not_count_as_a_second_spelling() {
    let (fixture, _root) = two_branches();
    let result = run(&fixture.root(), "/mnt/repo", "log", &["side"]).await;
    assert_eq!(
        result.code, 0,
        "an unstated --rev must not conflict with a positional: {}",
        result.err
    );
}
