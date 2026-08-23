//! P10 characterization tests (docs/issues.md): semantic inputs this crate
//! had never been run against before this file — a real shallow clone,
//! replace refs and grafts, an octopus (3+ parent) merge, an unborn HEAD, tag
//! chains at and past the two depth bounds (`show`'s 8, `peel_tag_chain`'s
//! 32), non-UTF-8 names in a tree and in the untracked walk, `--path ""`, and
//! `core.autocrlf true`.
//!
//! Real git is the oracle throughout, following `support.rs`'s idiom: every
//! assertion here holds our output against what a real `git` invocation on
//! the same fixture actually printed, never against a hand-written belief
//! about what git does. Some of these are fidelity claims (we agree with
//! git); some are divergences, pinned with git's own answer beside ours the
//! way `docs/issues.md`'s D1/D2/L8/L9/B2/B3/B7 entries are; one is left
//! failing on purpose because the underlying behavior is wrong, not merely
//! different, and is reported rather than fixed here.

#[path = "support.rs"]
mod support;

use std::path::{Path, PathBuf};
use std::sync::Arc;

use kaish_tool_api::Tool;
use kaish_types::{ExecResult, ToolArgs, Value};

use kaish_tools_git::GitConfig;

use support::{git, git_as, require_git, write_file, Fixture, StrictBackend, TestCtx};

/// The VFS root every fixture's scratch directory is mounted at.
const MOUNT: &str = "/mnt";

// ═══════════════════════════════════════════════════════════════════════════
// A shared argv builder, parameterized per verb like `tests/ls.rs`'s
// VALUE_FLAGS/BOOL_FLAGS split — explicit rather than log.rs's "does the next
// token start with `--`" heuristic, because several tests here pass a bare
// positional (a revision) right after a bool flag, which that heuristic would
// misread as the flag's value.
// ═══════════════════════════════════════════════════════════════════════════

fn build_args(verb: &str, value_flags: &[&str], bool_flags: &[&str], argv: &[&str]) -> ToolArgs {
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
        if value_flags.contains(&name) {
            let value = argv
                .get(i + 1)
                .unwrap_or_else(|| panic!("'--{name}' takes a value, none followed in {argv:?}"));
            args.named.insert(name.to_string(), Value::String((*value).to_string()));
            i += 2;
        } else if bool_flags.contains(&name) {
            args.flags.insert(name.to_string());
            i += 1;
        } else {
            panic!("'--{name}' is not classified as a value or bool flag for '{verb}' in this harness");
        }
    }
    args
}

async fn run_verb(
    verb: &str,
    value_flags: &[&str],
    bool_flags: &[&str],
    config: GitConfig,
    mount_real: &Path,
    cwd: &str,
    argv: &[&str],
) -> ExecResult {
    let backend = Arc::new(StrictBackend::single(PathBuf::from(MOUNT), mount_real.to_path_buf()));
    let mut ctx = TestCtx::new(backend, cwd);
    let tool = kaish_tools_git::tool(config).expect("config");
    tool.execute(build_args(verb, value_flags, bool_flags, argv), &mut ctx).await
}

const LOG_VALUE: &[&str] = &["limit", "repo", "rev", "since", "until", "author", "path"];
const LOG_BOOL: &[&str] = &["body", "stat", "merges", "no-merges", "first-parent", "json"];

async fn log(mount_real: &Path, cwd: &str, argv: &[&str]) -> ExecResult {
    run_verb("log", LOG_VALUE, LOG_BOOL, GitConfig::read_only(), mount_real, cwd, argv).await
}

const STATUS_VALUE: &[&str] = &["limit", "repo", "path", "untracked"];
const STATUS_BOOL: &[&str] = &["ignored", "json"];

async fn status_run(mount_real: &Path, cwd: &str, argv: &[&str]) -> ExecResult {
    run_verb("status", STATUS_VALUE, STATUS_BOOL, GitConfig::read_only(), mount_real, cwd, argv).await
}

const BRANCH_VALUE: &[&str] = &["limit", "repo", "contains", "merged"];
const BRANCH_BOOL: &[&str] = &["all", "remote", "ahead-behind", "json"];

async fn branch_run(mount_real: &Path, cwd: &str, argv: &[&str]) -> ExecResult {
    run_verb("branch", BRANCH_VALUE, BRANCH_BOOL, GitConfig::read_only(), mount_real, cwd, argv).await
}

const TAG_VALUE: &[&str] = &["limit", "repo", "contains"];
const TAG_BOOL: &[&str] = &["json"];

async fn tag_run(mount_real: &Path, cwd: &str, argv: &[&str]) -> ExecResult {
    run_verb("tag", TAG_VALUE, TAG_BOOL, GitConfig::read_only(), mount_real, cwd, argv).await
}

const SHOW_VALUE: &[&str] = &["limit", "repo"];
const SHOW_BOOL: &[&str] = &["json"];

async fn show_run(mount_real: &Path, cwd: &str, argv: &[&str]) -> ExecResult {
    run_verb("show", SHOW_VALUE, SHOW_BOOL, GitConfig::read_only(), mount_real, cwd, argv).await
}

const LS_VALUE: &[&str] = &["limit", "repo"];
const LS_BOOL: &[&str] = &["json", "recursive"];

async fn ls_run(mount_real: &Path, cwd: &str, argv: &[&str]) -> ExecResult {
    run_verb("ls", LS_VALUE, LS_BOOL, GitConfig::read_only(), mount_real, cwd, argv).await
}

/// The typed model out of a successful `--json`-carrying result.
fn json(result: &ExecResult) -> serde_json::Value {
    assert_eq!(result.code, 0, "verb failed: {}", result.err);
    result
        .output()
        .and_then(|o| o.rich_json.clone())
        .expect("--json carries the typed model")
}

/// `log`'s full oids, in order.
fn log_oids(result: &ExecResult) -> Vec<String> {
    json(result)["commits"]
        .as_array()
        .expect("commits is an array")
        .iter()
        .map(|c| c["oid"].as_str().expect("oid is a string").to_string())
        .collect()
}

/// `git log --format=%H` on the same args — the oracle for [`log_oids`].
fn git_log_oids(root: &Path, args: &[&str]) -> Vec<String> {
    let mut argv = vec!["log", "--format=%H"];
    argv.extend_from_slice(args);
    let out = git(root, &argv);
    out.lines().map(|l| l.trim().to_string()).collect()
}

/// Run git without asserting success — for an oracle call that is *meant* to
/// fail (a shallow-refusing filter, an octopus conflict, an empty pathspec).
/// Same hermetic environment as `support::git`.
fn git_allow_fail(cwd: &Path, args: &[&str]) -> std::process::Output {
    std::process::Command::new("git")
        .args(args)
        .current_dir(cwd)
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_SYSTEM", "/dev/null")
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_TERMINAL_PROMPT", "0")
        .env("GIT_AUTHOR_NAME", "Fixture Author")
        .env("GIT_AUTHOR_EMAIL", "author@example.invalid")
        .env("GIT_COMMITTER_NAME", "Fixture Committer")
        .env("GIT_COMMITTER_EMAIL", "committer@example.invalid")
        .env("GIT_AUTHOR_DATE", "2026-08-01T10:00:00+00:00")
        .env("GIT_COMMITTER_DATE", "2026-08-01T10:00:00+00:00")
        .output()
        .unwrap_or_else(|e| panic!("git {args:?} in {}: {e}", cwd.display()))
}

/// A repository with a single commit, for fixtures that just need a base to
/// build on top of.
fn one_commit_repo(name: &str) -> (Fixture, PathBuf, String) {
    require_git();
    let fixture = Fixture::empty();
    let root = fixture.path(name);
    std::fs::create_dir_all(&root).expect("create repo dir");
    git(&root, &["init", "--initial-branch=main", "--quiet"]);
    git(&root, &["config", "gc.writeCommitGraph", "false"]);
    write_file(&root, "f.txt", "content\n");
    git(&root, &["add", "."]);
    git(&root, &["commit", "-m", "base", "--quiet"]);
    let oid = git(&root, &["rev-parse", "HEAD"]);
    (fixture, root, oid)
}

// ═══════════════════════════════════════════════════════════════════════════
// 1. Shallow clones — the `refuse_shallow` gate, exercised for real
// ═══════════════════════════════════════════════════════════════════════════

/// A real repository (5 linear commits) plus a real `git clone --depth 2` of
/// it — an actual shallow clone with a real `.git/shallow`, not a belief
/// about what one looks like.
struct ShallowPair {
    fixture: Fixture,
    /// The full repository's root (not mounted; only used to seed the clone).
    #[allow(dead_code)]
    full_root: PathBuf,
    /// The shallow clone's root — this is what every test here mounts.
    shallow_root: PathBuf,
}

impl ShallowPair {
    fn build() -> Self {
        require_git();
        let fixture = Fixture::empty();
        let full_root = fixture.path("full");
        std::fs::create_dir_all(&full_root).expect("create repo dir");
        git(&full_root, &["init", "--initial-branch=main", "--quiet"]);
        git(&full_root, &["config", "gc.writeCommitGraph", "false"]);
        for i in 1..=5 {
            write_file(&full_root, "f.txt", &format!("line{i}\n"));
            git(&full_root, &["add", "."]);
            git(&full_root, &["commit", "-m", &format!("c{i}"), "--quiet"]);
        }

        let shallow_root = fixture.path("shallow");
        git(
            &fixture.root(),
            &[
                "clone",
                "-q",
                "--depth",
                "2",
                // `file://` is load-bearing: git ignores `--depth` for a clone
                // from a local PATH (it hardlinks the object store instead)
                // and only warns. The control below caught exactly that.
                &format!("file://{}", full_root.to_str().expect("utf-8 path")),
                shallow_root.to_str().expect("utf-8 path"),
            ],
        );

        // Negative control: the clone really is shallow, or every assertion
        // below about a "shallow clone" is about an ordinary one.
        let is_shallow = git(&shallow_root, &["rev-parse", "--is-shallow-repository"]);
        assert_eq!(is_shallow, "true", "the fixture must be a real shallow clone");

        Self { fixture, full_root, shallow_root }
    }

    fn scratch(&self) -> PathBuf {
        self.fixture.root()
    }
}

/// `branch --contains` refuses on a shallow clone, naming the flag and the
/// word "shallow" — the gate `refuse_shallow` exists for, exercised against a
/// real `git clone --depth`.
#[tokio::test]
async fn branch_contains_refuses_on_a_shallow_clone() {
    let pair = ShallowPair::build();
    let result = branch_run(&pair.scratch(), "/mnt/shallow", &["--contains", "HEAD"]).await;
    assert_eq!(result.code, 2, "stderr was: {}", result.err);
    assert!(result.err.contains("shallow"), "names the condition: {}", result.err);
    assert!(result.err.contains("--contains"), "names the flag: {}", result.err);

    // Negative control: the same clone answers a plain listing fine — the
    // refusal is about the flag, not about the repository being unusable.
    let plain = branch_run(&pair.scratch(), "/mnt/shallow", &[]).await;
    assert_eq!(plain.code, 0, "a plain listing must still work: {}", plain.err);
}

/// `branch --merged` refuses the same way.
#[tokio::test]
async fn branch_merged_refuses_on_a_shallow_clone() {
    let pair = ShallowPair::build();
    let result = branch_run(&pair.scratch(), "/mnt/shallow", &["--merged", "HEAD"]).await;
    assert_eq!(result.code, 2, "stderr was: {}", result.err);
    assert!(result.err.contains("shallow"), "names the condition: {}", result.err);
    assert!(result.err.contains("--merged"), "names the flag: {}", result.err);
}

/// `branch --ahead-behind` refuses too, even with no upstream configured at
/// all — the gate runs unconditionally, before any per-row upstream walk.
#[tokio::test]
async fn branch_ahead_behind_refuses_on_a_shallow_clone() {
    let pair = ShallowPair::build();
    let result = branch_run(&pair.scratch(), "/mnt/shallow", &["--ahead-behind"]).await;
    assert_eq!(result.code, 2, "stderr was: {}", result.err);
    assert!(result.err.contains("shallow"), "names the condition: {}", result.err);
    assert!(result.err.contains("--ahead-behind"), "names the flag: {}", result.err);
}

/// `tag --contains` refuses the same way as `branch --contains`.
#[tokio::test]
async fn tag_contains_refuses_on_a_shallow_clone() {
    let pair = ShallowPair::build();
    let result = tag_run(&pair.scratch(), "/mnt/shallow", &["--contains", "HEAD"]).await;
    assert_eq!(result.code, 2, "stderr was: {}", result.err);
    assert!(result.err.contains("shallow"), "names the condition: {}", result.err);
    assert!(result.err.contains("--contains"), "names the flag: {}", result.err);

    // Negative control: a plain tag listing on the same clone still works.
    let plain = tag_run(&pair.scratch(), "/mnt/shallow", &[]).await;
    assert_eq!(plain.code, 0, "a plain tag listing must still work: {}", plain.err);
}

/// `show HEAD` on a shallow clone is unaffected — it reads one commit, not
/// ancestry, so `refuse_shallow` has nothing to do with it and nothing here
/// should refuse.
#[tokio::test]
async fn show_is_unaffected_by_a_shallow_clone() {
    let pair = ShallowPair::build();
    let result = show_run(&pair.scratch(), "/mnt/shallow", &["HEAD"]).await;
    assert_eq!(result.code, 0, "show must not be gated by shallowness: {}", result.err);
}

/// **Left failing on purpose (2026-08-23).** `log`'s default walk is not
/// gated by `refuse_shallow` at all — only `branch`/`tag`'s ancestry flags
/// are — and nothing in `verbs::log` consults `.git/shallow` either. The walk
/// reads a shallow boundary commit's *real* parent oid straight off the raw
/// commit object (which still names it — the shallow marker is what tells
/// git's own walker to stop, not a rewritten commit), then tries to
/// `find_commit` that parent to enqueue it. On this fixture (5 commits,
/// cloned at `--depth 2`) the object genuinely is not there, and `enqueue`
/// (`verbs/log.rs`) propagates a raw "object not found" through `?` — a
/// crash on the exact caller who does nothing unusual: a default `git log`
/// against the kind of shallow checkout `actions/checkout` or `git clone
/// --depth 1` produce every day. Real git handles this natively — a shallow
/// clone's log stops cleanly at the boundary and reports what it has.
///
/// `refuse_shallow`'s own doc comment names this exact failure mode
/// ("the failure would otherwise wear the shape of a missing object rather
/// than of a shallow repository") as the reason `branch`/`tag`'s ancestry
/// flags are gated — but `log` was never gated, so the failure mode the gate
/// exists to avoid is reachable anyway, through the one verb with no gate at
/// all. Filed as docs/issues.md L11. Do not "fix" this test by lowering the
/// expectation to the crash it currently produces — the assertion below is
/// what correct behavior looks like, and it should go green the day this is
/// fixed, not before.
#[tokio::test]
async fn log_default_walk_on_a_shallow_clone_matches_git() {
    let pair = ShallowPair::build();
    let oracle = git_log_oids(&pair.shallow_root, &[]);
    assert_eq!(oracle.len(), 2, "the depth-2 shallow clone must hold exactly 2 commits: {oracle:?}");

    let result = log(&pair.scratch(), "/mnt/shallow", &[]).await;
    assert_eq!(
        result.code, 0,
        "a default `git log` on a shallow clone must not crash reaching the \
         boundary the way `refuse_shallow`'s own doc comment warns about — \
         stderr was: {}",
        result.err
    );
    assert_eq!(log_oids(&result), oracle);
}

// ═══════════════════════════════════════════════════════════════════════════
// 2. Replace refs and grafts — honored by git, invisible to this crate
// ═══════════════════════════════════════════════════════════════════════════

/// git's default `log` (and every other read) transparently substitutes a
/// `refs/replace/<oid>` target for the named object; `ReadRepo` opens the
/// object store with raw `gix_odb::at`, which knows nothing about
/// `refs/replace/*` (that layer lives in `gix::Repository`, not in the
/// low-level crates this build is built on — architecture.md's "Path 2"
/// plumbing choice). So the two disagree about what commit `B` *is*, while
/// agreeing perfectly about its oid and its place in history.
///
/// Pinned with git's own answer beside ours: docs/issues.md L10.
#[tokio::test]
async fn log_ignores_a_replace_ref_where_git_honors_it() {
    require_git();
    let fixture = Fixture::empty();
    let root = fixture.path("repo");
    std::fs::create_dir_all(&root).expect("create repo dir");
    git(&root, &["init", "--initial-branch=main", "--quiet"]);
    git(&root, &["config", "gc.writeCommitGraph", "false"]);

    write_file(&root, "a.txt", "one\n");
    git(&root, &["add", "."]);
    git(&root, &["commit", "-m", "A", "--quiet"]);
    let a = git(&root, &["rev-parse", "HEAD"]);

    write_file(&root, "a.txt", "one\ntwo\n");
    git(&root, &["add", "."]);
    git(&root, &["commit", "-m", "B original", "--quiet"]);
    let b = git(&root, &["rev-parse", "HEAD"]);

    write_file(&root, "a.txt", "one\ntwo\nthree\n");
    git(&root, &["add", "."]);
    git(&root, &["commit", "-m", "C", "--quiet"]);

    // A replacement for B: same tree, same parent, a different message. Git's
    // replace mechanism keeps the *name* (`b`'s oid never changes — every ref
    // and every other commit's `parent` line still says `b`) and substitutes
    // the *content* transparently for anything that reads it by that name.
    let tree_b = git(&root, &["rev-parse", &format!("{b}^{{tree}}")]);
    let replacement = git(&root, &["commit-tree", &tree_b, "-p", &a, "-m", "B REPLACED"]);
    git(&root, &["replace", &b, &replacement]);

    // Oracle: git's default log shows the replaced message, at the SAME oid.
    let oracle_summary = git(&root, &["log", "-1", "--format=%s", &b]);
    assert_eq!(oracle_summary, "B REPLACED", "git must honor the replace ref by default");

    // Ours: the raw object store never consults refs/replace/*, so the walk
    // reports B's original content.
    let result = log(&fixture.root(), "/mnt/repo", &[]).await;
    assert_eq!(result.code, 0, "stderr: {}", result.err);
    let commits = json(&result)["commits"].clone();
    let commits = commits.as_array().expect("commits array");
    let ours = commits
        .iter()
        .find(|c| c["oid"] == b)
        .unwrap_or_else(|| panic!("B ({b}) must still be walked: {commits:?}"));
    assert_eq!(
        ours["summary"], "B original",
        "we read the object store directly and never consult refs/replace/*"
    );

    // Both sides agree the oid is still `b` — the divergence is about content
    // read through that name, not about identity.
    assert_eq!(ours["oid"], b);
}

/// `.git/info/grafts` — legacy, deprecated by git itself in favor of replace
/// refs, but still honored by git 2.55 (a stderr deprecation hint, not a
/// refusal). It rewrites a commit's *parents* for every reader that consults
/// it; this crate's `enqueue`/`commit.parents()` reads the raw commit object,
/// which the graft never touches, so the walk follows the real ancestry the
/// object records rather than the grafted one.
///
/// Pinned with git's own answer beside ours: docs/issues.md L10.
#[tokio::test]
async fn log_ignores_grafts_where_git_honors_them() {
    require_git();
    let fixture = Fixture::empty();
    let root = fixture.path("repo");
    std::fs::create_dir_all(&root).expect("create repo dir");
    git(&root, &["init", "--initial-branch=main", "--quiet"]);
    git(&root, &["config", "gc.writeCommitGraph", "false"]);

    write_file(&root, "a.txt", "one\n");
    git(&root, &["add", "."]);
    git(&root, &["commit", "-m", "root", "--quiet"]);
    let r = git(&root, &["rev-parse", "HEAD"]);

    write_file(&root, "a.txt", "one\ntwo\n");
    git(&root, &["add", "."]);
    git(&root, &["commit", "-m", "mid", "--quiet"]);
    let m = git(&root, &["rev-parse", "HEAD"]);

    write_file(&root, "a.txt", "one\ntwo\nthree\n");
    git(&root, &["add", "."]);
    git(&root, &["commit", "-m", "tip", "--quiet"]);
    let t = git(&root, &["rev-parse", "HEAD"]);

    // Graft `tip`'s parent straight to `root`, skipping `mid` — the graft
    // file format is `"<commit> <parent>..."`.
    std::fs::write(root.join(".git/info/grafts"), format!("{t} {r}\n")).expect("write grafts");

    // Oracle: git's log honors the graft and skips `mid` entirely.
    let oracle = git_log_oids(&root, &[]);
    assert_eq!(oracle, vec![t.clone(), r.clone()], "git must honor the graft");
    assert!(!oracle.contains(&m), "the graft hides `mid` from git's own walk");

    // Ours: the raw commit object's real parent (`mid`) is what `enqueue`
    // reads, so the walk reports all three commits.
    let result = log(&fixture.root(), "/mnt/repo", &[]).await;
    assert_eq!(result.code, 0, "stderr: {}", result.err);
    let ours = log_oids(&result);
    assert_eq!(ours, vec![t, m.clone(), r], "we never read .git/info/grafts");
    assert!(ours.contains(&m), "we report `mid`, which the graft hid from git");
}

// ═══════════════════════════════════════════════════════════════════════════
// 3. Octopus merges — every fixture merge so far has had two parents
// ═══════════════════════════════════════════════════════════════════════════

/// A root commit plus three branches (`b1`, `b2`, `b3`), each touching a
/// distinct file, merged into `main` in one `git merge b1 b2 b3` — a real
/// octopus merge with 4 parents (the pre-merge `main` tip plus the three
/// branches), not a synthetic 3-parent commit built by hand.
struct OctopusRepo {
    fixture: Fixture,
    root: PathBuf,
    root_oid: String,
    x1: String,
    x2: String,
    x3: String,
}

impl OctopusRepo {
    fn build() -> Self {
        require_git();
        let fixture = Fixture::empty();
        let root = fixture.path("repo");
        std::fs::create_dir_all(&root).expect("create repo dir");
        let amy = ("Amy Tobey", "amy@example.invalid");
        git(&root, &["init", "--initial-branch=main", "--quiet"]);
        git(&root, &["config", "gc.writeCommitGraph", "false"]);

        // Strictly increasing committer instants throughout: `git()`'s fixed
        // single timestamp would tie every branch commit's committer time,
        // which is L5's own divergence (a tied instant orders differently
        // from git's own discovery order) — irrelevant to what THIS fixture
        // is for, so it is avoided rather than accidentally re-triggered.
        write_file(&root, "base.txt", "base\n");
        git_as(&root, amy, "2026-01-01T00:00:00+00:00", &["add", "."]);
        git_as(&root, amy, "2026-01-01T00:00:00+00:00", &["commit", "-m", "root", "--quiet"]);
        let root_oid = git(&root, &["rev-parse", "HEAD"]);

        git(&root, &["branch", "b1"]);
        git(&root, &["branch", "b2"]);
        git(&root, &["branch", "b3"]);

        git(&root, &["checkout", "--quiet", "b1"]);
        write_file(&root, "one.txt", "one\n");
        git_as(&root, amy, "2026-01-02T00:00:00+00:00", &["add", "."]);
        git_as(&root, amy, "2026-01-02T00:00:00+00:00", &["commit", "-m", "b1 work", "--quiet"]);
        let x1 = git(&root, &["rev-parse", "HEAD"]);

        git(&root, &["checkout", "--quiet", "b2"]);
        write_file(&root, "two.txt", "two\n");
        git_as(&root, amy, "2026-01-03T00:00:00+00:00", &["add", "."]);
        git_as(&root, amy, "2026-01-03T00:00:00+00:00", &["commit", "-m", "b2 work", "--quiet"]);
        let x2 = git(&root, &["rev-parse", "HEAD"]);

        git(&root, &["checkout", "--quiet", "b3"]);
        write_file(&root, "three.txt", "three\n");
        git_as(&root, amy, "2026-01-04T00:00:00+00:00", &["add", "."]);
        git_as(&root, amy, "2026-01-04T00:00:00+00:00", &["commit", "-m", "b3 work", "--quiet"]);
        let x3 = git(&root, &["rev-parse", "HEAD"]);

        git(&root, &["checkout", "--quiet", "main"]);
        git_as(
            &root,
            amy,
            "2026-01-05T00:00:00+00:00",
            &["merge", "--no-ff", "--quiet", "-m", "octopus merge", "b1", "b2", "b3"],
        );

        // Negative control: this really is an octopus (3+ parents), or the
        // tests below prove nothing about the 3+-parent code paths they
        // claim to.
        let parents = git(&root, &["show", "-s", "--format=%P", "HEAD"]);
        assert_eq!(
            parents.split_whitespace().count(),
            4,
            "fixture must be a real octopus merge (root + 3 branches): {parents}"
        );

        Self { fixture, root, root_oid, x1, x2, x3 }
    }

    fn scratch(&self) -> PathBuf {
        self.fixture.root()
    }

    fn merge_oid(&self) -> String {
        git(&self.root, &["rev-parse", "HEAD"])
    }
}

/// `--merges`/`--no-merges` partition history the same way they do for a
/// two-parent merge, and agree with git on an octopus one too.
#[tokio::test]
async fn octopus_merge_filters_match_git() {
    let repo = OctopusRepo::build();

    let merges = log(&repo.scratch(), "/mnt/repo", &["--merges"]).await;
    assert_eq!(log_oids(&merges), git_log_oids(&repo.root, &["--merges"]));
    assert_eq!(log_oids(&merges), vec![repo.merge_oid()], "exactly the octopus merge");

    let no_merges = log(&repo.scratch(), "/mnt/repo", &["--no-merges"]).await;
    assert_eq!(log_oids(&no_merges), git_log_oids(&repo.root, &["--no-merges"]));

    let all = log(&repo.scratch(), "/mnt/repo", &[]).await;
    assert_eq!(
        log_oids(&merges).len() + log_oids(&no_merges).len(),
        log_oids(&all).len(),
        "the two filters still partition history with 4 parents in play"
    );
}

/// `--first-parent` follows only the mainline parent (the pre-merge `main`
/// tip), which on an octopus merge means all three side branches vanish, not
/// just one.
#[tokio::test]
async fn octopus_first_parent_matches_git() {
    let repo = OctopusRepo::build();
    let result = log(&repo.scratch(), "/mnt/repo", &["--first-parent"]).await;
    assert_eq!(log_oids(&result), git_log_oids(&repo.root, &["--first-parent"]));

    let ours = log_oids(&result);
    for side in [&repo.x1, &repo.x2, &repo.x3] {
        assert!(!ours.contains(side), "no side branch survives --first-parent: {side}");
    }
    assert!(ours.contains(&repo.root_oid), "the mainline root is still there");

    // And every side commit IS reachable without the flag, so the assertions
    // above test the flag rather than commits that were never walked.
    let full = log_oids(&log(&repo.scratch(), "/mnt/repo", &[]).await);
    for side in [&repo.x1, &repo.x2, &repo.x3] {
        assert!(full.contains(side));
    }
}

/// `log --stat` on an octopus merge reports zero files/lines, matching git's
/// default of showing no diffstat for ANY merge regardless of parent count —
/// the same fidelity L8/L9 already pin for a two-parent merge, now checked
/// against a real 4-parent one.
#[tokio::test]
async fn octopus_stat_reports_no_lines_matching_git() {
    let repo = OctopusRepo::build();
    let result = log(&repo.scratch(), "/mnt/repo", &["--stat", "--merges"]).await;
    assert_eq!(result.code, 0, "stderr: {}", result.err);
    let stat = &json(&result)["commits"][0]["stat"];
    assert_eq!(stat["files"], 0);
    assert_eq!(stat["additions"], 0);
    assert_eq!(stat["deletions"], 0);

    // `git log --stat`, not `git show --stat`: the two disagree on a merge.
    // `git show <merge>` defaults to a combined (`--cc`-like) diff and DOES
    // print a stat — confirmed against git 2.55 on both a 2-parent and this
    // 3-parent fixture — while `git log --stat` (what `log --stat` in this
    // crate models) shows nothing for any merge, parent count included. Using
    // `show` here would have manufactured a divergence out of asking the
    // wrong oracle.
    let oracle = git(&repo.root, &["log", "--stat", "--format=", "-1", &repo.merge_oid()]);
    assert!(
        oracle.trim().is_empty(),
        "git log --stat shows no diffstat for a merge by default, octopus included: {oracle:?}"
    );
}

/// `^3` resolves the third parent (1-based) of an octopus merge — git's
/// `b2` branch tip in parent order `[root, b1, b2, b3]` — agreeing with
/// `git rev-parse <merge>^3`. `nth_parent`'s `parents.get(n - 1)` is generic
/// in the parent count, and this is the first fixture that actually gives it
/// a third parent to find.
#[tokio::test]
async fn octopus_third_parent_revspec_matches_git() {
    let repo = OctopusRepo::build();
    let merge = repo.merge_oid();
    let expected = git(&repo.root, &["rev-parse", &format!("{merge}^3")]);
    assert_eq!(expected, repo.x2, "sanity: ^3 must be b2's tip in this fixture");

    let result = log(&repo.scratch(), "/mnt/repo", &["--rev", &format!("{merge}^3"), "--limit", "1"]).await;
    assert_eq!(result.code, 0, "stderr: {}", result.err);
    assert_eq!(log_oids(&result)[0], expected);
}

// ═══════════════════════════════════════════════════════════════════════════
// 4. `status` on an unborn HEAD
// ═══════════════════════════════════════════════════════════════════════════

/// A fresh repository with no commits at all: `head_tree_id`'s "unborn
/// branch" case, compared against real `git status --porcelain` on the same
/// tree (one staged file via `git add`, one untracked).
#[tokio::test]
async fn status_on_a_fresh_unborn_repository_matches_git() {
    require_git();
    let fixture = Fixture::empty();
    let root = fixture.path("repo");
    std::fs::create_dir_all(&root).expect("create repo dir");
    git(&root, &["init", "--initial-branch=main", "--quiet"]);

    write_file(&root, "a.txt", "one\n");
    git(&root, &["add", "a.txt"]);
    write_file(&root, "b.txt", "not staged\n");

    let result = status_run(&fixture.root(), "/mnt/repo", &[]).await;
    assert_eq!(result.code, 0, "status must not error on an unborn HEAD: {}", result.err);

    let model = json(&result);
    assert!(model["head"]["oid"].is_null(), "unborn HEAD has no commit: {model}");
    assert_eq!(model["head"]["branch"], "main");
    assert_eq!(model["head"]["detached"], false);

    // Every staged file is an addition against the empty tree — the same
    // shape `flatten_head_tree`'s doc comment claims.
    assert_eq!(model["totals"]["staged"], 1);
    assert_eq!(model["totals"]["untracked"], 1);
    assert_eq!(model["clean"], false);

    // Oracle: real git status on the same tree.
    let oracle = git(&root, &["status", "--porcelain=v1"]);
    let mut oracle_lines: Vec<&str> = oracle.lines().collect();
    oracle_lines.sort_unstable();
    assert_eq!(oracle_lines, vec!["?? b.txt", "A  a.txt"], "sanity: git's own porcelain on this tree");
}

/// A totally empty unborn repository — no files at all — is reported clean,
/// matching git's own silent `status --porcelain` on the same tree.
#[tokio::test]
async fn status_on_a_totally_empty_unborn_repository_is_clean() {
    require_git();
    let fixture = Fixture::empty();
    let root = fixture.path("repo");
    std::fs::create_dir_all(&root).expect("create repo dir");
    git(&root, &["init", "--initial-branch=main", "--quiet"]);

    let result = status_run(&fixture.root(), "/mnt/repo", &[]).await;
    assert_eq!(result.code, 0, "stderr: {}", result.err);
    let model = json(&result);
    assert_eq!(model["clean"], true);
    assert_eq!(model["entries"].as_array().map(Vec::len), Some(0));

    let oracle = git(&root, &["status", "--porcelain=v1"]);
    assert!(oracle.is_empty(), "git itself reports nothing for an empty unborn tree: {oracle:?}");
}

// ═══════════════════════════════════════════════════════════════════════════
// 5. Deep tag chains — `show`'s 8-level bound, `peel_tag_chain`'s 32-level one
// ═══════════════════════════════════════════════════════════════════════════

/// Build a chain of `n` nested annotated tags on top of `base`: `chain0`
/// tags `base` directly, `chain1` tags `chain0`, ..., `chain{n-1}` tags
/// `chain{n-2}`. Returns the outermost tag's name — the one a caller would
/// pass to `show`/list with `git tag`.
fn build_tag_chain(root: &Path, base: &str, n: usize) -> String {
    assert!(n >= 1, "a chain needs at least one tag");
    let mut prev = base.to_string();
    let mut name = String::new();
    for i in 0..n {
        name = format!("chain{i}");
        git(root, &["tag", "-a", &name, "-m", &format!("chain tag {i}"), &prev]);
        prev = name.clone();
    }
    name
}

/// `MAX_TAG_DEPTH` in `verbs/show.rs` is 8, read from the source rather than
/// assumed: `build_show_tag` refuses once `depth >= 8`, and depth is the
/// number of tag-to-tag hops already taken when a given tag is reached. A
/// chain of exactly 8 nested annotated tags (`chain0..chain7`) is reached at
/// depths 0..7 — all under the bound — so `show` on the outermost tag must
/// still succeed and peel all the way to the base commit.
#[tokio::test]
async fn show_follows_a_tag_chain_exactly_eight_deep() {
    let (fixture, root, base) = one_commit_repo("repo");
    let outer = build_tag_chain(&root, &base, 8);

    // Sanity: git itself has no trouble with this depth either.
    let peeled = git(&root, &[&format!("rev-parse"), &format!("{outer}^{{commit}}")]);
    assert_eq!(peeled, base, "sanity: git peels 8 levels of tags fine");

    let result = show_run(&fixture.root(), "/mnt/repo", &[&outer]).await;
    assert_eq!(result.code, 0, "show must accept exactly 8 levels of tag nesting: {}", result.err);
}

/// One level deeper (9 tags, `chain0..chain8`) crosses `MAX_TAG_DEPTH`:
/// `build_show_tag` is called for `chain0` at depth 8, which fails the
/// `depth >= 8` check before reading it. Real git has no such bound and peels
/// straight through. Pinned with git's own answer beside ours:
/// docs/issues.md V1.
#[tokio::test]
async fn show_refuses_a_tag_chain_nine_deep_where_git_has_no_bound() {
    let (fixture, root, base) = one_commit_repo("repo");
    let outer = build_tag_chain(&root, &base, 9);

    // Oracle: git peels straight through — no bound at all.
    let peeled = git(&root, &["rev-parse", &format!("{outer}^{{commit}}")]);
    assert_eq!(peeled, base, "git has no depth bound peeling tags");

    let result = show_run(&fixture.root(), "/mnt/repo", &[&outer]).await;
    assert_ne!(result.code, 0, "9 levels of tag nesting must be refused, not silently truncated");
    assert!(
        result.err.contains('8') && result.err.contains("levels of tag nesting"),
        "the refusal names the bound: {}",
        result.err
    );
}

/// `ReadRepo::peel_tag_chain`'s own loop bound (`repo.rs`) is 32 iterations,
/// and every `git tag` row calls it once to decide lightweight-vs-annotated
/// and to report `target_oid`/`target_kind`. Each iteration consumes one tag
/// object plus one final iteration to confirm the non-tag terminal object, so
/// a chain of 31 nested tags (`chain0..chain30`, 32 iterations exactly)
/// succeeds — read from the source, not assumed.
#[tokio::test]
async fn tag_listing_survives_a_tag_chain_31_deep() {
    let (fixture, root, base) = one_commit_repo("repo");
    let outer = build_tag_chain(&root, &base, 31);

    let result = tag_run(&fixture.root(), "/mnt/repo", &["--json"]).await;
    assert_eq!(result.code, 0, "31 levels of tag nesting must not fail the listing: {}", result.err);
    let names: Vec<String> = json(&result)["tags"]
        .as_array()
        .expect("tags array")
        .iter()
        .map(|t| t["name"].as_str().expect("name").to_string())
        .collect();
    assert!(names.contains(&outer), "the outer tag must be listed: {names:?}");
}

/// One tag deeper (`chain0..chain31`, 32 tags) needs 33 iterations and trips
/// `peel_tag_chain`'s bound — and because `verbs/tag.rs`'s listing loop
/// propagates that error with a bare `?`, **the whole listing fails**, taking
/// down every other, perfectly ordinary tag in the repository with it. A
/// second, unrelated tag (`z-ordinary`) on the same base commit proves this:
/// it is a perfectly good annotated tag, and it still never appears anywhere
/// because the ref iteration hits the bad chain and aborts. Real git has no
/// such bound — `git tag -l`/`rev-parse ...^{commit}` both handle 32 levels
/// without complaint.
///
/// This is a different shape from B6's "skip the one bad row, report the
/// rest, and say how many were skipped" — here ONE bad tag hides every good
/// one, silently, with no count reported at all. Pinned with git's own
/// answer beside ours: docs/issues.md V2.
#[tokio::test]
async fn tag_listing_fails_whole_on_a_tag_chain_32_deep_where_git_has_no_bound() {
    let (fixture, root, base) = one_commit_repo("repo");
    let outer = build_tag_chain(&root, &base, 32);
    git(&root, &["tag", "-a", "z-ordinary", "-m", "an unrelated, perfectly fine tag", &base]);

    // Oracle: git lists both tags fine, and peels the deep chain fine too.
    let oracle_names = git(&root, &["tag", "-l"]);
    assert!(oracle_names.lines().any(|l| l == "z-ordinary"));
    assert!(oracle_names.lines().any(|l| l == outer));
    let peeled = git(&root, &["rev-parse", &format!("{outer}^{{commit}}")]);
    assert_eq!(peeled, base, "git has no depth bound peeling tags");

    let result = tag_run(&fixture.root(), "/mnt/repo", &["--json"]).await;
    assert_ne!(
        result.code, 0,
        "32 levels of tag nesting must fail SOMETHING, or this test no longer exercises the bound"
    );
    assert!(
        result.err.contains("32") && result.err.contains("tag chain"),
        "the refusal names the bound: {}",
        result.err
    );
    // The point of the entry: `z-ordinary` is completely unrelated to the
    // deep chain, and it still never gets a row — one bad ref hides every
    // good one, with no count of what was hidden.
}

// ═══════════════════════════════════════════════════════════════════════════
// 6. Non-UTF-8 names — a tracked tree entry, and an untracked worktree file
// ═══════════════════════════════════════════════════════════════════════════

/// A filename made of raw, non-UTF-8 bytes — created directly with
/// `std::fs::write` (never passed as a process argument, which would force
/// choosing an encoding for it). `git add .` / `git commit` need no filename
/// argument, so the byte content never has to survive a shell round trip.
#[cfg(unix)]
fn write_non_utf8_named_file(dir: &Path, prefix: &str, contents: &[u8]) {
    use std::os::unix::ffi::OsStrExt;
    let mut bytes = prefix.as_bytes().to_vec();
    bytes.push(0xFF);
    bytes.push(0xFE);
    bytes.extend_from_slice(b".bin");
    let name = std::ffi::OsStr::from_bytes(&bytes).to_os_string();
    std::fs::write(dir.join(&name), contents).expect("write non-utf8-named file");
}

/// A tracked file whose name is not valid UTF-8 renders lossily (U+FFFD) in
/// our tree listing, where `git ls-tree` C-quotes the same name as printable
/// ASCII (`"bad_\377\376name.bin"`) — confirmed against git 2.55 with its
/// default `core.quotepath=true`. Pinned with git's own answer beside ours:
/// docs/issues.md N1.
#[tokio::test]
#[cfg(unix)]
async fn tree_listing_renders_a_non_utf8_name_lossily_where_git_c_quotes_it() {
    require_git();
    let fixture = Fixture::empty();
    let root = fixture.path("repo");
    std::fs::create_dir_all(&root).expect("create repo dir");
    git(&root, &["init", "--initial-branch=main", "--quiet"]);
    write_non_utf8_named_file(&root, "bad_", b"hello\n");
    git(&root, &["add", "."]);
    git(&root, &["commit", "-m", "base", "--quiet"]);

    // Oracle: git C-quotes the non-UTF-8 bytes as octal escapes.
    let oracle = git(&root, &["ls-tree", "HEAD"]);
    assert!(
        oracle.contains("\\377\\376"),
        "git must C-quote the invalid bytes: {oracle:?}"
    );

    let result = ls_run(&fixture.root(), "/mnt/repo", &["--json"]).await;
    assert_eq!(result.code, 0, "stderr: {}", result.err);
    let paths: Vec<String> = json(&result)["entries"]
        .as_array()
        .expect("entries array")
        .iter()
        .map(|e| e["path"].as_str().expect("path").to_string())
        .collect();
    assert_eq!(paths.len(), 1, "exactly one tracked file: {paths:?}");
    assert!(
        paths[0].contains('\u{FFFD}'),
        "the name must render lossily (U+FFFD), not C-quoted like git: {paths:?}"
    );
}

/// An UNTRACKED file whose name is not valid UTF-8 is silently absent from
/// `status` entirely — not even a mangled row — because
/// `walk_untracked_and_ignored`'s directory scan only keeps a name that
/// `OsString::into_string()` accepts. Git reports it (C-quoted). An ordinary
/// UTF-8 untracked file in the same directory IS reported by both sides, so
/// the omission is specifically about the encoding, not about untracked
/// files in general. Pinned with git's own answer beside ours:
/// docs/issues.md N2.
#[tokio::test]
#[cfg(unix)]
async fn untracked_walk_silently_omits_a_non_utf8_named_file_where_git_reports_it() {
    require_git();
    let fixture = Fixture::empty();
    let root = fixture.path("repo");
    std::fs::create_dir_all(&root).expect("create repo dir");
    git(&root, &["init", "--initial-branch=main", "--quiet"]);
    write_file(&root, "tracked.txt", "tracked\n");
    git(&root, &["add", "."]);
    git(&root, &["commit", "-m", "base", "--quiet"]);

    write_file(&root, "keep.txt", "an ordinary untracked file\n");
    write_non_utf8_named_file(&root, "odd_", b"nope\n");

    // Oracle: git reports BOTH untracked files (the odd one quoted).
    let oracle = git(&root, &["status", "--porcelain=v1"]);
    let oracle_lines: Vec<&str> = oracle.lines().collect();
    assert_eq!(oracle_lines.len(), 2, "git must report both untracked files: {oracle_lines:?}");
    assert!(oracle_lines.contains(&"?? keep.txt"), "{oracle_lines:?}");
    assert!(
        oracle_lines.iter().any(|l| l.starts_with("?? \"odd_")),
        "git quotes the odd name: {oracle_lines:?}"
    );

    let result = status_run(&fixture.root(), "/mnt/repo", &[]).await;
    assert_eq!(result.code, 0, "stderr: {}", result.err);
    let model = json(&result);
    let paths: Vec<String> = model["entries"]
        .as_array()
        .expect("entries array")
        .iter()
        .map(|e| e["path"].as_str().expect("path").to_string())
        .collect();

    // Present thing: the ordinary untracked file IS reported (negative
    // control — this proves the walk ran at all).
    assert!(paths.contains(&"keep.txt".to_string()), "{paths:?}");
    // Absent thing: the non-UTF-8-named file is nowhere — not as a row, not
    // lossily renamed, not counted.
    assert_eq!(paths.len(), 1, "the odd file must be silently absent: {paths:?}");
    assert_eq!(model["totals"]["untracked"], 1, "only the ordinary file is counted");
}

// ═══════════════════════════════════════════════════════════════════════════
// 7. `--path ""` — silently matches everything here, where git refuses
// ═══════════════════════════════════════════════════════════════════════════

/// `PathFilter::parse` trims an empty `--path` value and `continue`s past it
/// without adding a `Spec`, so an empty-string-only filter ends up with zero
/// specs — and an empty `PathFilter` matches everything by construction. Git
/// refuses the same input outright. Shared by `status` and `log` (one
/// `PathFilter` implementation), so both are exercised. Pinned with git's own
/// answer beside ours: docs/issues.md F1.
#[tokio::test]
async fn log_path_empty_string_matches_everything_where_git_refuses() {
    let (fixture, root, _base) = one_commit_repo("repo");
    write_file(&root, "src/lib.rs", "fn a() {}\n");
    git(&root, &["add", "."]);
    git(&root, &["commit", "-m", "add src", "--quiet"]);

    let unfiltered = log(&fixture.root(), "/mnt/repo", &[]).await;
    let with_empty_path = log(&fixture.root(), "/mnt/repo", &["--path", ""]).await;
    assert_eq!(with_empty_path.code, 0, "stderr: {}", with_empty_path.err);
    assert_eq!(
        log_oids(&with_empty_path),
        log_oids(&unfiltered),
        "an empty --path silently matches everything, same as no filter at all"
    );

    // Oracle: git refuses an empty pathspec outright.
    let oracle = git_allow_fail(&root, &["log", "--", ""]);
    assert!(!oracle.status.success(), "git must refuse an empty pathspec");
    assert!(
        String::from_utf8_lossy(&oracle.stderr).contains("empty string is not a valid pathspec"),
        "git names the problem: {}",
        String::from_utf8_lossy(&oracle.stderr)
    );
}

/// The same divergence, through `status`.
#[tokio::test]
async fn status_path_empty_string_matches_everything_where_git_refuses() {
    let (fixture, root, _base) = one_commit_repo("repo");
    write_file(&root, "f.txt", "changed\n");

    let unfiltered = status_run(&fixture.root(), "/mnt/repo", &[]).await;
    let with_empty_path = status_run(&fixture.root(), "/mnt/repo", &["--path", ""]).await;
    assert_eq!(with_empty_path.code, 0, "stderr: {}", with_empty_path.err);
    assert_eq!(
        json(&with_empty_path)["entries"],
        json(&unfiltered)["entries"],
        "an empty --path silently matches everything, same as no filter at all"
    );
    assert_ne!(
        json(&unfiltered)["entries"].as_array().map(Vec::len),
        Some(0),
        "sanity: the fixture has something to report, so the assertion above is not vacuous"
    );

    // Oracle: git refuses an empty pathspec outright.
    let oracle = git_allow_fail(&root, &["status", "--", ""]);
    assert!(!oracle.status.success(), "git must refuse an empty pathspec");
    assert!(
        String::from_utf8_lossy(&oracle.stderr).contains("empty string is not a valid pathspec"),
        "git names the problem: {}",
        String::from_utf8_lossy(&oracle.stderr)
    );
}

// ═══════════════════════════════════════════════════════════════════════════
// 8. `core.autocrlf true` — C5, exercised for real
// ═══════════════════════════════════════════════════════════════════════════

/// Every existing fixture in this crate sets `core.autocrlf false` (or never
/// touches it). With it `true`: git's clean filter normalizes CRLF to LF when
/// content is staged, so the committed blob and the index entry both hold
/// LF, while the working-tree file — never re-checked-out after the commit —
/// still holds the CRLF bytes it was written with. Git's own `status`
/// compares through the same filter and sees no difference; this crate
/// hashes the worktree's raw bytes against the blob oid directly (C5's own
/// reasoning: "hermetic by design"), so it reports a spurious modification.
/// C5 recorded this in prose; this is the fixture that exercises it.
#[tokio::test]
async fn status_with_autocrlf_true_reports_a_spurious_modification_git_does_not() {
    require_git();
    let fixture = Fixture::empty();
    let root = fixture.path("repo");
    std::fs::create_dir_all(&root).expect("create repo dir");
    git(&root, &["init", "--initial-branch=main", "--quiet"]);
    git(&root, &["config", "core.autocrlf", "true"]);

    std::fs::write(root.join("f.txt"), b"one\r\ntwo\r\n").expect("write crlf file");
    git(&root, &["add", "f.txt"]);
    git(&root, &["commit", "-m", "base", "--quiet"]);

    // Sanity: the blob really was normalized to LF, and the worktree file
    // really is still CRLF — the two preconditions this divergence needs.
    let blob = git(&root, &["show", "HEAD:f.txt"]);
    assert_eq!(blob, "one\ntwo", "the committed blob must be LF-normalized");
    let worktree_bytes = std::fs::read(root.join("f.txt")).expect("read worktree file");
    assert_eq!(worktree_bytes, b"one\r\ntwo\r\n", "the worktree file must still be CRLF");

    // Oracle: git status is clean — its comparison goes through the same
    // clean filter that normalized the blob.
    let oracle = git(&root, &["status", "--porcelain=v1"]);
    assert!(oracle.is_empty(), "git must report a clean tree: {oracle:?}");

    // Ours: a raw byte hash of the worktree file against the (LF) blob oid
    // disagrees, and reports a spurious unstaged modification.
    let result = status_run(&fixture.root(), "/mnt/repo", &[]).await;
    assert_eq!(result.code, 0, "stderr: {}", result.err);
    let model = json(&result);
    assert_eq!(model["clean"], false, "we report the tree dirty where git does not");
    assert_eq!(model["totals"]["unstaged"], 1);
    let entries = model["entries"].as_array().expect("entries array");
    assert_eq!(entries.len(), 1);
    assert_eq!(entries[0]["path"], "f.txt");
    assert_eq!(entries[0]["worktree"], "modified");
}
