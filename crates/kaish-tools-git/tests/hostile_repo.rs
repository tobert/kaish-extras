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

use support::{git, require_git, write_file, Fixture, StrictBackend, TestCtx};

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
    // This is the layout a `git worktree add` PR workflow actually produces
    // when the worktree and its main repository are sibling directories
    // (kaibo's own layout: `~/src/wt/<repo>-<topic>` beside `~/src/<repo>`),
    // so the refusal an embedder hits here is not an edge case — it is the
    // first thing they see on their first call. The message must hand them a
    // way to find the escaping directory themselves rather than have this
    // crate echo it — and must say *where* to run that command, since
    // `git rev-parse` answers relative to cwd and the wrong cwd gives a
    // plausible-looking answer for a different repository. Pinned together
    // so a future edit cannot keep the command while dropping the location.
    let linked_real = std::fs::canonicalize(&linked).expect("canonicalize the refused worktree");
    let expected = format!(
        "run `git rev-parse --git-common-dir --path-format=absolute` inside \
         '{}' to find that repository",
        linked_real.display()
    );
    assert!(
        result.err.contains(&expected),
        "the refusal must name the command AND the worktree to run it in: \
         wanted a substring '{expected}', got: {}",
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

/// G4-alt (docs/issues.md): does `guard_alternates` treat a nonexistent
/// alternate target differently from an existing one outside the mount?
///
/// The doc entry is explicit that this is behavioral and must be probed, not
/// reasoned about: git's own alternates handling silently drops a broken
/// alternate, and `gix_odb::alternate::resolve` might do the same. If it does,
/// an entry naming a *nonexistent* outside path never reaches `contain` at
/// all — `guard_alternates` sees an empty chain and returns `Ok(())`, and the
/// store opens (exit 0) — while the identical entry naming an *existing*
/// outside path is refused (exit 4, confirmed above by
/// `an_alternates_file_escaping_the_mount_is_refused`). That split is the same
/// outside-vs-nonexistent host-existence oracle G2 and
/// `a_gitdir_line_cannot_report_whether_its_target_exists` (below) closed for
/// other repository-controlled paths.
#[tokio::test]
async fn an_alternates_entry_cannot_report_whether_its_outside_target_exists() {
    async fn probe(target_exists: bool) -> (i64, String) {
        require_git();
        let fixture = Fixture::empty();

        let outside = fixture.path("outside");
        std::fs::create_dir_all(&outside).expect("create outside dir");
        // The candidate the alternates entry names: a real directory in one
        // case (standing in for a real object store — guard_alternates never
        // validates that a store is *valid*, only that it resolves inside the
        // ceiling) and nothing at all in the other.
        let target = outside.join("maybe-store/objects");
        if target_exists {
            std::fs::create_dir_all(&target).expect("create outside object dir");
        }

        let mount = fixture.path("mounted");
        let repo = mount.join("repo");
        std::fs::create_dir_all(&repo).expect("create repo");
        git(&repo, &["init", "--initial-branch=main", "--quiet"]);
        support::write_file(&repo, "README.md", "inside\n");
        git(&repo, &["add", "."]);
        git(&repo, &["commit", "-m", "inside", "--quiet"]);

        let info = repo.join(".git/objects/info");
        std::fs::create_dir_all(&info).expect("create objects/info");
        std::fs::write(info.join("alternates"), format!("{}\n", target.display()))
            .expect("write alternates file");

        let result = info_at(mount.clone(), "/mnt/repo").await;
        let err = result
            .err
            .replace(&mount.display().to_string(), "<MOUNT>")
            .replace(&fixture.root().display().to_string(), "<FIXTURE>");
        (result.code, err)
    }

    let (code_present, err_present) = probe(true).await;
    let (code_absent, err_absent) = probe(false).await;

    // docs/issues.md G4: report both, don't reason about them.
    eprintln!(
        "G4-alt probe: existing outside target -> exit {code_present} ({err_present:?}); \
         nonexistent outside target -> exit {code_absent} ({err_absent:?})"
    );

    assert_eq!(
        code_present, code_absent,
        "G4-alt confirmed: guard_alternates is a 1-bit host-existence oracle — \
         an alternates entry naming an existing outside path exits \
         {code_present} while the identical entry naming a nonexistent \
         outside path exits {code_absent}. present: {err_present}; absent: \
         {err_absent}"
    );
    assert_eq!(
        code_present, 4,
        "and both must be the exit-4 refusal, not a missing-store surprise: {err_present}"
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

/// A `gitdir:` line must not report whether its target exists on the host.
///
/// The oracle this crate keeps growing back, found by cross-model review of
/// PR 3 and confirmed by probe. `git_dir` is the one discovery output a
/// repository controls: a `.git` **file** inside the sandbox names it, and can
/// name any absolute path on the host. The two ways that can fail us — the
/// path resolves outside the mount, or it does not resolve at all — used to
/// take different exits (4 vs 1), which is a reliable one-bit read of arbitrary
/// host paths: point `gitdir:` at a path, read the exit code, repeat.
///
/// Both cases are the same refusal now. The exit depends only on whether the
/// attacker's own working tree is inside the mount — something it already
/// knows — and never on the host.
///
/// This is the same rule `contain` and `open_leaf` already hold for
/// `commondir`, alternates and symlinked leaves; only this branch was written
/// before the rule existed.
#[tokio::test]
async fn a_gitdir_line_cannot_report_whether_its_target_exists() {
    require_git();

    /// Build a mount holding a worktree whose `.git` file points outside, and
    /// return `(exit code, stderr)`.
    async fn probe(target_exists: bool) -> (i64, String) {
        let fixture = Fixture::empty();
        let mount = fixture.path("mount");
        let outside = fixture.path("outside");
        std::fs::create_dir_all(&mount).expect("create mount");
        std::fs::create_dir_all(&outside).expect("create outside");

        // The path the `.git` file will name. It is a real repository in one
        // case and absent in the other; nothing else differs.
        let target = outside.join("realrepo.git");
        if target_exists {
            git(&outside, &["init", "--bare", "--quiet", "realrepo.git"]);
        }

        let wt = mount.join("wt");
        std::fs::create_dir_all(&wt).expect("create worktree dir");
        std::fs::write(wt.join(".git"), format!("gitdir: {}\n", target.display()))
            .expect("write .git file");

        let result = info_at(mount.clone(), "/mnt/wt").await;
        // Normalize the fixture's own temp root out of the message: each probe
        // builds its own, and that difference is ours, not the repository's.
        let err = result
            .err
            .replace(&mount.display().to_string(), "<MOUNT>")
            .replace(&fixture.root().display().to_string(), "<FIXTURE>");
        (result.code, err)
    }

    let (code_present, err_present) = probe(true).await;
    let (code_absent, err_absent) = probe(false).await;

    assert_eq!(
        code_present, code_absent,
        "the exit code must not depend on whether the host path exists — it \
         did (present={code_present}, absent={code_absent}), which is a 1-bit \
         read of any path on the host"
    );
    assert_eq!(
        code_present, 4,
        "and both are the environment refusal, not a missing repository: \
         {err_present}"
    );

    // The messages must not differ either: a caller who can tell the two
    // apart by wording has the same oracle in prose.
    assert_eq!(
        err_present, err_absent,
        "the refusals must be indistinguishable in wording too"
    );

    // Neither may echo the outside path back — that would hand over the
    // target directly rather than one bit about it.
    for err in [&err_present, &err_absent] {
        assert!(
            !err.contains("realrepo.git"),
            "the refusal must not echo the path the repository asked us to \
             read: {err}"
        );
    }
}

// ═══════════════════════════════════════════════════════════════════════════
// D.3 layer 3 — a repository that declares textconv, filters and hooks
// ═══════════════════════════════════════════════════════════════════════════

/// The names the hostile fixture's scripts use, one sentinel file each.
///
/// Separate files rather than one, so a failure says *which* mechanism fired
/// instead of only that something did.
const PWN_KINDS: &[&str] = &[
    "textconv",
    "diff-command",
    "clean",
    "smudge",
    "process",
    "pre-commit",
    "post-commit",
    "post-checkout",
    "post-index-change",
    "reference-transaction",
];

/// The bytes the hostile scripts print in place of real content. If this ever
/// reaches a caller, a repository's config decided what a read verb returned.
const PWN_MARKER: &str = "PWNED-BY-REPO-CONFIG";

/// A repository whose `.git/config` and `.gitattributes` try to make git run
/// its code (architecture.md D.3).
///
/// `diff.pwn.textconv` and `diff.pwn.command` for the diff path,
/// `filter.pwn.clean` / `smudge` / `process` for the content path,
/// `core.hooksPath` for the hook path, and `.gitattributes` mapping
/// `* diff=pwn filter=pwn` so all of it is in force for every file. Each
/// script writes its own sentinel and prints [`PWN_MARKER`].
struct PwnRepo {
    fixture: Fixture,
    root: PathBuf,
    sentinels: PathBuf,
}

impl PwnRepo {
    /// The real content, which is what every verb must report.
    const OLD_LINE: &'static str = "the-real-second-line";
    const NEW_LINE: &'static str = "the-real-third-line";

    fn build() -> Self {
        require_git();
        let fixture = Fixture::empty();
        let root = fixture.path("mount/repo");
        let sentinels = fixture.path("sentinels");
        let hooks = fixture.path("hooks");
        let bin = fixture.path("bin");
        std::fs::create_dir_all(&root).expect("create repo dir");
        std::fs::create_dir_all(&sentinels).expect("create sentinel dir");
        std::fs::create_dir_all(&hooks).expect("create hooks dir");
        std::fs::create_dir_all(&bin).expect("create bin dir");

        // One script per mechanism. `cat`-less on purpose: a textconv that
        // printed the file would be indistinguishable from not running.
        for kind in PWN_KINDS {
            let dir = if kind.contains('-') && !kind.starts_with("diff") {
                &hooks
            } else {
                &bin
            };
            let script = dir.join(kind);
            std::fs::write(
                &script,
                format!(
                    "#!/bin/sh\n: > '{}/{kind}'\necho '{PWN_MARKER}'\nexit 0\n",
                    sentinels.display()
                ),
            )
            .expect("write hostile script");
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755))
                    .expect("chmod hostile script");
            }
        }

        git(&root, &["init", "--initial-branch=main", "--quiet"]);
        write_file(&root, "file.txt", &format!("first\n{}\n", Self::OLD_LINE));
        write_file(&root, ".gitattributes", "* diff=pwn filter=pwn\n");
        git(&root, &["add", "file.txt", ".gitattributes"]);
        git(&root, &["commit", "-m", "real content", "--quiet"]);

        // The config goes in *after* the commit, so the stored blob is the
        // real content and not something a clean filter rewrote.
        // `filter.pwn.process` is deliberately **not** set here: git speaks a
        // packet protocol to a process filter, so a script that answers with
        // plain text makes every real-git invocation die with a protocol
        // error — which would leave the control below with nothing to prove.
        // `arm_process` turns it on for the one test that wants exactly that.
        for (key, kind) in [
            ("diff.pwn.textconv", "textconv"),
            ("diff.pwn.command", "diff-command"),
            ("filter.pwn.clean", "clean"),
            ("filter.pwn.smudge", "smudge"),
        ] {
            git(
                &root,
                &[
                    "config",
                    key,
                    bin.join(kind).to_str().expect("utf-8 script path"),
                ],
            );
        }
        git(
            &root,
            &["config", "core.hooksPath", hooks.to_str().expect("utf-8 hooks path")],
        );

        // A real, unstaged content change, so every diff endpoint has
        // something to report and the report can be checked for the real
        // bytes rather than the script's.
        write_file(&root, "file.txt", &format!("first\n{}\n", Self::NEW_LINE));

        Self {
            fixture,
            root,
            sentinels,
        }
    }

    /// The mount root the tool sees. `repo` sits directly under it.
    fn mount(&self) -> PathBuf {
        self.fixture.path("mount")
    }

    /// Every sentinel that exists right now.
    fn fired(&self) -> Vec<String> {
        let mut out: Vec<String> = std::fs::read_dir(&self.sentinels)
            .expect("read sentinel dir")
            .map(|e| e.expect("dir entry").file_name().to_string_lossy().into_owned())
            .collect();
        out.sort();
        out
    }

    /// Add `filter.pwn.process`, the long-running filter protocol.
    fn arm_process(&self) {
        git(
            &self.root,
            &[
                "config",
                "filter.pwn.process",
                self.fixture
                    .path("bin/process")
                    .to_str()
                    .expect("utf-8 script path"),
            ],
        );
    }

    /// Remove every sentinel, so the next observation starts from nothing.
    fn disarm(&self) {
        for name in self.fired() {
            std::fs::remove_file(self.sentinels.join(name)).expect("remove sentinel");
        }
    }
}

/// Run one verb against the hostile repository.
async fn pwn_run(repo: &PwnRepo, verb: &str, argv: &[&str]) -> ExecResult {
    let backend = Arc::new(StrictBackend::single(PathBuf::from("/mnt"), repo.mount()));
    let mut ctx = TestCtx::new(backend, "/mnt/repo");
    let tool = kaish_tools_git::tool(GitConfig::read_only()).expect("config");
    let mut args = ToolArgs::new();
    args.positional.push(Value::String(verb.to_string()));
    const VALUE_FLAGS: &[&str] = &["from", "to", "repo", "limit", "path"];
    let mut i = 0;
    while i < argv.len() {
        let token = argv[i];
        match token.strip_prefix("--") {
            None => {
                args.positional.push(Value::String(token.to_string()));
                i += 1;
            }
            Some(name) if VALUE_FLAGS.contains(&name) => {
                let value = argv
                    .get(i + 1)
                    .unwrap_or_else(|| panic!("'--{name}' takes a value in {argv:?}"));
                args.named
                    .insert(name.to_string(), Value::String((*value).to_string()));
                i += 2;
            }
            Some(name) => {
                args.flags.insert(name.to_string());
                i += 1;
            }
        }
    }
    tool.execute(args, &mut ctx).await
}

/// **The negative control**, and the reason the assertion below means
/// anything: this repository is armed. Hand it to real git and real git runs
/// the scripts.
///
/// A test that only checks a file is absent passes just as well when the
/// script was never executable, the config never took, or the fixture never
/// built. So: run one script directly and watch its sentinel appear, then
/// hand the same repository to real `git diff`, `git status` and
/// `git commit`, and watch textconv, the clean and smudge filters, and the
/// hooks fire. Only then is "our verbs leave the sentinel directory empty" a
/// statement about this build rather than about a broken fixture.
#[tokio::test]
async fn the_hostile_fixture_pwns_real_git() {
    let repo = PwnRepo::build();
    assert!(repo.fired().is_empty(), "nothing has run yet");

    // 1. The script itself works, and writes the file this test watches.
    let script = repo.fixture.path("bin/textconv");
    let out = std::process::Command::new(&script)
        .arg(repo.root.join("file.txt"))
        .output()
        .unwrap_or_else(|e| panic!("run {}: {e}", script.display()));
    assert!(out.status.success(), "the hostile script must run: {out:?}");
    assert!(
        String::from_utf8_lossy(&out.stdout).contains(PWN_MARKER),
        "and print its marker"
    );
    assert_eq!(repo.fired(), vec!["textconv".to_string()]);
    repo.disarm();

    // 2. Real git, given this repository, runs it. `diff.pwn.command` — an
    //    external diff driver — wins over `textconv`, so both spellings get
    //    their own invocation: the plain one reaches the driver, and
    //    `--no-ext-diff` turns the driver off and lets textconv through.
    let theirs = git(&repo.root, &["diff"]);
    assert!(
        theirs.contains(PWN_MARKER),
        "real git's whole answer here is the script's output, not the \
         repository's content: {theirs}"
    );
    git(&repo.root, &["diff", "--no-ext-diff"]);
    git(&repo.root, &["status", "--porcelain"]);
    let after_read = repo.fired();
    for kind in ["diff-command", "textconv", "clean"] {
        assert!(
            after_read.contains(&kind.to_string()),
            "real git must run {kind} against this repository, or the fixture \
             is not hostile: {after_read:?}"
        );
    }

    // 3. And the hooks fire, which is the third mechanism D.3 names. Worth
    //    knowing, and found here: `core.hooksPath` is reachable from a real
    //    git *read* command, not only from a write — `post-index-change`
    //    turns up in the sentinel list above, fired by `git status`
    //    refreshing the index. It is asserted on the write instead, because
    //    `post-index-change` only exists from git 2.22 and only fires when
    //    the index is actually rewritten, while `pre-commit` on a commit is
    //    stable everywhere. Nothing in this crate writes *or* refreshes an
    //    index (D.4 is the test that says so), so neither is reachable here.
    repo.disarm();
    git(&repo.root, &["commit", "--allow-empty", "-m", "hook bait", "--quiet"]);
    let after_write = repo.fired();
    assert!(
        after_write.contains(&"pre-commit".to_string()),
        "real git must run the pre-commit hook from core.hooksPath: \
         {after_write:?}"
    );
    repo.disarm();
}

/// D.3, layer 3, as behavior rather than as a dependency-graph argument:
/// every verb that touches blob content, run against a repository that
/// declares `diff.*.textconv`, `diff.*.command`, `filter.*.clean/smudge/
/// process` and `core.hooksPath`, leaves every sentinel unwritten and
/// reports the repository's real bytes.
///
/// The dependency tripwires (`cargo tree -i` over `gix-command`,
/// `gix-transport`, `gix-filter`, `.github/workflows/ci.yml`) say the code
/// that could act on any of this is not linked. This says what a caller
/// actually gets, which is the claim that survives a pin bump quietly adding
/// an edge — the case the tripwire is weakest against.
#[tokio::test]
async fn no_verb_runs_textconv_a_filter_or_a_hook() {
    let repo = PwnRepo::build();
    repo.disarm();

    let cases: &[(&str, &[&str])] = &[
        ("info", &[]),
        ("status", &["--json"]),
        ("ls", &["--json"]),
        ("log", &["--stat", "--json"]),
        ("show", &["HEAD", "--json"]),
        ("show", &["HEAD:file.txt"]),
        ("diff", &["--json"]),
        ("diff", &["--staged", "--json"]),
        ("diff", &["--from", "HEAD", "--json"]),
        #[cfg(feature = "textdiff")]
        ("diff", &["--patch"]),
    ];

    for (verb, argv) in cases {
        let result = pwn_run(&repo, verb, argv).await;
        assert_eq!(result.code, 0, "git {verb} {argv:?}: {}", result.err);
        assert!(
            repo.fired().is_empty(),
            "git {verb} {argv:?} ran {:?} — a repository's own config decided \
             what a read verb did",
            repo.fired()
        );
        let text = result.text_out().to_string();
        let json = result
            .output()
            .and_then(|o| o.rich_json.clone())
            .map(|v| v.to_string())
            .unwrap_or_default();
        for surface in [&text, &json] {
            assert!(
                !surface.contains(PWN_MARKER),
                "git {verb} {argv:?} returned the script's output: {surface}"
            );
        }
    }
}

/// The other half of the same claim: not only did nothing run, the answer is
/// the *internal* diff of the repository's real bytes.
///
/// An inert textconv could still have produced a wrong answer — an empty
/// diff, say, which is what real git reports here because its clean filter
/// flattens both sides to the same string. Ours reports the change.
#[tokio::test]
async fn the_answer_is_the_internal_diff_of_the_real_bytes() {
    let repo = PwnRepo::build();
    repo.disarm();

    let result = pwn_run(&repo, "diff", &["--json"]).await;
    assert_eq!(result.code, 0, "stderr: {}", result.err);
    let model = result
        .output()
        .and_then(|o| o.rich_json.clone())
        .expect("--json");
    let files = model["files"].as_array().expect("files");
    assert_eq!(files.len(), 1, "only file.txt changed: {model}");
    assert_eq!(files[0]["path"], "file.txt");
    assert_eq!(files[0]["additions"], 1);
    assert_eq!(files[0]["deletions"], 1);

    // `git show HEAD:file.txt` gives the committed bytes, not the script's.
    let blob = pwn_run(&repo, "show", &["HEAD:file.txt"]).await;
    assert_eq!(blob.code, 0, "stderr: {}", blob.err);
    assert!(
        blob.text_out().contains(PwnRepo::OLD_LINE),
        "the blob must be the repository's own content: {}",
        blob.text_out()
    );

    // And real git, on the same repository, answers with the script's output
    // instead of a diff. That contrast is the whole of layer 3: this config
    // is load-bearing for git and inert for us.
    let theirs = git(&repo.root, &["diff"]);
    assert!(
        theirs.contains(PWN_MARKER),
        "real git must be answering from the script here; if it is not, this \
         fixture no longer demonstrates the difference: {theirs}"
    );
    assert!(
        !theirs.contains(PwnRepo::NEW_LINE),
        "and its answer must not contain the real content: {theirs}"
    );
    repo.disarm();
}

/// `filter.*.process` — git's long-running filter protocol — is inert here
/// too, and this one comes with the loudest control of all: real git cannot
/// even *read* this repository, because it opens the protocol and the script
/// answers in prose.
///
/// Our verbs answer normally. Nothing spoke to anything.
#[tokio::test]
async fn a_long_running_process_filter_is_inert_too() {
    let repo = PwnRepo::build();
    repo.arm_process();
    repo.disarm();

    // The control: real git tries, and dies trying.
    let out = std::process::Command::new("git")
        .args(["diff", "--name-only"])
        .current_dir(&repo.root)
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_SYSTEM", "/dev/null")
        .output()
        .expect("run git diff");
    assert!(
        !out.status.success(),
        "real git must try to speak the filter protocol, or this proves nothing"
    );
    assert!(
        String::from_utf8_lossy(&out.stderr).contains("protocol error"),
        "and fail on the protocol, not on something else: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    repo.disarm();

    // And this build reads the repository as if the declaration were a
    // comment, because to this build it is one.
    let result = pwn_run(&repo, "diff", &["--json"]).await;
    assert_eq!(result.code, 0, "stderr: {}", result.err);
    assert!(repo.fired().is_empty(), "ran {:?}", repo.fired());
    let model = result
        .output()
        .and_then(|o| o.rich_json.clone())
        .expect("--json");
    assert_eq!(model["files"][0]["path"], "file.txt");
    assert_eq!(model["files"][0]["additions"], 1);
}

/// A registration's `gitdir` leaf must not be probed with a symlink-following
/// call, because `git info` publishes the count it feeds.
///
/// `worktree_count` screens the registration *directory* with
/// `symlink_metadata` and then asked whether `<dir>/gitdir` `is_file()` —
/// which follows symlinks. A registration whose `gitdir` is a symlink to a
/// host path was counted exactly when that path existed and was a file, so
/// `info`'s `worktrees` number carried one bit about an arbitrary host path,
/// per registration, out of an ordinary call.
///
/// The same probe is written correctly 400 lines away in `verbs/worktree.rs`,
/// whose comment refuses `Path::is_file` in as many words and routes through
/// `contained_leaf` instead. This is the careful thing done everywhere except
/// one spot — the shape that produced both prior containment bugs here.
///
/// Found by cross-model review (kaibo, qwen38 cast), 2026-08-22.
#[tokio::test]
async fn a_registrations_gitdir_symlink_cannot_report_whether_its_target_exists() {
    require_git();

    async fn probe(target_exists: bool) -> (i64, String) {
        let fixture = Fixture::empty();
        let mount = fixture.path("mount");
        let outside = fixture.path("outside");
        std::fs::create_dir_all(&mount).expect("create mount");
        std::fs::create_dir_all(&outside).expect("create outside");

        let repo = mount.join("repo");
        std::fs::create_dir_all(&repo).expect("create repo dir");
        git(&repo, &["init", "--initial-branch=main", "--quiet"]);
        support::write_file(&repo, "README.md", "hi\n");
        git(&repo, &["add", "README.md"]);
        git(&repo, &["commit", "-m", "one", "--quiet"]);

        // The one bit under test: a host path that is a real file in one arm
        // and absent in the other. Nothing else differs between the arms.
        let target = outside.join("secret.txt");
        if target_exists {
            std::fs::write(&target, "present\n").expect("write outside target");
        }

        // A registration directory that is NOT itself a symlink — so the
        // directory screen passes — whose `gitdir` leaf is.
        let reg = repo.join(".git/worktrees/probe");
        std::fs::create_dir_all(&reg).expect("create registration dir");
        std::os::unix::fs::symlink(&target, reg.join("gitdir")).expect("symlink gitdir");

        let result = info_at(mount.clone(), "/mnt/repo").await;
        // Each arm builds its own temp root, and that difference is ours, not
        // the repository's — normalize it out so the comparison is about the
        // one bit under test.
        let rendered = format!("{:?}", result.output())
            .replace(&fixture.root().display().to_string(), "<FIXTURE>");
        (result.code, rendered)
    }

    let (code_present, out_present) = probe(true).await;
    let (code_absent, out_absent) = probe(false).await;

    assert_eq!(
        code_present, code_absent,
        "the exit code must not depend on whether a host path exists"
    );
    assert_eq!(
        out_present, out_absent,
        "`info`'s output changed with the existence of a path outside the \
         mount -- the worktree count is a one-bit host oracle"
    );
}

// ═══════════════════════════════════════════════════════════════════════════
// The start path — the fourth path a repository can name
// ═══════════════════════════════════════════════════════════════════════════

/// The start path is repository-controlled too, and it was an existence
/// oracle for any directory on the host.
///
/// `resolve_real_path` is a backend method. kaish's own `LocalFs` implements
/// it by canonicalizing and refusing an escape, but the trait does not require
/// that, and a backend that joins lexically (this suite's `StrictBackend`, and
/// any embedder that writes the obvious three-line version) hands this crate a
/// path that is inside the mount as a string and a symlink out of it to
/// `openat`. Discovery then walks from the symlink's *target*.
///
/// One bit came back per invocation, and it was the plain existence of any
/// directory on the host: with the target present, discovery found the
/// target's own `.git` — or, absent that, still stopped there — and the call
/// answered about the repository at the mount root (exit 0); with the target
/// missing, `dir.metadata()` failed inside gix-discover and the call was "no
/// repository" (exit 1). Symlink at `/etc/ssl/private`, read the exit code.
///
/// The fix is in `screen_discovery_start`: a start path that resolves outside
/// the mount is refused with the same `NotARepository` a start path that does
/// not resolve at all produces, field for field.
#[tokio::test]
async fn a_symlinked_start_path_cannot_report_whether_its_target_exists() {
    require_git();

    /// Build a mount whose root is a repository, plant `link` in it pointing
    /// at `<scratch>/outside/probed`, and run `git info` from the link.
    ///
    /// The repository at the mount root is what makes the oracle visible:
    /// without it both probes end in "no repository" for their own reasons
    /// and the difference has nowhere to show up.
    async fn probe(target_exists: bool) -> (i64, String) {
        let fixture = Fixture::empty();
        let mount = fixture.path("mount");
        let outside = fixture.path("outside");
        std::fs::create_dir_all(&mount).expect("create mount");
        std::fs::create_dir_all(&outside).expect("create outside");

        git(&mount, &["init", "--initial-branch=main", "--quiet"]);
        write_file(&mount, "README.md", "inside the mount\n");
        git(&mount, &["add", "."]);
        git(&mount, &["commit", "-m", "inside", "--quiet"]);

        let target = outside.join("probed");
        if target_exists {
            std::fs::create_dir_all(&target).expect("create the probe target");
        }
        std::os::unix::fs::symlink(&target, mount.join("link")).expect("plant the symlink");

        let result = info_at(mount.clone(), "/mnt/link").await;
        // Normalize the fixture's own temp root out of the message: each probe
        // builds its own, and that difference is ours, not the repository's.
        let err = result
            .err
            .replace(&mount.display().to_string(), "<MOUNT>")
            .replace(&fixture.root().display().to_string(), "<FIXTURE>");
        (result.code, err)
    }

    let (code_present, err_present) = probe(true).await;
    let (code_absent, err_absent) = probe(false).await;

    assert_eq!(
        code_present, code_absent,
        "the exit code must not depend on whether the symlink's target exists \
         — it did (present={code_present}, absent={code_absent}), which is a \
         1-bit read of any path on the host"
    );
    assert_eq!(
        code_present, 1,
        "and both are 'no repository': from inside the mount there is none at \
         this path either way: {err_present}"
    );
    assert_eq!(
        err_present, err_absent,
        "the refusals must be indistinguishable in wording too"
    );
    for err in [&err_present, &err_absent] {
        assert!(
            !err.contains("probed"),
            "the refusal must not echo where the symlink aimed: {err}"
        );
    }
}

/// The negative control the test above needs: the same fixture, with the
/// symlink pointing at a directory *inside* the mount, answers.
///
/// Without this, "both probes exit 1" would pass just as well if this crate
/// refused every start path, or if the fixture never built a repository at
/// all. Exit 0 here is what says the refusal above is about leaving the mount
/// and nothing else.
#[tokio::test]
async fn a_symlink_to_a_directory_inside_the_mount_is_followed() {
    require_git();
    let fixture = Fixture::empty();
    let mount = fixture.path("mount");
    std::fs::create_dir_all(mount.join("sub")).expect("create mount and sub");

    git(&mount, &["init", "--initial-branch=main", "--quiet"]);
    write_file(&mount, "README.md", "inside the mount\n");
    git(&mount, &["add", "."]);
    git(&mount, &["commit", "-m", "inside", "--quiet"]);
    std::os::unix::fs::symlink(mount.join("sub"), mount.join("link")).expect("symlink");

    let result = info_at(mount.clone(), "/mnt/link").await;
    assert_eq!(
        result.code, 0,
        "a symlink that stays inside the mount must still resolve: {}",
        result.err
    );
    let json = result
        .output()
        .and_then(|o| o.rich_json.clone())
        .expect("structured output");
    assert_eq!(json["repo_root_vfs"], "/mnt");
}

// ═══════════════════════════════════════════════════════════════════════════
// The config gates, and what an include hid from them
// ═══════════════════════════════════════════════════════════════════════════

/// Build a repository inside the mount whose `.git/config` optionally
/// declares `include.path = extra`, with `extra` setting the two keys the
/// format and extension gates exist to catch.
///
/// `included` false is the negative control: the same fixture, same file
/// written, no include line — which must answer, so that the refusal below is
/// about the include and not about the fixture.
fn included_config_fixture(included: bool) -> (Fixture, PathBuf) {
    require_git();
    let fixture = Fixture::empty();
    let mount = fixture.path("mount");
    let repo = mount.join("repo");
    std::fs::create_dir_all(&repo).expect("create the repository directory");
    git(&repo, &["init", "--initial-branch=main", "--quiet"]);
    write_file(&repo, "README.md", "inside the mount\n");
    git(&repo, &["add", "."]);
    git(&repo, &["commit", "-m", "inside", "--quiet"]);

    let git_dir = repo.join(".git");
    std::fs::write(
        git_dir.join("extra"),
        "[core]\n\trepositoryformatversion = 2\n[extensions]\n\tobjectFormat = sha256\n",
    )
    .expect("write the included file");
    if included {
        let mut config = std::fs::read_to_string(git_dir.join("config")).expect("read config");
        config.push_str("[include]\n\tpath = extra\n");
        std::fs::write(git_dir.join("config"), config).expect("write config");
    }
    (fixture, mount)
}

/// An include is refused even when it stays inside the repository.
///
/// The two keys in the included file are exactly the ones
/// `check_format_version` and `check_extensions` refuse a repository over,
/// and both gates read the bare config: the include hid them. Real git hides
/// them from its own gates the same way — measured against git 2.55.0, `git
/// status` exits 0 here and 128 with the same keys written directly — so the
/// old behavior was not a wrong answer yet. It was a config we answered from
/// knowing it was incomplete, which is the silent fallback this crate does
/// not do.
#[tokio::test]
async fn a_repo_relative_include_is_refused_rather_than_answered_around() {
    let (_fixture, mount) = included_config_fixture(true);

    let result = info_at(mount, "/mnt/repo").await;
    assert_eq!(
        result.code, 4,
        "a config this crate cannot fully read must be refused: {:?}",
        result.output()
    );
    assert!(
        result.err.contains("include.path = extra"),
        "the refusal must name the declaration it is about: {}",
        result.err
    );
    assert!(
        result.err.contains("was not read"),
        "the refusal must say the include was not followed: {}",
        result.err
    );
}

/// The negative control: the same repository, the same `extra` file, no
/// include line — and it answers.
///
/// Without this the refusal above would pass just as well if this crate
/// refused every repository, or if `extra` alone were what triggered it.
#[tokio::test]
async fn a_config_with_no_include_still_answers() {
    let (_fixture, mount) = included_config_fixture(false);

    let result = info_at(mount, "/mnt/repo").await;
    assert_eq!(
        result.code, 0,
        "an ordinary repository must still answer: {}",
        result.err
    );
}

// ═══════════════════════════════════════════════════════════════════════════
// The refs gix opens by name
// ═══════════════════════════════════════════════════════════════════════════

/// Forty hex characters and a ref name — the shape a host file only has to
/// *look* like for a ref parser to hand its bytes back.
const HOST_OID: &str = "0123456789abcdef0123456789abcdef01234567";
const HOST_REF_NAME: &str = "leaked-from-the-host";

/// A repository inside the mount with one ref leaf symlinked at a file
/// outside it.
///
/// `leaf` is the path under `.git`, and its content is written to match what
/// that leaf's parser wants: a `packed-refs` line for `packed-refs`, a bare
/// oid for anything read as a single ref.
fn ref_leaf_fixture(leaf: &str) -> (Fixture, PathBuf) {
    require_git();
    let fixture = Fixture::empty();
    let mount = fixture.path("mount");
    let repo = mount.join("repo");
    let outside = fixture.path("outside");
    std::fs::create_dir_all(&repo).expect("create the repository directory");
    std::fs::create_dir_all(&outside).expect("create the outside directory");
    git(&repo, &["init", "--initial-branch=main", "--quiet"]);
    write_file(&repo, "README.md", "inside the mount\n");
    git(&repo, &["add", "."]);
    git(&repo, &["commit", "-m", "inside", "--quiet"]);

    let host_file = outside.join("hostfile");
    let body = if leaf == "packed-refs" {
        format!("{HOST_OID} refs/heads/{HOST_REF_NAME}\n")
    } else {
        format!("{HOST_OID}\n")
    };
    std::fs::write(&host_file, body).expect("write the host file");

    let target = repo.join(".git").join(leaf);
    let _ = std::fs::remove_file(&target);
    std::os::unix::fs::symlink(&host_file, &target).expect("plant the symlinked ref leaf");
    (fixture, mount)
}

/// Run `git show <rev>` with `mount_real` mounted at `/mnt` — the lookup
/// **by name**, which is the path a symlinked ref is reached through.
async fn show_at(mount_real: PathBuf, cwd: &str, rev: &str) -> ExecResult {
    let backend = Arc::new(StrictBackend::single(PathBuf::from("/mnt"), mount_real));
    let mut ctx = TestCtx::new(backend, cwd);
    let tool = kaish_tools_git::tool(GitConfig::read_only()).expect("config");
    let mut args = ToolArgs::new();
    args.positional.push(Value::String("show".to_string()));
    args.positional.push(Value::String(rev.to_string()));
    tool.execute(args, &mut ctx).await
}

/// Run `git branch --json` with `mount_real` mounted at `/mnt`.
async fn branch_at(mount_real: PathBuf, cwd: &str) -> ExecResult {
    let backend = Arc::new(StrictBackend::single(PathBuf::from("/mnt"), mount_real));
    let mut ctx = TestCtx::new(backend, cwd);
    let tool = kaish_tools_git::tool(GitConfig::read_only()).expect("config");
    let mut args = ToolArgs::new();
    args.positional.push(Value::String("branch".to_string()));
    args.flags.insert("json".to_string());
    tool.execute(args, &mut ctx).await
}

/// Assert a refusal that named nothing out of the host file.
fn assert_refused_without_the_host_bytes(result: &ExecResult) {
    assert_eq!(
        result.code, 4,
        "a ref leaf pointing outside the mount must be refused: {} {:?}",
        result.err,
        result.output()
    );
    let rendered = format!("{} {:?}", result.err, result.output());
    assert!(
        !rendered.contains(HOST_OID),
        "the host file's bytes reached the caller: {rendered}"
    );
    assert!(
        !rendered.contains(HOST_REF_NAME),
        "the host file's bytes reached the caller: {rendered}"
    );
}

/// `packed-refs` was a whole-file read, not a one-bit probe.
///
/// Every line of `<40 hex> <name>` in the target became a branch: with the
/// screen removed, `branch --json` returns a row named out of the host file's
/// own bytes, carrying an oid out of them too.
#[tokio::test]
async fn a_symlinked_packed_refs_is_refused_before_it_is_read() {
    let (_fixture, mount) = ref_leaf_fixture("packed-refs");
    let result = branch_at(mount, "/mnt/repo").await;
    assert_refused_without_the_host_bytes(&result);
}

/// `HEAD` gave up its target's first 40 characters through the error message.
///
/// Read as a detached HEAD, the oid goes straight to object lookup and comes
/// back inside "Object <40 characters> as referred to by HEAD could not be
/// found" — 160 bits of any host file, per invocation, from `info` alone.
#[tokio::test]
async fn a_symlinked_head_is_refused_before_it_is_read() {
    let (_fixture, mount) = ref_leaf_fixture("HEAD");
    let result = info_at(mount, "/mnt/repo").await;
    assert_refused_without_the_host_bytes(&result);
}

/// A whole ref hierarchy is a directory, and the same leaf screen covers it.
///
/// gix's iteration does not follow the symlink — the listing comes back empty
/// rather than full of the outside directory — but a lookup by name does, and
/// every verb that resolves a revision does lookups by name.
#[tokio::test]
async fn a_symlinked_refs_hierarchy_is_refused() {
    require_git();
    let fixture = Fixture::empty();
    let mount = fixture.path("mount");
    let repo = mount.join("repo");
    let outside = fixture.path("outside/heads");
    std::fs::create_dir_all(&repo).expect("create the repository directory");
    std::fs::create_dir_all(&outside).expect("create the outside hierarchy");
    git(&repo, &["init", "--initial-branch=main", "--quiet"]);
    write_file(&repo, "README.md", "inside the mount\n");
    git(&repo, &["add", "."]);
    git(&repo, &["commit", "-m", "inside", "--quiet"]);
    std::fs::write(outside.join(HOST_REF_NAME), format!("{HOST_OID}\n")).expect("write outside ref");

    let heads = repo.join(".git/refs/heads");
    std::fs::remove_dir_all(&heads).expect("remove the real hierarchy");
    std::os::unix::fs::symlink(&outside, &heads).expect("plant the symlinked hierarchy");

    let listed = branch_at(mount.clone(), "/mnt/repo").await;
    assert_refused_without_the_host_bytes(&listed);

    // The lookup by name is the leak the listing does not show: without the
    // screen this resolves the outside ref and returns its oid in the error.
    let named = show_at(mount, "/mnt/repo", HOST_REF_NAME).await;
    assert_refused_without_the_host_bytes(&named);
}

/// The negative control for the three refusals above: the same repository
/// with every ref leaf real still answers, and lists its own branch.
///
/// Without it, "exit 4 and no host bytes" would pass just as well against a
/// crate that refused every repository.
#[tokio::test]
async fn a_repository_whose_ref_leaves_are_real_still_answers() {
    require_git();
    let fixture = Fixture::empty();
    let mount = fixture.path("mount");
    let repo = mount.join("repo");
    std::fs::create_dir_all(&repo).expect("create the repository directory");
    git(&repo, &["init", "--initial-branch=main", "--quiet"]);
    write_file(&repo, "README.md", "inside the mount\n");
    git(&repo, &["add", "."]);
    git(&repo, &["commit", "-m", "inside", "--quiet"]);
    // Pack the refs, so `packed-refs` is a real file this run actually reads
    // rather than a leaf that happens to be absent.
    git(&repo, &["pack-refs", "--all"]);

    let result = branch_at(mount, "/mnt/repo").await;
    assert_eq!(result.code, 0, "an ordinary repository must answer: {}", result.err);
    let json = result
        .output()
        .and_then(|o| o.rich_json.clone())
        .expect("--json carries the typed model");
    let names: Vec<String> = json["branches"]
        .as_array()
        .expect("branches array")
        .iter()
        .map(|b| b["name"].as_str().expect("name").to_string())
        .collect();
    assert_eq!(names, vec!["main".to_string()]);
}

/// The residual, pinned: a symlinked **loose ref** still reaches a host file,
/// and `HEAD` naming it needs no help from the caller.
///
/// This asserts the leak, deliberately. `HEAD`, `packed-refs` and the three
/// `refs/` hierarchies are fixed names and are screened; a path the
/// *repository* names under `refs/` is not, because gix opens it by name and
/// intercepting that means wrapping every gix open. Closing it eagerly would
/// cost every verb an lstat per loose ref on a tree the repository sizes.
///
/// So the honest state is: this is open, `docs/embedding-git.md` says so, and
/// `docs/issues.md`'s P13 carries the close. When that lands, this test goes
/// red — which is the point. Update it and both documents together; do not
/// weaken it in place.
#[tokio::test]
async fn a_symlinked_loose_ref_still_reaches_a_host_file() {
    let (_fixture, mount) = ref_leaf_fixture("refs/heads/pwn");
    std::fs::write(
        mount.join("repo/.git/HEAD"),
        "ref: refs/heads/pwn\n",
    )
    .expect("point HEAD at the symlinked ref");

    let result = info_at(mount, "/mnt/repo").await;
    assert_eq!(
        result.code, 1,
        "the documented residual is a failed object lookup, not a refusal: {}",
        result.err
    );
    assert!(
        result.err.contains(HOST_OID),
        "the residual documented in embedding-git.md is that the host file's \
         first 40 characters come back — if they no longer do, the leak is \
         closed and the docs and this test must say so: {}",
        result.err
    );
}
