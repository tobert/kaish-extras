//! Discovery is ceilinged at the VFS mount root (architecture.md E.2).
//!
//! v2 introduces upward repository discovery for ergonomics — `git info` from
//! a subdirectory works, where v1's exact-root open did not. The ceiling is
//! what keeps that new capability from becoming an escape v1 never had: a
//! repository *above* the mount root must be invisible from inside the mount,
//! because from inside the mount it is not reachable.
//!
//! The interesting cases are the boundaries. gix-discover computes its ceiling
//! height by lexical prefix match and silently drops a ceiling it cannot match
//! — including when the start path *is* the ceiling, which is the ordinary
//! "`git info` at the top of a mount" invocation. `ReadRepo::discover`'s
//! post-check is what covers that, and `an_outer_repo_is_invisible_from_the_
//! mount_root` is the test that would fail if it were removed.

#[path = "support.rs"]
mod support;

use std::path::PathBuf;
use std::sync::Arc;

use kaish_tool_api::Tool;
use kaish_types::{ExecResult, ToolArgs, Value};

use kaish_tools_git::GitConfig;

use support::{git, require_git, Fixture, StrictBackend, TestCtx, VirtualBackend};

/// Run `git info` with `mount_real` mounted at `/mnt` and the given VFS cwd.
async fn info_at(mount_real: PathBuf, cwd: &str, repo_arg: Option<&str>) -> ExecResult {
    let backend = Arc::new(StrictBackend::single(PathBuf::from("/mnt"), mount_real));
    let mut ctx = TestCtx::new(backend, cwd);
    let git = kaish_tools_git::tool(GitConfig::read_only()).expect("config");

    let mut args = ToolArgs::new();
    args.positional.push(Value::String("info".to_string()));
    if let Some(repo) = repo_arg {
        args.named
            .insert("repo".to_string(), Value::String(repo.to_string()));
    }
    git.execute(args, &mut ctx).await
}

/// A repository at `<scratch>/outer`, with a plain directory tree at
/// `<scratch>/outer/inner/deep` that is *not* a repository.
///
/// Mounting `outer/inner` means the only repository reachable by walking up
/// is above the mount root — exactly the escape the ceiling forbids.
fn outer_repo_with_inner_tree() -> (Fixture, PathBuf, PathBuf) {
    require_git();
    let fixture = Fixture::empty();
    let outer = fixture.path("outer");
    std::fs::create_dir_all(outer.join("inner/deep")).expect("create inner tree");
    git(&outer, &["init", "--initial-branch=main", "--quiet"]);
    support::write_file(&outer, "README.md", "outer repo\n");
    git(&outer, &["add", "."]);
    git(&outer, &["commit", "-m", "outer", "--quiet"]);
    let inner = outer.join("inner");
    (fixture, outer, inner)
}

/// The headline: from a subdirectory inside the mount, a repository above the
/// mount root is not discovered. Exit 1, "no repository", not the outer repo.
#[tokio::test]
async fn an_outer_repo_is_invisible_from_inside_the_mount() {
    let (_fixture, outer, inner) = outer_repo_with_inner_tree();

    let result = info_at(inner, "/mnt/deep", None).await;
    assert_eq!(
        result.code, 1,
        "discovery escaped the mount and found {}: {:?}",
        outer.display(),
        result.output()
    );
    assert!(
        result.err.contains("no repository"),
        "the refusal must say what happened: {}",
        result.err
    );
    assert!(
        result.err.contains("mount root"),
        "the refusal must name the ceiling that stopped it: {}",
        result.err
    );
    assert!(
        !result.err.contains("outer/.git"),
        "the refusal must not leak the path of a repository outside the \
         sandbox: {}",
        result.err
    );
}

/// The boundary gix-discover itself gets wrong: when the start path *is* the
/// mount root, its ceiling height is zero and the ceiling is dropped, so its
/// walk is unbounded. The post-check in `ReadRepo::discover` is the only
/// thing standing between that and an escape.
#[tokio::test]
async fn an_outer_repo_is_invisible_from_the_mount_root() {
    let (_fixture, outer, inner) = outer_repo_with_inner_tree();

    let result = info_at(inner, "/mnt", None).await;
    assert_eq!(
        result.code, 1,
        "discovery from the mount root escaped and found {}: {:?}",
        outer.display(),
        result.output()
    );
}

/// The same, reached through `--repo` rather than the cwd, since that is a
/// second path into the bridge.
#[tokio::test]
async fn the_repo_flag_cannot_escape_the_mount_either() {
    let (_fixture, _outer, inner) = outer_repo_with_inner_tree();

    let result = info_at(inner.clone(), "/mnt", Some("/mnt/deep")).await;
    assert_eq!(result.code, 1, "--repo escaped the mount: {:?}", result.output());

    // `..` in the argument is normalized lexically by `resolve_path` before it
    // ever reaches the host, so it climbs out of the *VFS* rather than out of
    // the host filesystem: the resulting path belongs to no mount, and the
    // bridge refuses it (exit 4) before discovery is even reached. A
    // different code from the case above, and the right one — nothing about
    // this invocation is a statement about whether a repository exists.
    let result = info_at(inner, "/mnt/deep", Some("../../..")).await;
    assert_eq!(
        result.code, 4,
        "a `..`-bearing --repo escaped the mount: {:?}",
        result.output()
    );
    assert!(
        result.err.contains("not on a real filesystem")
            || result.err.contains("no mount contains"),
        "the refusal must name the missing mapping: {}",
        result.err
    );
}

/// The ceiling must not be so tight that it breaks the ordinary case: a
/// repository *at* the mount root is inside the mount and must be found, both
/// from the root itself and from a subdirectory.
#[tokio::test]
async fn a_repo_at_the_mount_root_is_found() {
    require_git();
    let fixture = Fixture::empty();
    let root = fixture.path("repo");
    std::fs::create_dir_all(root.join("src")).expect("create src");
    git(&root, &["init", "--initial-branch=main", "--quiet"]);
    support::write_file(&root, "src/lib.rs", "fn main() {}\n");
    git(&root, &["add", "."]);
    git(&root, &["commit", "-m", "only commit", "--quiet"]);

    for cwd in ["/mnt", "/mnt/src"] {
        let result = info_at(root.clone(), cwd, None).await;
        assert_eq!(
            result.code, 0,
            "a repository at the mount root must be discoverable from {cwd}: {}",
            result.err
        );
    }
}

/// A repository nested below the mount root is found by walking up from a
/// subdirectory — the ergonomic v2 added, asserted so the ceiling work cannot
/// quietly disable it.
#[tokio::test]
async fn upward_discovery_works_below_the_ceiling() {
    require_git();
    let fixture = Fixture::empty();
    let scratch = fixture.root();
    let repo = fixture.path("project");
    std::fs::create_dir_all(repo.join("a/b/c")).expect("create nested tree");
    git(&repo, &["init", "--initial-branch=main", "--quiet"]);
    support::write_file(&repo, "a/b/c/file.txt", "deep\n");
    git(&repo, &["add", "."]);
    git(&repo, &["commit", "-m", "deep", "--quiet"]);

    let result = info_at(scratch, "/mnt/project/a/b/c", None).await;
    assert_eq!(
        result.code, 0,
        "upward discovery below the ceiling must work: {}",
        result.err
    );
    let json = result
        .output()
        .and_then(|o| o.rich_json.clone())
        .expect("structured output");
    assert_eq!(json["repo_root_vfs"], "/mnt/project");
}

/// E.2's other refusal: a path that maps to no real filesystem is exit 4, not
/// a confusing "no repository". D.5 says this in the embedder guide too,
/// because the opposite belief is the dangerous one.
#[tokio::test]
async fn a_virtual_mount_is_refused_with_exit_four() {
    let backend = Arc::new(VirtualBackend::single(PathBuf::from("/mem")));
    let mut ctx = TestCtx::new(backend, "/mem/project");
    let git = kaish_tools_git::tool(GitConfig::read_only()).expect("config");

    let mut args = ToolArgs::new();
    args.positional.push(Value::String("info".to_string()));
    let result = git.execute(args, &mut ctx).await;

    assert_eq!(result.code, 4, "a memory mount is an environment refusal");
    assert!(
        result.err.contains("not on a real filesystem"),
        "the refusal must say why: {}",
        result.err
    );
}

/// A directory inside the mount that is simply not a repository is exit 1 and
/// says so — distinct from the exit 4 environment refusals.
#[tokio::test]
async fn a_plain_directory_is_not_a_repository() {
    let fixture = Fixture::empty();
    let plain = fixture.path("plain");
    std::fs::create_dir_all(&plain).expect("create plain dir");

    let result = info_at(plain, "/mnt", None).await;
    assert_eq!(result.code, 1);
    assert!(result.err.starts_with("git info:"), "{}", result.err);
}
