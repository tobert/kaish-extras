//! `git worktree list` against real git's `--porcelain` output
//! (architecture.md B.9).
//!
//! Read-side worktree enumeration is genuinely read-only, so it ships in the
//! read profile even though the rest of `worktree` waits on the ledger.
//! `git worktree list --porcelain` is the oracle for every row: the paths, the
//! HEAD oids, the branches, the lock and its reason, and which registration is
//! prunable.
//!
//! The half real git has no opinion about is `path_vfs`, because git has no
//! VFS. B.9's rule — null when the worktree lives outside every mount — is
//! tested by mounting the same fixture two ways: the scratch root, where every
//! sibling worktree is reachable, and the repository alone, where none of them
//! is.

#[path = "support.rs"]
mod support;

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use kaish_tool_api::Tool;
use kaish_types::{ExecResult, ToolArgs, Value};

use kaish_tools_git::{GitConfig, Verb};

use support::{require_git, StrictBackend, TestCtx, WorktreeRepo};

/// Run `git worktree list` with `mount_real` mounted at `/mnt` and the caller
/// standing at `cwd`.
async fn run(mount_real: &Path, cwd: &str, argv: &[&str]) -> ExecResult {
    let backend = Arc::new(StrictBackend::single(
        PathBuf::from("/mnt"),
        mount_real.to_path_buf(),
    ));
    let mut ctx = TestCtx::new(backend, cwd);
    let tool = kaish_tools_git::tool(GitConfig::read_only()).expect("config");
    let mut args = ToolArgs::new();
    args.positional.push(Value::String("worktree".to_string()));
    args.positional.push(Value::String("list".to_string()));
    let mut i = 0;
    while i < argv.len() {
        match argv[i].strip_prefix("--") {
            Some(name) if name == "json" => {
                args.flags.insert(name.to_string());
                i += 1;
            }
            Some(name) => {
                args.named.insert(
                    name.to_string(),
                    Value::String(argv[i + 1].to_string()),
                );
                i += 2;
            }
            None => {
                args.positional.push(Value::String(argv[i].to_string()));
                i += 1;
            }
        }
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

/// Rows keyed by `path_real`, which is the only identifier every worktree has
/// — the main working tree has no registration name.
fn rows_by_path(model: &serde_json::Value) -> BTreeMap<String, serde_json::Value> {
    model["worktrees"]
        .as_array()
        .expect("worktrees array")
        .iter()
        .map(|row| {
            (
                row["path_real"].as_str().expect("path_real").to_string(),
                row.clone(),
            )
        })
        .collect()
}

/// The whole listing, against `git worktree list --porcelain`: same paths, in
/// the same order, with the same HEAD oid and branch on every row.
#[tokio::test]
async fn the_listing_matches_real_gits_porcelain() {
    let repo = WorktreeRepo::build();
    let result = run(&repo.scratch(), "/mnt/repo", &["--json"]).await;
    let model = json(&result);

    let oracle = repo.porcelain();
    assert!(
        oracle.len() >= 5,
        "the fixture must register more than a couple of worktrees: {oracle:?}"
    );

    let ours = model["worktrees"].as_array().expect("worktrees array");
    let our_paths: Vec<&str> = ours
        .iter()
        .map(|r| r["path_real"].as_str().expect("path_real"))
        .collect();
    let git_paths: Vec<&str> = oracle
        .iter()
        .map(|r| r["worktree"].as_str())
        .collect();
    assert_eq!(
        our_paths, git_paths,
        "the worktree paths and their order must match git's"
    );

    for (ours, theirs) in ours.iter().zip(oracle.iter()) {
        let path = theirs["worktree"].as_str();
        assert_eq!(
            ours["head_oid"].as_str(),
            theirs.get("HEAD").map(String::as_str),
            "HEAD oid for {path}"
        );
        let git_branch = theirs.get("branch").map(|b| {
            b.strip_prefix("refs/heads/").unwrap_or(b).to_string()
        });
        assert_eq!(
            ours["branch"].as_str().map(str::to_string),
            git_branch,
            "branch for {path}"
        );
        assert_eq!(
            ours["locked"].as_bool(),
            Some(theirs.contains_key("locked")),
            "locked for {path}"
        );
        assert_eq!(
            ours["prunable"].as_bool(),
            Some(theirs.contains_key("prunable")),
            "prunable for {path} (git said {theirs:?})"
        );
    }
}

/// The main working tree is the row with no registration name, and it comes
/// first — git's own order, and the one an agent reading the first row
/// expects.
#[tokio::test]
async fn the_main_working_tree_is_first_and_unnamed() {
    let repo = WorktreeRepo::build();
    let model = json(&run(&repo.scratch(), "/mnt/repo", &["--json"]).await);
    let first = &model["worktrees"][0];
    assert_eq!(
        first["path_real"].as_str().map(Path::new),
        Some(repo.root.as_path())
    );
    assert!(
        first["name"].is_null(),
        "the main working tree has no registration under .git/worktrees, so it \
         has no name: {first}"
    );
    // Negative control: every other row does have one.
    for row in model["worktrees"].as_array().expect("array").iter().skip(1) {
        assert!(
            row["name"].is_string(),
            "a linked worktree is registered by name: {row}"
        );
    }
}

/// The lock and its reason come off the `locked` file, and a detached HEAD is
/// a null branch rather than an invented one.
#[tokio::test]
async fn lock_reason_and_detached_head_are_reported() {
    let repo = WorktreeRepo::build();
    let model = json(&run(&repo.scratch(), "/mnt/repo", &["--json"]).await);
    let rows = rows_by_path(&model);

    let locked = &rows[repo.locked.to_str().expect("utf-8")];
    assert_eq!(locked["locked"], true);
    assert_eq!(locked["lock_reason"], "held for review");

    let detached = &rows[repo.detached.to_str().expect("utf-8")];
    assert!(
        detached["branch"].is_null(),
        "a detached worktree names no branch: {detached}"
    );
    assert!(
        detached["head_oid"].is_string(),
        "but it still has a HEAD: {detached}"
    );

    // Negative control: an ordinary worktree is neither locked nor detached.
    let plain = &rows[repo.plain.to_str().expect("utf-8")];
    assert_eq!(plain["locked"], false);
    assert!(plain["lock_reason"].is_null());
    assert_eq!(plain["branch"], "wt-plain-branch");
}

/// A registration whose directory is gone is prunable, and it says why.
#[tokio::test]
async fn a_deleted_worktree_directory_is_prunable_with_a_reason() {
    let repo = WorktreeRepo::build();
    let model = json(&run(&repo.scratch(), "/mnt/repo", &["--json"]).await);
    let rows = rows_by_path(&model);

    let gone = &rows[repo.gone.to_str().expect("utf-8")];
    assert_eq!(gone["prunable"], true, "{gone}");
    assert!(
        gone["prunable_reason"]
            .as_str()
            .expect("a reason")
            .contains("no longer"),
        "the reason must say what is missing: {gone}"
    );
    // Negative control: a worktree that is still there is not prunable, and
    // carries no reason.
    let plain = &rows[repo.plain.to_str().expect("utf-8")];
    assert_eq!(plain["prunable"], false);
    assert!(plain["prunable_reason"].is_null());
}

/// B.9's `path_vfs` rule, both halves. Mounting the scratch root puts every
/// worktree inside the mount; mounting the repository alone leaves every
/// sibling outside it, and an agent is told so with a null rather than shown
/// a VFS path that resolves to something else.
#[tokio::test]
async fn path_vfs_is_null_for_a_worktree_outside_the_mount() {
    let repo = WorktreeRepo::build();

    // Wide mount: everything is reachable, so nothing is null.
    let wide = json(&run(&repo.scratch(), "/mnt/repo", &["--json"]).await);
    for row in wide["worktrees"].as_array().expect("array") {
        assert!(
            row["path_vfs"].is_string(),
            "every worktree is inside this mount: {row}"
        );
    }
    let wide_rows = rows_by_path(&wide);
    assert_eq!(
        wide_rows[repo.plain.to_str().expect("utf-8")]["path_vfs"],
        "/mnt/wt-plain"
    );

    // Narrow mount: only the repository itself, so every sibling worktree is
    // a path the caller can see named but cannot reach.
    let narrow = json(&run(&repo.root, "/mnt", &["--json"]).await);
    let narrow_rows = rows_by_path(&narrow);
    assert_eq!(
        narrow_rows[repo.root.to_str().expect("utf-8")]["path_vfs"], "/mnt",
        "the repository itself is still reachable"
    );
    assert!(
        narrow_rows[repo.plain.to_str().expect("utf-8")]["path_vfs"].is_null(),
        "a sibling worktree is outside this mount"
    );
    assert!(
        narrow_rows[repo.locked.to_str().expect("utf-8")]["path_vfs"].is_null()
    );
    // The nested one is inside the working tree, so it is reachable either way
    // — the control that keeps the assertions above from passing because
    // `path_vfs` is always null under a narrow mount.
    assert_eq!(
        narrow_rows[repo.inside.to_str().expect("utf-8")]["path_vfs"],
        "/mnt/nested/wt-inside"
    );
}

/// A worktree the mount cannot reach is **not probed**. `prunable` is null
/// rather than a guess, because answering it would mean stat-ing a host path
/// the repository chose — a one-bit existence oracle for anything outside the
/// sandbox, which is exactly what `repo.rs`'s `contain` refuses to give.
#[tokio::test]
async fn a_worktree_outside_the_mount_is_named_but_not_examined() {
    let repo = WorktreeRepo::build();
    let narrow = json(&run(&repo.root, "/mnt", &["--json"]).await);
    let rows = rows_by_path(&narrow);

    let plain = &rows[repo.plain.to_str().expect("utf-8")];
    assert!(plain["path_vfs"].is_null());
    assert!(
        plain["prunable"].is_null(),
        "prunability outside the mount is not ours to answer: {plain}"
    );
    // The registration's own metadata still comes through, because all of it
    // lives under the common dir, inside the mount.
    assert_eq!(plain["name"], "wt-plain");
    assert_eq!(plain["branch"], "wt-plain-branch");
    assert!(plain["head_oid"].is_string());

    // The gone worktree is outside this mount too, so even though it IS
    // prunable, we do not say so — the honest answer is that we did not look.
    let gone = &rows[repo.gone.to_str().expect("utf-8")];
    assert!(gone["prunable"].is_null(), "{gone}");

    // Negative control: under this same narrow mount, a worktree that IS
    // inside gets a real answer, so `prunable: null` above is about the mount
    // and not about the verb having stopped working.
    let inside = &rows[repo.inside.to_str().expect("utf-8")];
    assert_eq!(inside["prunable"], false, "{inside}");
}

/// Listing from inside a linked worktree reports the whole repository, not
/// just the worktree the caller is standing in.
#[tokio::test]
async fn listing_from_inside_a_linked_worktree_sees_them_all() {
    let repo = WorktreeRepo::build();
    let from_main = json(&run(&repo.scratch(), "/mnt/repo", &["--json"]).await);
    let from_linked = json(&run(&repo.scratch(), "/mnt/wt-plain", &["--json"]).await);
    assert_eq!(
        from_main["worktrees"], from_linked["worktrees"],
        "the set of worktrees is a property of the repository, not of where \
         the caller stands"
    );
}

/// `--limit` bounds the rows and says so, and the count of registrations is
/// reported whether or not the rows were cut.
#[tokio::test]
async fn limit_truncates_and_reports() {
    let repo = WorktreeRepo::build();
    let result = run(&repo.scratch(), "/mnt/repo", &["--json", "--limit", "2"]).await;
    let model = json(&result);
    assert_eq!(model["worktrees"].as_array().expect("array").len(), 2);
    assert_eq!(model["truncated"], true);
    assert!(
        result.err.contains("--limit"),
        "truncation is reported on stderr too: {}",
        result.err
    );

    // Negative control: without the limit nothing is truncated.
    let full = run(&repo.scratch(), "/mnt/repo", &["--json"]).await;
    assert_eq!(json(&full)["truncated"], false);
    assert!(full.err.is_empty(), "stderr: {}", full.err);
}

/// The verb takes no operands, and says so rather than answering a different
/// question — the `git info /some/path` defect, which cost exit 0 and a wrong
/// answer.
#[tokio::test]
async fn operands_are_refused() {
    let repo = WorktreeRepo::build();
    let result = run(&repo.scratch(), "/mnt/repo", &["wt-plain"]).await;
    assert_eq!(result.code, 2, "stderr: {}", result.err);
    assert!(result.err.contains("wt-plain"), "{}", result.err);
}

/// `git worktree` with no subcommand names the one this build has rather than
/// guessing, and the refusal is a usage error.
#[tokio::test]
async fn worktree_without_a_subcommand_names_the_read_profiles_only_one() {
    let repo = WorktreeRepo::build();
    let backend = Arc::new(StrictBackend::single(
        PathBuf::from("/mnt"),
        repo.scratch(),
    ));
    let mut ctx = TestCtx::new(backend, "/mnt/repo");
    let tool = kaish_tools_git::tool(GitConfig::read_only()).expect("config");
    let mut args = ToolArgs::new();
    args.positional.push(Value::String("worktree".to_string()));
    let result = tool.execute(args, &mut ctx).await;
    assert_eq!(result.code, 2, "stderr: {}", result.err);
    assert!(
        result.err.contains("worktree list"),
        "the refusal names the subcommand that exists: {}",
        result.err
    );
}

/// The verb is subtractable like every other, and subtracting it takes the
/// whole `worktree` node with it — there is no bare `git worktree` left
/// behind advertising a subcommand this build will not run.
#[tokio::test]
async fn subtracting_the_verb_removes_the_worktree_node() {
    require_git();
    let tool = kaish_tools_git::tool(GitConfig::read_only().without_verb(Verb::WorktreeList))
        .expect("config");
    let schema = tool.schema();
    let names: Vec<&str> = schema.subcommands.iter().map(|s| s.name.as_str()).collect();
    assert!(
        !names.contains(&"worktree"),
        "the parent node must go with its only child: {names:?}"
    );
    // Negative control: it is there when the verb is enabled.
    let full = kaish_tools_git::tool(GitConfig::read_only()).expect("config");
    let schema = full.schema();
    let names: Vec<&str> = schema.subcommands.iter().map(|s| s.name.as_str()).collect();
    assert!(names.contains(&"worktree"), "{names:?}");
}
