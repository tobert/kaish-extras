//! `git status` behavior, with real git as the oracle (architecture.md B.2, H
//! PR 2).
//!
//! Every test here can fail: each asserts a specific composed answer, and the
//! cross-check tests hold our output against `git status --porcelain=v1` on the
//! same fixture. A read-only-but-wrong status — the failure mode the fingerprint
//! test (D.4) cannot catch, because a wrong answer touches nothing — fails here.

#[path = "support.rs"]
mod support;

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Arc;

use kaish_tool_api::Tool;
use kaish_types::{ExecResult, ToolArgs, Value};

use kaish_tools_git::{GitConfig, Limits};

use support::{git, require_git, write_file, Fixture, StrictBackend, TestCtx};

/// The VFS root the fixture scratch directory is mounted at.
const MOUNT: &str = "/mnt";

/// Build the `ToolArgs` the kernel would, from an argv slice. `--flag value`
/// binds a named argument, a lone `--flag` binds a flag, and the verb word
/// leads the positionals.
fn tool_args(verb: &str, argv: &[&str]) -> ToolArgs {
    let mut args = ToolArgs::new();
    args.positional.push(Value::String(verb.to_string()));
    let mut i = 0;
    while i < argv.len() {
        let token = argv[i]
            .strip_prefix("--")
            .unwrap_or_else(|| panic!("argv must use long flags: {}", argv[i]));
        match argv.get(i + 1) {
            Some(value) if !value.starts_with("--") => {
                args.named
                    .insert(token.to_string(), Value::String((*value).to_string()));
                i += 2;
            }
            _ => {
                args.flags.insert(token.to_string());
                i += 1;
            }
        }
    }
    args
}

/// Run `git status` against `mount_real` mounted at `/mnt`, from `cwd`.
async fn status(mount_real: &Path, cwd: &str, argv: &[&str]) -> ExecResult {
    status_with(GitConfig::read_only(), mount_real, cwd, argv).await
}

/// The same, with an embedder config the test chose — how a [`Limits`] cap is
/// exercised without a fixture large enough to hit the default.
async fn status_with(
    config: GitConfig,
    mount_real: &Path,
    cwd: &str,
    argv: &[&str],
) -> ExecResult {
    let backend = Arc::new(StrictBackend::single(PathBuf::from(MOUNT), mount_real.to_path_buf()));
    let mut ctx = TestCtx::new(backend, cwd);
    let tool = kaish_tools_git::tool(config).expect("config");
    tool.execute(tool_args("status", argv), &mut ctx).await
}


/// The typed model out of a `--json` result.
fn json(result: &ExecResult) -> serde_json::Value {
    assert_eq!(result.code, 0, "status failed: {}", result.err);
    result
        .output()
        .and_then(|o| o.rich_json.clone())
        .expect("--json carries the typed model")
}

/// The rendered porcelain rows: `(XY, path)` pairs from the text table.
fn rows(result: &ExecResult) -> Vec<(String, String)> {
    result
        .output()
        .expect("output")
        .root
        .iter()
        .map(|node| (node.name.clone(), node.cells.first().cloned().unwrap_or_default()))
        .collect()
}

/// Run git without asserting success — for the merge that is *meant* to
/// conflict, where a nonzero exit is the point. Same hermetic environment as
/// `support::git`.
fn git_allow_fail(cwd: &Path, args: &[&str]) -> std::process::Output {
    Command::new("git")
        .args(args)
        .current_dir(cwd)
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_SYSTEM", "/dev/null")
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_AUTHOR_NAME", "Fixture Author")
        .env("GIT_AUTHOR_EMAIL", "author@example.invalid")
        .env("GIT_COMMITTER_NAME", "Fixture Committer")
        .env("GIT_COMMITTER_EMAIL", "committer@example.invalid")
        .env("GIT_AUTHOR_DATE", "2026-08-01T10:00:00+00:00")
        .env("GIT_COMMITTER_DATE", "2026-08-01T10:00:00+00:00")
        .output()
        .unwrap_or_else(|e| panic!("git {args:?}: {e}"))
}

/// A repository under a fresh scratch mount, initialized empty.
struct Repo {
    fixture: Fixture,
    root: PathBuf,
}

impl Repo {
    /// `git init` a repository named `name` under the scratch mount.
    fn init(name: &str) -> Self {
        require_git();
        let fixture = Fixture::empty();
        let root = fixture.path(name);
        std::fs::create_dir_all(&root).expect("create repo dir");
        git(&root, &["init", "--initial-branch=main", "--quiet"]);
        git(&root, &["config", "gc.writeCommitGraph", "false"]);
        Repo { fixture, root }
    }

    /// The scratch directory the mount maps to `/mnt`.
    fn mount(&self) -> PathBuf {
        self.fixture.root()
    }

    fn write(&self, rel: &str, contents: &str) {
        write_file(&self.root, rel, contents);
    }

    fn git(&self, args: &[&str]) -> String {
        git(&self.root, args)
    }
}

/// `git status --porcelain=v1` as `(XY, path)` pairs — the oracle. Renames are
/// reduced to `(XY, new_path)` so both sides compare on the destination, and a
/// trailing `/` on an untracked directory is dropped to match our path model.
fn porcelain_oracle(repo_root: &Path) -> BTreeSet<(String, String)> {
    let out = git(repo_root, &["status", "--porcelain=v1"]);
    let mut set = BTreeSet::new();
    for line in out.lines() {
        if line.len() < 3 {
            continue;
        }
        let xy = line[..2].to_string();
        let rest = &line[3..];
        // A rename/copy line is `XY orig -> new`; take the destination.
        let path = match rest.split_once(" -> ") {
            Some((_orig, new)) => new,
            None => rest,
        };
        let path = path.trim_matches('"').trim_end_matches('/');
        set.insert((xy, path.to_string()));
    }
    set
}

/// Our rendered rows as the same `(XY, new_path)` pairs.
fn our_oracle(result: &ExecResult) -> BTreeSet<(String, String)> {
    rows(result)
        .into_iter()
        .map(|(xy, path)| {
            // Our rename rows render `new ← orig`; take the destination.
            let path = path.split(" ← ").next().unwrap_or(&path).to_string();
            (xy, path)
        })
        .collect()
}

/// A dirty repository: one staged modification, one unstaged modification, one
/// untracked file — no renames, so git's default similarity detection and our
/// exact-only detection cannot disagree.
fn dirty_repo() -> Repo {
    let repo = Repo::init("repo");
    repo.write("README.md", "hello\n");
    repo.write("src/lib.rs", "fn one() {}\n");
    repo.git(&["add", "."]);
    repo.git(&["commit", "-m", "initial", "--quiet"]);

    repo.write("README.md", "hello\nstaged line\n");
    repo.git(&["add", "README.md"]);
    repo.write("src/lib.rs", "fn one() {}\n// unstaged\n");
    repo.write("untracked.txt", "nope\n");
    repo
}

// ═══════════════════════════════════════════════════════════════════════════
// The oracle: agree with real git
// ═══════════════════════════════════════════════════════════════════════════

#[tokio::test]
async fn status_agrees_with_real_git() {
    let repo = dirty_repo();
    let result = status(&repo.mount(), "/mnt/repo", &["--json"]).await;

    let ours = our_oracle(&result);
    let theirs = porcelain_oracle(&repo.root);
    assert_eq!(
        ours, theirs,
        "our porcelain disagrees with `git status --porcelain=v1`"
    );

    // And the totals it implies: one staged, one unstaged, one untracked.
    let j = json(&result);
    assert_eq!(j["totals"]["staged"], 1, "{j}");
    assert_eq!(j["totals"]["unstaged"], 1, "{j}");
    assert_eq!(j["totals"]["untracked"], 1, "{j}");
    assert_eq!(j["totals"]["conflicted"], 0);
    assert_eq!(j["clean"], false);
}

/// A clean checkout is clean, with no entries — the base case the dirty tests
/// vary from.
#[tokio::test]
async fn a_clean_tree_is_clean() {
    let repo = Repo::init("repo");
    repo.write("f.txt", "content\n");
    repo.git(&["add", "."]);
    repo.git(&["commit", "-m", "only commit", "--quiet"]);

    let result = status(&repo.mount(), "/mnt/repo", &["--json"]).await;
    let j = json(&result);
    assert_eq!(j["clean"], true, "{j}");
    assert_eq!(j["entries"].as_array().expect("entries").len(), 0);
    assert_eq!(j["head"]["branch"], "main");
}

// ═══════════════════════════════════════════════════════════════════════════
// Renames: exact-match only, honestly reported
// ═══════════════════════════════════════════════════════════════════════════

/// An exact rename (`git mv`, identical content) is detected: index `renamed`,
/// `orig_path` set to the source.
#[tokio::test]
async fn an_exact_rename_is_detected() {
    let repo = Repo::init("repo");
    repo.write("old.txt", "unchanged content\n");
    repo.git(&["add", "."]);
    repo.git(&["commit", "-m", "add old", "--quiet"]);
    repo.git(&["mv", "old.txt", "new.txt"]);

    let result = status(&repo.mount(), "/mnt/repo", &["--json"]).await;
    let j = json(&result);
    let entries = j["entries"].as_array().expect("entries");
    let rename = entries
        .iter()
        .find(|e| e["path"] == "new.txt")
        .unwrap_or_else(|| panic!("expected a rename entry for new.txt: {j}"));
    assert_eq!(rename["index"], "renamed", "{rename}");
    assert_eq!(rename["orig_path"], "old.txt", "{rename}");
    // No stray delete of the source: the rename folds the pair into one row.
    assert!(
        !entries.iter().any(|e| e["path"] == "old.txt"),
        "the rename source must not also appear as a delete: {j}"
    );
}

/// A modified-then-moved file is **not** a rename here — the blob oid changed,
/// so the exact-match tracker finds no pair. This is the permanent limitation
/// under this dependency set (no `gix-diff` `blob` feature), reported honestly
/// as a delete plus an add rather than a scored rename.
#[tokio::test]
async fn a_modified_then_moved_file_is_not_a_rename() {
    let repo = Repo::init("repo");
    repo.write("a.txt", "line one\nline two\nline three\n");
    repo.git(&["add", "."]);
    repo.git(&["commit", "-m", "add a", "--quiet"]);
    // Move *and* change content — enough that the oid differs.
    std::fs::remove_file(repo.root.join("a.txt")).expect("remove a.txt");
    repo.write("b.txt", "line one\nline two\nline three\nline four added\n");
    repo.git(&["add", "-A"]);

    let result = status(&repo.mount(), "/mnt/repo", &["--json"]).await;
    let j = json(&result);
    let entries = j["entries"].as_array().expect("entries");
    assert!(
        entries.iter().all(|e| e["index"] != "renamed"),
        "a modified-then-moved file must not be scored as a rename: {j}"
    );
    let a = entries.iter().find(|e| e["path"] == "a.txt").expect("a.txt entry");
    assert_eq!(a["index"], "deleted", "{a}");
    let b = entries.iter().find(|e| e["path"] == "b.txt").expect("b.txt entry");
    assert_eq!(b["index"], "added", "{b}");
}

// ═══════════════════════════════════════════════════════════════════════════
// Conflicts
// ═══════════════════════════════════════════════════════════════════════════

/// A real `git merge` conflict: the path is `conflicted`, counted in
/// `totals.conflicted`, and renders the porcelain `UU` git spells.
#[tokio::test]
async fn a_merge_conflict_is_reported() {
    let repo = Repo::init("repo");
    repo.write("f.txt", "base\n");
    repo.git(&["add", "."]);
    repo.git(&["commit", "-m", "base", "--quiet"]);

    repo.git(&["checkout", "-b", "other", "--quiet"]);
    repo.write("f.txt", "their change\n");
    repo.git(&["commit", "-am", "theirs", "--quiet"]);

    repo.git(&["checkout", "main", "--quiet"]);
    repo.write("f.txt", "our change\n");
    repo.git(&["commit", "-am", "ours", "--quiet"]);

    let merge = git_allow_fail(&repo.root, &["merge", "other"]);
    assert!(
        !merge.status.success(),
        "the merge was supposed to conflict; fixture is wrong"
    );

    let result = status(&repo.mount(), "/mnt/repo", &["--json"]).await;
    let j = json(&result);
    assert_eq!(j["totals"]["conflicted"], 1, "{j}");
    let entry = j["entries"]
        .as_array()
        .expect("entries")
        .iter()
        .find(|e| e["path"] == "f.txt")
        .unwrap_or_else(|| panic!("no f.txt entry: {j}"));
    assert_eq!(entry["conflicted"], true, "{entry}");

    let ours = our_oracle(&result);
    assert!(
        ours.contains(&("UU".to_string(), "f.txt".to_string())),
        "a both-modified conflict must render UU: {ours:?}"
    );
    // And git agrees it is UU.
    let theirs = porcelain_oracle(&repo.root);
    assert!(theirs.contains(&("UU".to_string(), "f.txt".to_string())), "{theirs:?}");
}

// ═══════════════════════════════════════════════════════════════════════════
// Untracked modes and ignored
// ═══════════════════════════════════════════════════════════════════════════

/// `--untracked no|normal|all` differ exactly: `no` hides untracked entirely,
/// `normal` collapses a wholly-untracked directory to one `dir` row, `all`
/// recurses and lists each file.
#[tokio::test]
async fn untracked_modes_differ() {
    let repo = Repo::init("repo");
    repo.write("tracked.txt", "t\n");
    repo.git(&["add", "."]);
    repo.git(&["commit", "-m", "one tracked file", "--quiet"]);
    repo.write("top.txt", "untracked top\n");
    repo.write("sub/deep.txt", "untracked deep\n");

    let no = json(&status(&repo.mount(), "/mnt/repo", &["--untracked", "no", "--json"]).await);
    assert_eq!(no["totals"]["untracked"], 0, "-uno hides untracked: {no}");

    let normal = json(&status(&repo.mount(), "/mnt/repo", &["--untracked", "normal", "--json"]).await);
    let normal_paths: BTreeSet<String> = normal["entries"]
        .as_array()
        .unwrap()
        .iter()
        .map(|e| e["path"].as_str().unwrap().to_string())
        .collect();
    assert!(normal_paths.contains("top.txt"), "{normal}");
    assert!(
        normal_paths.contains("sub"),
        "normal mode collapses the untracked dir to `sub`: {normal}"
    );
    assert!(
        !normal_paths.contains("sub/deep.txt"),
        "normal mode must not descend the untracked dir: {normal}"
    );

    let all = json(&status(&repo.mount(), "/mnt/repo", &["--untracked", "all", "--json"]).await);
    let all_paths: BTreeSet<String> = all["entries"]
        .as_array()
        .unwrap()
        .iter()
        .map(|e| e["path"].as_str().unwrap().to_string())
        .collect();
    assert!(all_paths.contains("top.txt"), "{all}");
    assert!(
        all_paths.contains("sub/deep.txt"),
        "all mode lists each untracked file: {all}"
    );
    assert!(
        !all_paths.contains("sub"),
        "all mode reports the files, not the collapsed dir: {all}"
    );
}

/// `--ignored` toggles ignored entries, and an ignored file is never reported
/// as untracked.
#[tokio::test]
async fn ignored_toggles_with_the_flag() {
    let repo = Repo::init("repo");
    repo.write(".gitignore", "*.log\n");
    repo.git(&["add", "."]);
    repo.git(&["commit", "-m", "gitignore", "--quiet"]);
    repo.write("debug.log", "ignored\n");
    repo.write("keep.txt", "untracked\n");

    let off = json(&status(&repo.mount(), "/mnt/repo", &["--json"]).await);
    assert_eq!(off["totals"]["ignored"], 0, "ignored are hidden by default: {off}");
    let off_paths: BTreeSet<String> = off["entries"]
        .as_array()
        .unwrap()
        .iter()
        .map(|e| e["path"].as_str().unwrap().to_string())
        .collect();
    assert!(off_paths.contains("keep.txt"));
    assert!(
        !off_paths.contains("debug.log"),
        "an ignored file must not surface as untracked: {off}"
    );

    let on = json(&status(&repo.mount(), "/mnt/repo", &["--ignored", "--json"]).await);
    assert_eq!(on["totals"]["ignored"], 1, "{on}");
    let ignored = on["entries"]
        .as_array()
        .unwrap()
        .iter()
        .find(|e| e["path"] == "debug.log")
        .unwrap_or_else(|| panic!("debug.log must appear with --ignored: {on}"));
    assert_eq!(ignored["index"], "ignored", "{ignored}");
    assert_eq!(ignored["worktree"], "ignored", "{ignored}");
}

// ═══════════════════════════════════════════════════════════════════════════
// Path filter, limit and truncation
// ═══════════════════════════════════════════════════════════════════════════

/// `--path` restricts to a directory; a magic pathspec is a loud usage error.
#[tokio::test]
async fn path_filter_restricts_and_rejects_magic() {
    let repo = Repo::init("repo");
    repo.write("root.txt", "u\n");
    repo.write("src/a.txt", "u\n");
    repo.write("src/b.txt", "u\n");
    repo.git(&["add", "src/a.txt"]);
    repo.git(&["commit", "-m", "seed", "--quiet"]);
    repo.write("src/a.txt", "u changed\n"); // unstaged, under src

    let filtered = json(&status(&repo.mount(), "/mnt/repo", &["--path", "src", "--json"]).await);
    let paths: BTreeSet<String> = filtered["entries"]
        .as_array()
        .unwrap()
        .iter()
        .map(|e| e["path"].as_str().unwrap().to_string())
        .collect();
    assert!(paths.iter().all(|p| p.starts_with("src")), "restricted to src: {filtered}");
    assert!(paths.contains("src/a.txt") || paths.contains("src/b.txt"), "{filtered}");
    assert!(!paths.contains("root.txt"), "{filtered}");

    let magic = status(&repo.mount(), "/mnt/repo", &["--path", ":(exclude)src"]).await;
    assert_eq!(magic.code, 2, "pathspec magic is a usage error: {}", magic.err);
    assert!(magic.err.contains("pathspec magic"), "{}", magic.err);
}

/// `--limit` truncates, and truncation is always reported: `truncated: true` in
/// JSON and a note on stderr, never silent (E.5).
#[tokio::test]
async fn limit_truncates_and_says_so() {
    let repo = Repo::init("repo");
    repo.write("tracked.txt", "t\n");
    repo.git(&["add", "."]);
    repo.git(&["commit", "-m", "seed", "--quiet"]);
    for name in ["a.txt", "b.txt", "c.txt", "d.txt"] {
        repo.write(name, "untracked\n");
    }

    let result = status(&repo.mount(), "/mnt/repo", &["--limit", "2", "--json"]).await;
    let j = json(&result);
    assert_eq!(j["truncated"], true, "over-limit output must be flagged: {j}");
    assert_eq!(j["entries"].as_array().unwrap().len(), 2, "capped to --limit: {j}");
    // The totals are over the untruncated set — a fact about the repo, not the cap.
    assert_eq!(j["totals"]["untracked"], 4, "totals ignore the cap: {j}");
    assert!(
        result.err.contains("truncated"),
        "truncation must also fire a stderr note: {}",
        result.err
    );
}

// ═══════════════════════════════════════════════════════════════════════════
// Text vs JSON: letters in text, words in JSON, and they agree
// ═══════════════════════════════════════════════════════════════════════════

/// The two surfaces are computed from one model, so for every non-conflicted
/// entry the porcelain letter and the JSON word must name the same change.
#[tokio::test]
async fn text_letters_and_json_words_agree() {
    let repo = dirty_repo();
    let result = status(&repo.mount(), "/mnt/repo", &["--json"]).await;
    let j = json(&result);

    // Map each JSON entry's path -> (index word, worktree word, conflicted).
    let rendered = rows(&result);
    for entry in j["entries"].as_array().expect("entries") {
        let path = entry["path"].as_str().unwrap();
        if entry["conflicted"].as_bool().unwrap_or(false) {
            continue; // conflicts carry `U`, which has no word of its own.
        }
        let (xy, row_path) = rendered
            .iter()
            .find(|(_, p)| p.split(" ← ").next() == Some(path))
            .unwrap_or_else(|| panic!("no rendered row for {path}"));
        let mut chars = xy.chars();
        let x = chars.next().unwrap();
        let y = chars.next().unwrap();
        assert_eq!(
            word_to_letter(entry["index"].as_str().unwrap()),
            x,
            "index letter/word disagree for {row_path}"
        );
        assert_eq!(
            word_to_letter(entry["worktree"].as_str().unwrap()),
            y,
            "worktree letter/word disagree for {row_path}"
        );
    }
}

/// The inverse of the JSON word mapping, for the cross-check above.
fn word_to_letter(word: &str) -> char {
    match word {
        "none" => ' ',
        "added" => 'A',
        "modified" => 'M',
        "deleted" => 'D',
        "renamed" => 'R',
        "copied" => 'C',
        "typechange" => 'T',
        "untracked" => '?',
        "ignored" => '!',
        other => panic!("unexpected status word {other:?}"),
    }
}

// ═══════════════════════════════════════════════════════════════════════════
// Containment: an escaping index is refused, not followed
// ═══════════════════════════════════════════════════════════════════════════

/// A `.git/index` symlinked to a file outside the mount is refused (exit 4),
/// the same invariant `discover` holds for every leaf — extended to the index
/// the status read opens. A read-only tool that *followed* the symlink would
/// leak host content while never writing a byte.
#[tokio::test]
async fn a_symlinked_index_escaping_the_mount_is_refused() {
    require_git();
    let fixture = Fixture::empty();

    // A secret outside the mount, and a real repository inside it.
    let outside = fixture.path("outside");
    std::fs::create_dir_all(&outside).expect("create outside");
    write_file(&outside, "secret", "OUTSIDE_INDEX_SECRET\n");

    let mount = fixture.path("mounted");
    let repo = mount.join("repo");
    std::fs::create_dir_all(&repo).expect("create repo");
    git(&repo, &["init", "--initial-branch=main", "--quiet"]);
    write_file(&repo, "f.txt", "inside\n");
    git(&repo, &["add", "."]);
    git(&repo, &["commit", "-m", "inside", "--quiet"]);

    // Point the index at the outside secret.
    let index = repo.join(".git/index");
    std::fs::remove_file(&index).expect("remove real index");
    std::os::unix::fs::symlink(outside.join("secret"), &index).expect("symlink index outside");

    let result = status(&mount, "/mnt/repo", &["--json"]).await;
    assert_eq!(
        result.code, 4,
        "an index symlinked out of the mount must be refused: {} {:?}",
        result.err,
        result.output()
    );
    assert!(result.err.contains("outside the mount"), "{}", result.err);
    let rendered = format!("{} {:?} {:?}", result.err, result.output(), result.baggage);
    assert!(
        !rendered.contains("OUTSIDE_INDEX_SECRET"),
        "outside content reached the caller: {rendered}"
    );
}

/// Status against a bare repository is a git-level error (exit 1), not a crash
/// and not a silent empty answer — a bare repo has no working tree to compare.
#[tokio::test]
async fn status_on_a_bare_repo_needs_a_worktree() {
    require_git();
    let fixture = Fixture::empty();
    let mount = fixture.root();
    let bare = mount.join("bare.git");
    std::fs::create_dir_all(&bare).expect("create bare dir");
    git(&bare, &["init", "--bare", "--quiet"]);

    let result = status(&mount, "/mnt/bare.git", &["--json"]).await;
    assert_eq!(result.code, 1, "bare status is a git-level no: {}", result.err);
    assert!(result.err.contains("working tree"), "{}", result.err);
}

// ═══════════════════════════════════════════════════════════════════════════
// The blob cap (Limits::max_blob_bytes)
// ═══════════════════════════════════════════════════════════════════════════

/// A tracked file larger than the embedder's `max_blob_bytes` is a loud
/// refusal, not an unbounded read.
///
/// Status hashes every tracked file's content, so an unchecked
/// `std::fs::read` is a multi-GB allocation waiting for a repository that
/// wants one. The path is inside the mount and the caller already knows it, so
/// this one names itself — nothing to leak.
#[tokio::test]
async fn a_tracked_file_over_the_blob_cap_is_refused() {
    let repo = Repo::init("repo");
    repo.write("small.txt", "ok\n");
    repo.write("big.txt", &"x".repeat(4096));
    repo.git(&["add", "."]);
    repo.git(&["commit", "-m", "add both", "--quiet"]);

    let config = GitConfig::read_only().with_limits(Limits {
        max_blob_bytes: 64,
        ..Limits::default()
    });
    let result = status_with(config, &repo.mount(), "/mnt/repo", &["--json"]).await;
    assert_eq!(
        result.code, 1,
        "an over-cap tracked file is a git-level no: {} {:?}",
        result.err,
        result.output()
    );
    assert!(result.err.contains("big.txt"), "{}", result.err);
    assert!(result.err.contains("4096"), "{}", result.err);
    assert!(result.err.contains("64"), "{}", result.err);
}

/// The cap's over-refusal guard: a symlink whose *target string* is long is
/// still read, because `read_link` allocates nothing the repository controls
/// beyond a path, and the default cap never fires on ordinary files.
#[tokio::test]
async fn the_blob_cap_does_not_fire_on_symlinks_or_ordinary_files() {
    let repo = Repo::init("repo");
    repo.write("small.txt", "ok\n");
    std::os::unix::fs::symlink("small.txt", repo.root.join("link")).expect("symlink");
    repo.git(&["add", "-A"]);
    repo.git(&["commit", "-m", "add", "--quiet"]);

    let config = GitConfig::read_only().with_limits(Limits {
        max_blob_bytes: 8,
        ..Limits::default()
    });
    let result = status_with(config, &repo.mount(), "/mnt/repo", &["--json"]).await;
    let j = json(&result);
    assert_eq!(j["clean"], true, "a 9-char symlink target is not a blob read: {j}");
}

// ═══════════════════════════════════════════════════════════════════════════
// Exact-rename pairing
// ═══════════════════════════════════════════════════════════════════════════

/// A deleted symlink and an added regular file that happen to share a blob oid
/// are **not** a rename. Git's own rename detection pairs within a type; a
/// pairing that ignores class turns `ln -s hello a` plus `echo -n hello > b`
/// into a fabricated `R a -> b`.
#[tokio::test]
async fn a_deleted_symlink_does_not_pair_with_an_added_file() {
    let repo = Repo::init("repo");
    // The blob of a symlink is its target string, so this link and a regular
    // file containing "hello" hash to the same oid.
    std::os::unix::fs::symlink("hello", repo.root.join("a")).expect("symlink a");
    repo.git(&["add", "-A"]);
    repo.git(&["commit", "-m", "add link", "--quiet"]);

    std::fs::remove_file(repo.root.join("a")).expect("remove a");
    std::fs::write(repo.root.join("b"), "hello").expect("write b");
    repo.git(&["add", "-A"]);

    let result = status(&repo.mount(), "/mnt/repo", &["--json"]).await;
    let j = json(&result);
    let entries = j["entries"].as_array().expect("entries");
    assert!(
        entries.iter().all(|e| e["index"] != "renamed"),
        "a symlink and a file must not pair as a rename: {j}"
    );
    let a = entries.iter().find(|e| e["path"] == "a").expect("a entry");
    assert_eq!(a["index"], "deleted", "{a}");
    let b = entries.iter().find(|e| e["path"] == "b").expect("b entry");
    assert_eq!(b["index"], "added", "{b}");
}
