//! Repositories that lie about where their data lives.
//!
//! Architecture.md D.3's premise: repo-local `.git` content is
//! attacker-controlled the moment you open a repository you did not create,
//! which is the *normal* case for a codebase-analysis agent. Most of that
//! surface is answered structurally — nothing that could act on
//! `diff.*.textconv` is linked at all — but the paths a repository names for
//! itself are different, because we do have to read those.
//!
//! Three of them exist, and only one comes from discovery:
//!
//! | Path | Chosen by |
//! |---|---|
//! | `git_dir` | discovery — but a `.git` *file* can name it (`gitdir: …`) |
//! | `common_dir` | `<git_dir>/commondir`, a file that is entirely a path |
//! | `work_dir` | discovery's physical parent |
//!
//! A ceiling on discovery alone leaves the other two free. `commondir` in
//! particular redirects both the config read and the object store, so a
//! repository sitting honestly inside the mount could point kaish-git at any
//! directory on the host. These tests are what says it cannot.

#[path = "support.rs"]
mod support;

use std::path::PathBuf;
use std::sync::Arc;

use kaish_tool_api::Tool;
use kaish_types::{ExecResult, ToolArgs, Value};

use kaish_tools_git::GitConfig;

use support::{git, require_git, Fixture, StrictBackend, TestCtx};

/// A branch name that exists **only** in the repository outside the mount.
const SENTINEL_BRANCH: &str = "pwned-outside-the-mount";

/// What the outside repository looks like, for leak detection.
///
/// The branch name alone is not enough, and assuming it was is a mistake worth
/// recording: `git info` never lists branches, so a sentinel branch could
/// never have appeared in its output whether we read the outside repository or
/// not. The **HEAD oid** is the value `info` actually reports, so that is the
/// one that proves a read happened. Verified by construction — with the
/// ceiling check removed, `info` returns exactly this oid and exits 0.
struct Outside {
    /// The outside repository's HEAD oid — the value that leaks.
    head_oid: String,
}

/// Run `git info` with `mount_real` mounted at `/mnt`, from the given VFS cwd.
async fn info_at(mount_real: PathBuf, cwd: &str) -> ExecResult {
    let backend = Arc::new(StrictBackend::single(PathBuf::from("/mnt"), mount_real));
    let mut ctx = TestCtx::new(backend, cwd);
    let tool = kaish_tools_git::tool(GitConfig::read_only()).expect("config");

    let mut args = ToolArgs::new();
    args.positional.push(Value::String("info".to_string()));
    tool.execute(args, &mut ctx).await
}

/// Build the escape fixture.
///
/// Layout, with only `mounted/` inside the mount:
///
/// ```text
/// <scratch>/outside/        a real repository, with SENTINEL_BRANCH
/// <scratch>/mounted/        the mount root
/// <scratch>/mounted/repo/   an ordinary repository whose .git/commondir lies
/// ```
///
/// Returns the mount root and a description of what lies outside it.
fn escape_fixture(
    commondir_contents: impl FnOnce(&PathBuf, &PathBuf) -> String,
) -> (Fixture, PathBuf, Outside) {
    require_git();
    let fixture = Fixture::empty();

    // The prize: a genuine repository outside the mount, carrying a branch
    // name that appears nowhere else.
    let outside = fixture.path("outside");
    std::fs::create_dir_all(&outside).expect("create outside dir");
    git(&outside, &["init", "--initial-branch=main", "--quiet"]);
    support::write_file(&outside, "secret.txt", "host-side content\n");
    git(&outside, &["add", "."]);
    git(&outside, &["commit", "-m", "outside commit", "--quiet"]);
    git(&outside, &["branch", SENTINEL_BRANCH]);
    let outside_head = git(&outside, &["rev-parse", "HEAD"]);

    // An ordinary, honest-looking repository inside the mount.
    let mount = fixture.path("mounted");
    let repo = mount.join("repo");
    std::fs::create_dir_all(&repo).expect("create inside repo dir");
    git(&repo, &["init", "--initial-branch=main", "--quiet"]);
    support::write_file(&repo, "README.md", "inside the mount\n");
    git(&repo, &["add", "."]);
    git(&repo, &["commit", "-m", "inside commit", "--quiet"]);

    // The lie. `commondir` is where a linked worktree records its main
    // repository; nothing stops an ordinary repository from carrying one, and
    // nothing in git's format says the path has to stay local.
    let git_dir = repo.join(".git");
    let contents = commondir_contents(&git_dir, &outside);
    std::fs::write(git_dir.join("commondir"), contents).expect("write commondir");

    (
        fixture,
        mount,
        Outside {
            head_oid: outside_head,
        },
    )
}

/// Assert a result refused the escape without reading what lay beyond it.
fn assert_refused_without_reading(result: &ExecResult, outside: &Outside) {
    assert_eq!(
        result.code, 4,
        "a repository pointing outside the mount must be refused (exit 4), \
         got {}: {} {:?}",
        result.code,
        result.err,
        result.output()
    );
    assert!(
        result.err.contains("outside the mount"),
        "the refusal must say what happened: {}",
        result.err
    );
    assert!(
        result.err.contains("mount it too"),
        "the refusal must tell an embedder with a legitimate layout what to \
         do about it: {}",
        result.err
    );

    // The proof that nothing beyond the ceiling was read. The outside
    // repository's HEAD oid is what `info` would report if the common dir had
    // been followed — with the ceiling check removed this assertion fires,
    // which is what makes it a test of the fix rather than of the fixture.
    let rendered = format!("{} {:?} {:?}", result.err, result.output(), result.baggage);
    assert!(
        !rendered.contains(&outside.head_oid),
        "the outside repository's HEAD oid ({}) reached the caller — data \
         from beyond the mount was read: {rendered}",
        outside.head_oid
    );
    assert!(
        !rendered.contains(SENTINEL_BRANCH),
        "content from outside the mount reached the caller: {rendered}"
    );
}

/// The escape, in its most direct form: an absolute `commondir`.
///
/// Without the ceiling check on `common_dir`, this is where `read_repo_config`
/// and `gix_odb::at` are pointed — both at a host path the sandbox never
/// granted.
#[tokio::test]
async fn an_absolute_commondir_outside_the_mount_is_refused() {
    let (_fixture, mount, outside) =
        escape_fixture(|_git_dir, outside| outside.join(".git").display().to_string());

    let result = info_at(mount, "/mnt/repo").await;
    assert_refused_without_reading(&result, &outside);
}

/// The same escape spelled relatively. `commondir` is resolved against the
/// git dir, so enough `..` walks straight out of the mount — and lexical
/// normalization alone would happily produce the path.
#[tokio::test]
async fn a_dotdot_commondir_escaping_the_mount_is_refused() {
    let (_fixture, mount, outside) = escape_fixture(|_git_dir, _outside| {
        // <scratch>/mounted/repo/.git + ../../../outside/.git
        "../../../outside/.git".to_string()
    });

    let result = info_at(mount, "/mnt/repo").await;
    assert_refused_without_reading(&result, &outside);
}

/// `/etc` is the shape the escape takes in the wild: not another repository,
/// just somewhere on the host we should never look. It must be refused the
/// same way, and before anything is read.
#[tokio::test]
async fn a_commondir_pointing_at_a_system_directory_is_refused() {
    let (_fixture, mount, _outside) = escape_fixture(|_git_dir, _outside| "/etc".to_string());

    let result = info_at(mount, "/mnt/repo").await;
    assert_eq!(result.code, 4, "got {}: {}", result.code, result.err);
    assert!(
        result.err.contains("outside the mount"),
        "{}",
        result.err
    );
    assert!(
        !result.err.contains("/etc"),
        "the refusal must not echo the attacker-supplied path back — that \
         would make it an oracle for probing the host: {}",
        result.err
    );
}

/// Over-refusal is the other way to get this wrong. A `commondir` that stays
/// inside the mount is exactly how a linked worktree works, and it must keep
/// working.
#[tokio::test]
async fn a_commondir_inside_the_mount_still_works() {
    require_git();
    let fixture = Fixture::empty();
    let mount = fixture.path("mounted");
    let repo = mount.join("repo");
    std::fs::create_dir_all(&repo).expect("create repo dir");
    git(&repo, &["init", "--initial-branch=main", "--quiet"]);
    support::write_file(&repo, "README.md", "inside\n");
    git(&repo, &["add", "."]);
    git(&repo, &["commit", "-m", "inside", "--quiet"]);

    // A `commondir` naming the git dir itself: redundant, inside the mount,
    // and must be accepted.
    std::fs::write(repo.join(".git/commondir"), ".").expect("write commondir");

    let result = info_at(mount, "/mnt/repo").await;
    assert_eq!(
        result.code, 0,
        "a commondir inside the mount is legitimate: {}",
        result.err
    );
}

/// The real thing, end to end: a genuine `git worktree add` inside the mount,
/// where `commondir` points at the main repository a couple of directories up
/// but still inside. The regression guard that says the fix did not break the
/// feature it protects.
#[tokio::test]
async fn a_real_linked_worktree_inside_the_mount_still_works() {
    require_git();
    let fixture = Fixture::empty();
    let mount = fixture.path("mounted");
    let main = mount.join("main");
    std::fs::create_dir_all(&main).expect("create main dir");
    git(&main, &["init", "--initial-branch=main", "--quiet"]);
    support::write_file(&main, "README.md", "main\n");
    git(&main, &["add", "."]);
    git(&main, &["commit", "-m", "main", "--quiet"]);
    git(&main, &["branch", "side"]);

    let linked = mount.join("linked");
    git(
        &main,
        &[
            "worktree",
            "add",
            "--quiet",
            linked.to_str().expect("utf-8"),
            "side",
        ],
    );

    // Confirm the fixture really does have a commondir pointing elsewhere —
    // otherwise this test would pass without exercising the path at all.
    let private_git_dir = main.join(".git/worktrees/linked");
    assert!(
        private_git_dir.join("commondir").is_file(),
        "the fixture must have a real commondir to be a regression guard"
    );

    let backend = Arc::new(StrictBackend::single(PathBuf::from("/mnt"), mount));
    let mut ctx = TestCtx::new(backend, "/mnt/linked");
    let tool = kaish_tools_git::tool(GitConfig::read_only()).expect("config");
    let mut args = ToolArgs::new();
    args.positional.push(Value::String("info".to_string()));
    let result = tool.execute(args, &mut ctx).await;

    assert_eq!(
        result.code, 0,
        "a linked worktree whose common dir is inside the mount must work: {}",
        result.err
    );
}

/// A linked worktree whose main repository is genuinely outside the mount.
///
/// This is the honest case the refusal cannot distinguish from the hostile
/// one, and the design nuance worth stating: under the sandbox model the main
/// repository is not readable, so refusing is correct — but the message has to
/// tell the embedder that mounting the common dir is the fix, or they will
/// read this as a bug in the tool rather than a gap in their mount table.
#[tokio::test]
async fn a_legitimate_worktree_whose_common_dir_is_unmounted_is_refused_helpfully() {
    require_git();
    let fixture = Fixture::empty();

    // The main repository lives outside the mount entirely.
    let main = fixture.path("main");
    std::fs::create_dir_all(&main).expect("create main dir");
    git(&main, &["init", "--initial-branch=main", "--quiet"]);
    support::write_file(&main, "README.md", "main\n");
    git(&main, &["add", "."]);
    git(&main, &["commit", "-m", "main", "--quiet"]);
    git(&main, &["branch", "side"]);

    // Only the worktree is mounted.
    let mount = fixture.path("mounted");
    std::fs::create_dir_all(&mount).expect("create mount dir");
    let linked = mount.join("wt");
    git(
        &main,
        &[
            "worktree",
            "add",
            "--quiet",
            linked.to_str().expect("utf-8"),
            "side",
        ],
    );

    let result = info_at(mount, "/mnt/wt").await;
    assert_eq!(
        result.code, 4,
        "an unmounted common dir is an environment refusal, not a missing \
         repository: {}",
        result.err
    );
    assert!(
        result.err.contains("mount it too"),
        "the refusal must name the fix for the legitimate case: {}",
        result.err
    );
    assert!(
        result.err.contains("linked worktree"),
        "the refusal must name the legitimate shape it might be: {}",
        result.err
    );
}

/// Why there is no positive fixture for the `work_dir` ceiling check.
///
/// The check is defense in depth, and this test records the reasoning it
/// depends on so a future change that invalidates it is caught by a reader
/// rather than by an incident. Traced through gix-discover 0.54:
///
/// - `Path::WorkTree(wt)` yields `git_dir = wt.join(".git")`, so `work_dir`
///   is `git_dir`'s parent and cannot be outside a ceiling `git_dir` is
///   inside — unless the ceiling is itself named `.git`, which would require
///   mounting a git directory as the mount root.
/// - `Path::LinkedWorkTree { work_dir, git_dir }` takes `work_dir` from where
///   discovery physically walked and `git_dir` from the `.git` file's
///   `gitdir:` line. Only `git_dir` is repository-controlled.
/// - `Path::Repository(_)` has no work dir at all.
/// - Nothing in this crate reads `core.worktree`, which is the one config key
///   that would let a repository name its own working tree.
///
/// So today the `git_dir` check bounds `work_dir` transitively. What would
/// invalidate that, and require a fixture here: reading `core.worktree`,
/// honoring `extensions.worktreeConfig` (currently refused, `repo.rs`), or a
/// gix-discover release that lets a `gitdir:` file influence `work_dir`.
/// `submodule_count` reads `<work_dir>/.gitmodules`, so the blast radius is
/// real if any of those lands.
#[test]
fn work_dir_is_bounded_by_discovery() {
    // The assertion this reasoning actually rests on: no source file outside
    // the write path reads `core.worktree`. If that changes, the comment
    // above is stale and the check above stops being merely defensive.
    let src = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
    let mut found = Vec::new();
    let mut stack = vec![src.clone()];
    while let Some(dir) = stack.pop() {
        for entry in std::fs::read_dir(&dir).expect("read_dir") {
            let path = entry.expect("entry").path();
            if path.is_dir() {
                stack.push(path);
            } else if path.extension().is_some_and(|e| e == "rs") {
                let text = std::fs::read_to_string(&path).expect("read source");
                // Case-folded: git config keys are case-insensitive, and the
                // lookup would be spelled `core.worktree` either way.
                if text.to_ascii_lowercase().contains("\"core.worktree\"") {
                    found.push(path.strip_prefix(&src).expect("under src").to_path_buf());
                }
            }
        }
    }
    assert!(
        found.is_empty(),
        "core.worktree is now read in {found:?} — the work_dir ceiling check \
         is no longer merely defensive, and this test needs a real fixture \
         proving a repository cannot relocate its working tree outside the \
         mount"
    );
}
