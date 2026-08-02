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

use std::path::{Path, PathBuf};
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

/// The symlink spelling of the same escape, and the one a lexical ceiling
/// check cannot see.
///
/// `commondir` names `evil`, a symlink *inside* `.git` pointing outside the
/// mount. Lexically `<git_dir>/evil` is inside the ceiling; to `openat` it is
/// not. Nothing in kaish's VFS ever inspects this path — we build it ourselves
/// out of repository content, after `resolve_real_path` has already approved
/// the perfectly legitimate `/mnt/repo` — so the containment check `LocalFs`
/// performs never gets a chance to run. Only canonicalizing before the
/// comparison catches it.
#[tokio::test]
async fn a_symlink_inside_dot_git_cannot_smuggle_the_common_dir_out() {
    let (_fixture, mount, outside) = escape_fixture(|git_dir, outside| {
        std::os::unix::fs::symlink(outside.join(".git"), git_dir.join("evil"))
            .expect("plant the symlink inside .git");
        // Relative, and lexically innocent: `<git_dir>/evil` is under the
        // ceiling by every string comparison there is.
        "evil".to_string()
    });

    let result = info_at(mount, "/mnt/repo").await;
    assert_refused_without_reading(&result, &outside);
}

/// The `..`-through-a-symlink spelling, and why the two path helpers must run
/// in the order they do.
///
/// `<git_dir>/evil/..` is `<outside>` to the kernel, because `..` applies to
/// what `evil` *points at*. It never gets that far: `resolve_common_dir`
/// normalizes lexically first, folding `evil/..` back to `<git_dir>` before
/// any syscall, and `canonicalize` then confirms that result is inside the
/// ceiling. The repository reads its own data and the symlink is never
/// traversed.
///
/// Lexical-then-canonical is the safe order, and this test pins it. Canonical
/// alone would resolve `evil` and land outside; lexical alone would miss the
/// plain `evil` spelling the previous test covers. Each helper closes the
/// other's hole, which is why neither may be dropped as redundant.
#[tokio::test]
async fn a_dotdot_through_a_symlink_folds_back_inside_and_reads_our_own_data() {
    let (_fixture, mount, outside) = escape_fixture(|git_dir, outside| {
        // `evil` -> <outside>/.git, so to the kernel `evil/..` is <outside>.
        std::os::unix::fs::symlink(outside.join(".git"), git_dir.join("evil"))
            .expect("plant the symlink inside .git");
        "evil/..".to_string()
    });

    let result = info_at(mount, "/mnt/repo").await;
    assert_eq!(
        result.code, 0,
        "lexical normalization folds this back to the repository's own git \
         dir, which is readable: {}",
        result.err
    );

    // The assertion that matters whichever way it resolved: nothing from
    // beyond the mount reached the caller.
    let rendered = format!("{} {:?} {:?}", result.err, result.output(), result.baggage);
    assert!(
        !rendered.contains(&outside.head_oid),
        "the outside repository's HEAD oid reached the caller: {rendered}"
    );
}

/// A mount that is itself reached through a symlink must still work.
///
/// This is what over-canonicalizing would break: resolve one side of the
/// comparison and not the other and every repository under a symlinked mount
/// root is refused. `/tmp` on macOS is exactly this shape, so it is not a
/// hypothetical.
#[tokio::test]
async fn a_symlinked_mount_root_still_works() {
    require_git();
    let fixture = Fixture::empty();
    let real = fixture.path("real-mount");
    let repo = real.join("repo");
    std::fs::create_dir_all(&repo).expect("create repo");
    git(&repo, &["init", "--initial-branch=main", "--quiet"]);
    support::write_file(&repo, "README.md", "inside\n");
    git(&repo, &["add", "README.md"]);
    git(&repo, &["commit", "-m", "inside", "--quiet"]);

    // The mount root handed to us is a symlink to the real directory.
    let link = fixture.path("mount-link");
    std::os::unix::fs::symlink(&real, &link).expect("symlink the mount root");

    let result = info_at(link, "/mnt/repo").await;
    assert_eq!(
        result.code, 0,
        "a mount root reached through a symlink must still resolve: {}",
        result.err
    );
}

// ─────────────────────────────────────────────────────────────────────────
// The escaping *leaf* class: a control file or probe under a checked
// directory is itself a symlink, or names an outside store. The ceiling check
// covers the directory; these cover everything reached by joining a name onto
// it. Found by DeepSeek's adversarial pass after the commondir-content fix.
// ─────────────────────────────────────────────────────────────────────────

/// A unique marker that exists only in a file outside the mount.
const OUTSIDE_MARKER: &str = "OUTSIDE_ONLY_MARKER_do_not_leak";

/// Build a repository inside a mount, hand a closure the pieces to sabotage,
/// and return the mount root plus the outside marker/oid to check for leaks.
///
/// The closure receives `(inside_git_dir, inside_work_dir, outside_dir)`.
/// `outside_dir` is outside the mount and holds `secret.txt` (containing
/// [`OUTSIDE_MARKER`]) and a real repository whose HEAD is `outside_head`.
fn sabotaged(
    sabotage: impl FnOnce(&Path, &Path, &Path),
) -> (Fixture, PathBuf, String) {
    require_git();
    let fixture = Fixture::empty();

    let outside = fixture.path("outside");
    std::fs::create_dir_all(&outside).expect("create outside dir");
    support::write_file(&outside, "secret.txt", &format!("{OUTSIDE_MARKER}\n"));
    git(&outside, &["init", "--initial-branch=main", "--quiet"]);
    support::write_file(&outside, "tracked.txt", "outside content\n");
    git(&outside, &["add", "."]);
    git(&outside, &["commit", "-m", "outside", "--quiet"]);
    let outside_head = git(&outside, &["rev-parse", "HEAD"]);

    let mount = fixture.path("mounted");
    let repo = mount.join("repo");
    std::fs::create_dir_all(&repo).expect("create inside repo");
    git(&repo, &["init", "--initial-branch=main", "--quiet"]);
    support::write_file(&repo, "README.md", "inside\n");
    git(&repo, &["add", "."]);
    git(&repo, &["commit", "-m", "inside", "--quiet"]);

    sabotage(&repo.join(".git"), &repo, &outside);

    (fixture, mount, outside_head)
}

/// Assert a refusal that read nothing outside: exit 4, and neither the outside
/// file's marker nor the outside repository's HEAD oid anywhere in the result.
fn assert_contained(result: &ExecResult, outside_head: &str) {
    assert_eq!(
        result.code, 4,
        "an escaping leaf must be refused (exit 4), got {}: {} {:?}",
        result.code,
        result.err,
        result.output()
    );
    let rendered = format!("{} {:?} {:?}", result.err, result.output(), result.baggage);
    assert!(
        !rendered.contains(OUTSIDE_MARKER),
        "outside file content reached the caller: {rendered}"
    );
    assert!(
        !rendered.contains(outside_head),
        "the outside repository's HEAD oid reached the caller: {rendered}"
    );
}

/// MUST-FIX 1: the `commondir` *file itself* is a symlink to an outside file.
///
/// Distinct from the content escape the previous commit closed — there the
/// commondir file was ordinary and its *content* pointed out. Here the file is
/// a symlink, so `read_to_string` would follow it and read the outside file in
/// full before anything looked at where the content pointed. `open_leaf` lstat
/// s the leaf and refuses the symlink before the read.
#[tokio::test]
async fn a_symlinked_commondir_file_is_refused_before_it_is_read() {
    let (_fixture, mount, outside_head) = sabotaged(|git_dir, _work, outside| {
        std::fs::remove_file(git_dir.join("commondir")).ok();
        std::os::unix::fs::symlink(outside.join("secret.txt"), git_dir.join("commondir"))
            .expect("symlink commondir at an outside file");
    });
    let result = info_at(mount, "/mnt/repo").await;
    assert_contained(&result, &outside_head);
}

/// The same for the repository `config`: a symlink to an outside file would be
/// read in full by `read_repo_config` without `open_leaf` in front of it.
#[tokio::test]
async fn a_symlinked_config_file_is_refused_before_it_is_read() {
    let (_fixture, mount, outside_head) = sabotaged(|git_dir, _work, outside| {
        std::fs::remove_file(git_dir.join("config")).ok();
        std::os::unix::fs::symlink(outside.join("secret.txt"), git_dir.join("config"))
            .expect("symlink config at an outside file");
    });
    let result = info_at(mount, "/mnt/repo").await;
    assert_contained(&result, &outside_head);
}

/// And `.gitmodules`, which `submodule_count` reads out of the working tree.
#[tokio::test]
async fn a_symlinked_gitmodules_is_refused_before_it_is_read() {
    let (_fixture, mount, outside_head) = sabotaged(|_git_dir, work, outside| {
        std::os::unix::fs::symlink(outside.join("secret.txt"), work.join(".gitmodules"))
            .expect("symlink .gitmodules at an outside file");
    });
    let result = info_at(mount, "/mnt/repo").await;
    assert_contained(&result, &outside_head);
}

/// The `shallow` marker is a probe, not a read, but a symlink there is still
/// an out-of-mount stat that answers a yes/no about the host. Refused.
#[tokio::test]
async fn a_symlinked_shallow_marker_is_refused() {
    let (_fixture, mount, outside_head) = sabotaged(|git_dir, _work, outside| {
        std::os::unix::fs::symlink(outside.join("secret.txt"), git_dir.join("shallow"))
            .expect("symlink shallow at an outside file");
    });
    let result = info_at(mount, "/mnt/repo").await;
    assert_contained(&result, &outside_head);
}

/// MUST-FIX 3 (VERIFY 1, confirmed a 4th escape): `objects/info/alternates`
/// names an outside object store, no symlink required.
///
/// `gix-odb` honors alternates and has no option to turn them off, so a
/// repository that ships this file would have gix search an arbitrary host
/// path for objects. `guard_alternates` resolves the chain the way gix does
/// and refuses any entry that leaves the ceiling, before the store is opened.
#[tokio::test]
async fn an_alternates_file_escaping_the_mount_is_refused() {
    let (_fixture, mount, outside_head) = sabotaged(|git_dir, _work, outside| {
        let info = git_dir.join("objects/info");
        std::fs::create_dir_all(&info).expect("create objects/info");
        std::fs::write(
            info.join("alternates"),
            format!("{}\n", outside.join(".git/objects").display()),
        )
        .expect("write an escaping alternates file");
    });
    let result = info_at(mount, "/mnt/repo").await;
    assert_contained(&result, &outside_head);
}

/// MUST-FIX 2: a canonicalize failure on a repo-controlled path must not echo
/// the path, and must be indistinguishable from the escape case.
///
/// `commondir` names a nonexistent path *outside* the mount. `canonicalize`
/// fails (nothing is there), and the naive handling returned a `Repository`
/// error at exit 1 with the attacker-supplied path echoed — a one-bit oracle
/// (does `/outside/x` exist?) plus a path echo. The refusal must instead be
/// the same exit-4, no-echo `EscapesMount` the existing case gives.
#[tokio::test]
async fn a_commondir_naming_a_nonexistent_outside_path_does_not_echo_it() {
    let secret_path = "/nonexistent-marker-9c3f/OUTSIDE_ORACLE_probe";
    let (_fixture, mount, _outside_head) = sabotaged(|git_dir, _work, _outside| {
        std::fs::write(git_dir.join("commondir"), format!("{secret_path}\n"))
            .expect("write a nonexistent-outside commondir");
    });
    let result = info_at(mount, "/mnt/repo").await;
    assert_eq!(
        result.code, 4,
        "a nonexistent outside path must refuse like an existing one, not \
         leak its (non)existence through a different code: {}",
        result.err
    );
    let rendered = format!("{} {:?}", result.err, result.output());
    assert!(
        !rendered.contains("OUTSIDE_ORACLE_probe"),
        "the attacker-supplied path was echoed back — an existence oracle: \
         {rendered}"
    );
}

/// The comparison case for the one above: a commondir naming a path that
/// *does* exist outside. It must give the identical exit code and the same
/// non-echoing refusal, so the two cannot be told apart.
#[tokio::test]
async fn nonexistent_and_existing_outside_commondirs_are_indistinguishable() {
    let (_fa, mount_a, _) = sabotaged(|git_dir, _w, outside| {
        std::fs::write(git_dir.join("commondir"), format!("{}\n", outside.display()))
            .expect("existing outside commondir");
    });
    let existing = info_at(mount_a, "/mnt/repo").await;

    let (_fb, mount_b, _) = sabotaged(|git_dir, _w, _outside| {
        std::fs::write(git_dir.join("commondir"), "/nonexistent-xyz/deadbeef\n")
            .expect("nonexistent outside commondir");
    });
    let missing = info_at(mount_b, "/mnt/repo").await;

    assert_eq!(
        existing.code, missing.code,
        "an existence oracle: exists gave {} but missing gave {}",
        existing.code, missing.code
    );
    assert_eq!(existing.code, 4, "both must be the exit-4 refusal");
}

/// Over-refusal guard for the leaf checks: a legitimate symlink that stays
/// *inside* the mount must be followed, not refused. `.git/objects` symlinked
/// to a store elsewhere under the same mount is the classic shared-store
/// layout, and it must keep working.
#[tokio::test]
async fn a_symlinked_objects_dir_inside_the_mount_still_works() {
    require_git();
    let fixture = Fixture::empty();
    let mount = fixture.path("mounted");
    let repo = mount.join("repo");
    std::fs::create_dir_all(&repo).expect("create repo");
    git(&repo, &["init", "--initial-branch=main", "--quiet"]);
    support::write_file(&repo, "README.md", "inside\n");
    git(&repo, &["add", "."]);
    git(&repo, &["commit", "-m", "inside", "--quiet"]);

    // Move objects to a sibling inside the mount and symlink to it.
    let git_dir = repo.join(".git");
    let store = mount.join("shared-objects");
    std::fs::rename(git_dir.join("objects"), &store).expect("move objects");
    std::os::unix::fs::symlink(&store, git_dir.join("objects")).expect("symlink objects");

    let result = info_at(mount, "/mnt/repo").await;
    assert_eq!(
        result.code, 0,
        "a store symlinked to elsewhere inside the mount is legitimate: {}",
        result.err
    );
}

/// And the alternates over-refusal guard: an alternates file naming a store
/// *inside* the mount is legitimate and must work.
#[tokio::test]
async fn an_alternates_file_inside_the_mount_still_works() {
    require_git();
    let fixture = Fixture::empty();
    let mount = fixture.path("mounted");

    // A donor repository inside the mount, whose objects the main one borrows.
    let donor = mount.join("donor");
    std::fs::create_dir_all(&donor).expect("create donor");
    git(&donor, &["init", "--initial-branch=main", "--quiet"]);
    support::write_file(&donor, "d.txt", "donor\n");
    git(&donor, &["add", "."]);
    git(&donor, &["commit", "-m", "donor", "--quiet"]);

    let repo = mount.join("repo");
    std::fs::create_dir_all(&repo).expect("create repo");
    git(&repo, &["init", "--initial-branch=main", "--quiet"]);
    support::write_file(&repo, "README.md", "inside\n");
    git(&repo, &["add", "."]);
    git(&repo, &["commit", "-m", "inside", "--quiet"]);

    let info = repo.join(".git/objects/info");
    std::fs::create_dir_all(&info).expect("create objects/info");
    std::fs::write(
        info.join("alternates"),
        format!("{}\n", donor.join(".git/objects").display()),
    )
    .expect("write an in-mount alternates file");

    let result = info_at(mount, "/mnt/repo").await;
    assert_eq!(
        result.code, 0,
        "an alternate inside the mount is legitimate: {}",
        result.err
    );
}

/// A relative `objects/info/alternates` entry naming another in-mount object
/// store must be accepted — the case a first pass at `guard_alternates` broke.
///
/// `gix_odb::alternate::resolve` joins a relative entry onto the *objects
/// directory*, not the ceiling; a version of `guard_alternates` that passed
/// the ceiling as the resolver's base, or that ceiling-checked the resolver's
/// raw (lexically-joined, un-canonicalized) output instead of a canonicalized
/// path, could get this wrong in either direction. This fixture is the
/// positive case: `../shared/objects`, which is exactly what a repository
/// sharing an object store with a sibling inside the mount looks like.
#[tokio::test]
async fn a_relative_alternates_entry_inside_the_mount_still_works() {
    require_git();
    let fixture = Fixture::empty();
    let mount = fixture.path("mounted");

    // The donor store, a sibling of `repo` inside the mount.
    let shared = mount.join("shared");
    std::fs::create_dir_all(&shared).expect("create shared donor");
    git(&shared, &["init", "--initial-branch=main", "--quiet"]);
    support::write_file(&shared, "d.txt", "donor\n");
    git(&shared, &["add", "."]);
    git(&shared, &["commit", "-m", "donor", "--quiet"]);

    let repo = mount.join("repo");
    std::fs::create_dir_all(&repo).expect("create repo");
    git(&repo, &["init", "--initial-branch=main", "--quiet"]);
    support::write_file(&repo, "README.md", "inside\n");
    git(&repo, &["add", "."]);
    git(&repo, &["commit", "-m", "inside", "--quiet"]);

    // Relative from `repo/.git/objects`: `../../../shared/.git/objects`.
    let info = repo.join(".git/objects/info");
    std::fs::create_dir_all(&info).expect("create objects/info");
    std::fs::write(info.join("alternates"), "../../../shared/.git/objects\n")
        .expect("write a relative in-mount alternates file");

    let result = info_at(mount, "/mnt/repo").await;
    assert_eq!(
        result.code, 0,
        "a relative alternate that resolves inside the mount is legitimate: {}",
        result.err
    );
}

/// The escaping twin of the test above: a relative `objects/info/alternates`
/// entry with enough `..` to walk past the mount root entirely.
///
/// This is the case that shows the raw-path bug is a real refusal gap, not
/// merely an over-refusal: `gix_odb::alternate::resolve` hands back the
/// lexically-joined path with its `..` components intact, never realpathed.
/// A ceiling check that does `dir.starts_with(ceiling)` on that raw value
/// accepts *any* relative entry unconditionally, because `objects_dir` itself
/// already starts with `ceiling` and nothing appended after that literal
/// prefix — however many `..` it carries — can change what a lexical
/// `starts_with` sees. Only resolving each entry with `canonicalize` before
/// the ceiling check (what `contain` does) catches this.
#[tokio::test]
async fn a_relative_alternates_entry_escaping_the_mount_is_refused() {
    let (_fixture, mount, outside_head) = sabotaged(|git_dir, _work, _outside| {
        let info = git_dir.join("objects/info");
        std::fs::create_dir_all(&info).expect("create objects/info");
        // repo/.git/objects is mount/repo/.git/objects, three segments below
        // the mount root; one more ".." walks past `mount` into the fixture
        // scratch root, landing on `outside/.git/objects`.
        std::fs::write(
            info.join("alternates"),
            "../../../../outside/.git/objects\n",
        )
        .expect("write a relative escaping alternates file");
    });
    let result = info_at(mount, "/mnt/repo").await;
    assert_contained(&result, &outside_head);
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
