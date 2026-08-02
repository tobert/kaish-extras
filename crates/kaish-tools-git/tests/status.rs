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
// Index entries as hostile paths (architecture.md E.2)
// ═══════════════════════════════════════════════════════════════════════════

/// Build a version-2 `.git/index` carrying exactly the given paths, bypassing
/// git entirely.
///
/// Real git refuses to *write* an index entry with a `..` component, so the
/// only way to fixture one is to lay the bytes down ourselves — and a
/// repository handed to an agent was not necessarily written by real git.
/// Format: `DIRC`, version, count, then per entry 62 fixed bytes (stat fields,
/// a 20-byte oid, a flags word whose low 12 bits are the path length) followed
/// by the path, NUL-padded so the entry length is a multiple of 8. The trailer
/// is a hash over everything before it; `gix-index` only verifies it when the
/// caller supplies an expected value, which we do not, so zeros are enough.
fn write_raw_index(git_dir: &Path, paths: &[&str]) {
    let mut out = Vec::new();
    out.extend_from_slice(b"DIRC");
    out.extend_from_slice(&2u32.to_be_bytes());
    out.extend_from_slice(&(paths.len() as u32).to_be_bytes());
    for path in paths {
        let start = out.len();
        // ctime, mtime, dev, ino: zero. Nothing reads them — status hashes
        // content rather than trusting the racy-clean stat cache.
        out.extend_from_slice(&[0u8; 24]);
        // mode 0o100644, then uid, gid, size.
        out.extend_from_slice(&0o100644u32.to_be_bytes());
        out.extend_from_slice(&[0u8; 12]);
        // A zero oid: the entry is meant to be refused before it is compared.
        out.extend_from_slice(&[0u8; 20]);
        let flags = (path.len() as u16).min(0x0fff);
        out.extend_from_slice(&flags.to_be_bytes());
        out.extend_from_slice(path.as_bytes());
        let padded = (out.len() - start + 8) & !7;
        out.resize(start + padded, 0);
    }
    out.extend_from_slice(&[0u8; 20]);
    std::fs::write(git_dir.join("index"), out).expect("write raw index");
}

/// An index entry whose *intermediate directory* is a symlink out of the mount
/// must be refused (exit 4) — the escape a leaf-only `lstat` cannot see.
///
/// `symlink_metadata` does not follow the final component, but the kernel
/// resolves every component before it, so `evil/x` where `evil` is a symlink
/// lands on a host path outside the mount and `std::fs::read` returns its
/// bytes. Nothing about the final component is a symlink, so the leaf check
/// that guards `.git/index` itself never fires.
#[tokio::test]
async fn an_index_entry_under_a_symlinked_directory_is_refused() {
    require_git();
    let fixture = Fixture::empty();

    // Outside the mount: a directory holding a file at the name the index
    // tracks, with content the repository is not supposed to be able to learn.
    let outside = fixture.path("outside");
    std::fs::create_dir_all(&outside).expect("create outside");
    write_file(&outside, "x", "OUTSIDE_WORKTREE_SECRET\n");

    let mount = fixture.path("mounted");
    let repo = mount.join("repo");
    std::fs::create_dir_all(&repo).expect("create repo");
    git(&repo, &["init", "--initial-branch=main", "--quiet"]);
    write_file(&repo, "evil/x", "inside\n");
    git(&repo, &["add", "."]);
    git(&repo, &["commit", "-m", "inside", "--quiet"]);

    // Swap the tracked directory for a symlink out of the mount. The index is
    // untouched and still carries `evil/x`; only an intermediate component of
    // the path moved.
    std::fs::remove_file(repo.join("evil/x")).expect("remove tracked file");
    std::fs::remove_dir(repo.join("evil")).expect("remove tracked dir");
    std::os::unix::fs::symlink(&outside, repo.join("evil")).expect("symlink evil outside");

    let result = status(&mount, "/mnt/repo", &["--json"]).await;
    assert_eq!(
        result.code, 4,
        "an index entry reached through a symlinked directory must be refused: \
         {} {:?}",
        result.err,
        result.output()
    );
    assert!(result.err.contains("outside the mount"), "{}", result.err);
    let rendered = format!("{} {:?} {:?}", result.err, result.output(), result.baggage);
    assert!(
        !rendered.contains("OUTSIDE_WORKTREE_SECRET")
            && !rendered.contains(outside.to_str().expect("utf-8 path")),
        "the refusal must not echo where the path aimed: {rendered}"
    );
    // And the row itself is gone: a refusal that still reported `evil/x` as
    // modified would have leaked the hash comparison it was refusing to make.
    assert!(!rendered.contains("evil/x"), "{rendered}");
}

/// An index entry with a `..` component must be refused (exit 4).
///
/// `join_repo_relative` is a pure lexical join, so `../../outside/secret`
/// walks straight out of the working tree before anything is stat'ed. The
/// screen is lexical on purpose: a repository whose index carries such an
/// entry is hostile, and the honest answer to the whole operation is a
/// refusal, not a quietly skipped row.
#[tokio::test]
async fn an_index_entry_with_a_parent_component_is_refused() {
    require_git();
    let fixture = Fixture::empty();

    let outside = fixture.path("outside");
    std::fs::create_dir_all(&outside).expect("create outside");
    write_file(&outside, "secret", "OUTSIDE_INDEX_PATH_SECRET\n");

    let mount = fixture.path("mounted");
    let repo = mount.join("repo");
    std::fs::create_dir_all(&repo).expect("create repo");
    git(&repo, &["init", "--initial-branch=main", "--quiet"]);
    write_raw_index(&repo.join(".git"), &["../../outside/secret"]);

    let result = status(&mount, "/mnt/repo", &["--json"]).await;
    assert_eq!(
        result.code, 4,
        "an index entry with a '..' component must be refused: {} {:?}",
        result.err,
        result.output()
    );
    assert!(result.err.contains("outside the mount"), "{}", result.err);
    let rendered = format!("{} {:?} {:?}", result.err, result.output(), result.baggage);
    assert!(
        !rendered.contains("OUTSIDE_INDEX_PATH_SECRET")
            && !rendered.contains("secret")
            && !rendered.contains(".."),
        "the refusal must not echo the escaping entry: {rendered}"
    );
}

/// An absolute index entry is refused by the same screen.
///
/// What it costs to *not* screen it is platform-shaped, which is why the
/// screen is lexical rather than a bet on one platform's join. On unix
/// `join_repo_relative` splits on `/` and pushes ordinary names, so
/// `/etc/hostname` lands under the working tree and status reports a row for
/// a path that is not in the working tree at all — a lie about the tree, and
/// an existence probe for `<worktree>/etc/hostname`. On Windows the same push
/// of a rooted component replaces the base outright, and the read leaves the
/// mount for real.
#[tokio::test]
async fn an_absolute_index_entry_is_refused() {
    require_git();
    let fixture = Fixture::empty();
    let mount = fixture.path("mounted");
    let repo = mount.join("repo");
    std::fs::create_dir_all(&repo).expect("create repo");
    git(&repo, &["init", "--initial-branch=main", "--quiet"]);
    write_raw_index(&repo.join(".git"), &["/etc/hostname"]);

    let result = status(&mount, "/mnt/repo", &["--json"]).await;
    assert_eq!(
        result.code, 4,
        "an absolute index entry must be refused: {} {:?}",
        result.err,
        result.output()
    );
    let rendered = format!("{} {:?} {:?}", result.err, result.output(), result.baggage);
    assert!(
        !rendered.contains("hostname"),
        "the refusal must not echo the escaping entry: {rendered}"
    );
}

/// Over-refusal guard for the containment screen: a symlinked *leaf* is a
/// legitimate tracked object (git stores the link target as the blob), and a
/// deep in-tree path with an ordinary directory chain must still be read.
#[tokio::test]
async fn deep_paths_and_symlink_leaves_still_work() {
    let repo = Repo::init("repo");
    repo.write("a/b/c/deep.txt", "deep content\n");
    std::os::unix::fs::symlink("a/b/c/deep.txt", repo.root.join("link"))
        .expect("symlink leaf");
    repo.git(&["add", "-A"]);
    repo.git(&["commit", "-m", "deep", "--quiet"]);

    let result = status(&repo.mount(), "/mnt/repo", &["--json"]).await;
    let j = json(&result);
    assert_eq!(j["clean"], true, "a committed deep tree must be clean: {j}");

    // And the leaf symlink is compared by its target, not by following it: a
    // retarget is a modification, never a read of the new target.
    std::fs::remove_file(repo.root.join("link")).expect("remove link");
    std::os::unix::fs::symlink("a/b/c/other.txt", repo.root.join("link"))
        .expect("retarget link");
    let result = status(&repo.mount(), "/mnt/repo", &["--json"]).await;
    let j = json(&result);
    let link = j["entries"]
        .as_array()
        .expect("entries")
        .iter()
        .find(|e| e["path"] == "link")
        .unwrap_or_else(|| panic!("expected a row for link: {j}"));
    assert_eq!(link["worktree"], "modified", "{link}");
}

// ───────────────────────────────────────────────────────────────────────────
// The refusal must not be an existence oracle for the host
// ───────────────────────────────────────────────────────────────────────────

/// What the caller can observe about one run: the exit code, and everything
/// rendered anywhere. Both halves have to match across the probe pair.
struct Observable {
    code: i64,
    rendered: String,
}

/// Track `probe/x` in the index, aim the worktree's `probe` at a path outside
/// the mount, and run status.
///
/// `target_exists` is the only difference between the two calls: it decides
/// whether the symlink's target is a real directory on the host. That bit is
/// the repository's to *ask about* and never ours to answer, so both calls must
/// come back observably identical.
async fn symlinked_parent_probe(target_exists: bool) -> (Observable, String) {
    require_git();
    let fixture = Fixture::empty();

    let outside = fixture.path("outside");
    if target_exists {
        std::fs::create_dir_all(&outside).expect("create outside");
        write_file(&outside, "x", "OUTSIDE_ORACLE_SECRET\n");
    }

    let mount = fixture.path("mounted");
    let repo = mount.join("repo");
    std::fs::create_dir_all(&repo).expect("create repo");
    git(&repo, &["init", "--initial-branch=main", "--quiet"]);
    // Hand-written, because the point is an index whose entry survives the
    // worktree being rearranged underneath it.
    write_raw_index(&repo.join(".git"), &["probe/x"]);
    std::os::unix::fs::symlink(&outside, repo.join("probe")).expect("symlink probe outside");

    let result = status(&mount, "/mnt/repo", &["--json"]).await;
    let observable = Observable {
        code: result.code,
        rendered: format!("{} {:?} {:?}", result.err, result.output(), result.baggage),
    };
    (observable, outside.to_str().expect("utf-8 path").to_string())
}

/// A worktree symlink out of the mount is refused the same way whether or not
/// its target exists on the host.
///
/// The oracle this closes: a whole-chain `canonicalize` fails with `NotFound`
/// on a dangling symlink and succeeds on a live one, and the two answers used
/// to diverge — exit 4 for "the target is there", exit 0 with the entry
/// reported deleted for "it is not". The repository chooses the symlink target,
/// so that difference is a one-bit read of any host path it names, driven
/// entirely by repository content. `repo.rs`'s `contain` already makes
/// "resolves outside" and "does not resolve" indistinguishable; this is the
/// same rule for the working tree.
#[tokio::test]
async fn a_symlinked_parent_out_of_the_mount_cannot_report_whether_its_target_exists() {
    let (present, outside_present) = symlinked_parent_probe(true).await;
    let (absent, outside_absent) = symlinked_parent_probe(false).await;

    assert_eq!(
        present.code, absent.code,
        "the exit code told the repository whether the symlink target exists: \
         present={} absent={}\npresent: {}\nabsent: {}",
        present.code, absent.code, present.rendered, absent.rendered
    );
    assert_eq!(
        present.code, 4,
        "a worktree symlink out of the mount is a refusal: {}",
        present.rendered
    );

    for (observable, outside) in [(&present, &outside_present), (&absent, &outside_absent)] {
        assert!(
            observable.rendered.contains("outside the mount"),
            "{}",
            observable.rendered
        );
        assert!(
            !observable.rendered.contains("OUTSIDE_ORACLE_SECRET")
                && !observable.rendered.contains(outside.as_str())
                && !observable.rendered.contains("probe/x"),
            "the refusal must not echo where the path aimed: {}",
            observable.rendered
        );
    }
}

/// Over-refusal guard (a): a tracked file deleted from a directory that is
/// still there is reported deleted, not refused. No symlink is involved, so
/// nothing about it is a containment question.
#[tokio::test]
async fn a_deleted_tracked_file_under_a_live_directory_is_reported_deleted() {
    let repo = Repo::init("repo");
    repo.write("dir/kept.txt", "kept\n");
    repo.write("dir/gone.txt", "gone\n");
    repo.git(&["add", "."]);
    repo.git(&["commit", "-m", "two files", "--quiet"]);
    std::fs::remove_file(repo.root.join("dir/gone.txt")).expect("remove tracked file");

    let result = status(&repo.mount(), "/mnt/repo", &["--json"]).await;
    let j = json(&result);
    let gone = j["entries"]
        .as_array()
        .expect("entries")
        .iter()
        .find(|e| e["path"] == "dir/gone.txt")
        .unwrap_or_else(|| panic!("expected a row for dir/gone.txt: {j}"));
    assert_eq!(gone["worktree"], "deleted", "{gone}");
}

/// Over-refusal guard (b): a tracked file whose whole parent directory was
/// removed is reported deleted too. The chain is honestly absent — that is a
/// fact about the working tree, not about the host.
#[tokio::test]
async fn a_deleted_parent_directory_reports_its_entries_deleted() {
    let repo = Repo::init("repo");
    repo.write("dir/a.txt", "a\n");
    repo.write("dir/b.txt", "b\n");
    repo.git(&["add", "."]);
    repo.git(&["commit", "-m", "a dir", "--quiet"]);
    std::fs::remove_dir_all(repo.root.join("dir")).expect("remove tracked dir");

    let result = status(&repo.mount(), "/mnt/repo", &["--json"]).await;
    let j = json(&result);
    let entries = j["entries"].as_array().expect("entries");
    for path in ["dir/a.txt", "dir/b.txt"] {
        let row = entries
            .iter()
            .find(|e| e["path"] == path)
            .unwrap_or_else(|| panic!("expected a row for {path}: {j}"));
        assert_eq!(row["worktree"], "deleted", "{row}");
    }
}

/// Over-refusal guard (c): a symlinked directory that stays *inside* the
/// working tree is legitimate and must still be followed. A component walk that
/// refused every symlink would break this checkout.
#[tokio::test]
async fn an_internal_symlinked_directory_is_followed() {
    let repo = Repo::init("repo");
    repo.write("real/x", "content\n");
    std::os::unix::fs::symlink("real", repo.root.join("link")).expect("symlink link -> real");
    repo.git(&["add", "-A"]);
    repo.git(&["commit", "-m", "link and real", "--quiet"]);

    // Rewrite the index so `link/x` is a tracked path in its own right —
    // git stores `link` as a symlink blob and never descends it, so this is the
    // shape only a hand-written index produces.
    write_raw_index(&repo.root.join(".git"), &["link/x"]);

    let result = status(&repo.mount(), "/mnt/repo", &["--json"]).await;
    assert_eq!(
        result.code, 0,
        "a symlink inside the working tree is not an escape: {} {:?}",
        result.err,
        result.output()
    );
    let j = json(&result);
    let row = j["entries"]
        .as_array()
        .expect("entries")
        .iter()
        .find(|e| e["path"] == "link/x")
        .unwrap_or_else(|| panic!("expected a row for link/x: {j}"));
    // The zero oid in the raw index never matches real content, so the
    // interesting assertion is that the file was *read and compared* at all.
    assert_eq!(row["worktree"], "modified", "{row}");
}

/// The leaf half of the same question: a tracked *symlink* whose target does
/// not exist behaves exactly like one whose target does.
///
/// `read_worktree_blob` uses `read_link`, which returns the target string
/// without resolving it, so there is nothing here to leak — this fixture is
/// what keeps that true if the leaf ever grows a resolution step.
#[tokio::test]
async fn a_symlinked_leaf_reads_the_same_whether_its_target_exists() {
    async fn probe(target: &str) -> (i64, String) {
        let repo = Repo::init("repo");
        std::os::unix::fs::symlink(target, repo.root.join("link")).expect("symlink leaf");
        repo.git(&["add", "-A"]);
        repo.git(&["commit", "-m", "link", "--quiet"]);
        // Retarget to the same length so only the target's existence differs.
        std::fs::remove_file(repo.root.join("link")).expect("remove link");
        std::os::unix::fs::symlink(target, repo.root.join("link")).expect("relink");

        let result = status(&repo.mount(), "/mnt/repo", &["--json"]).await;
        (
            result.code,
            format!("{} {:?}", result.err, result.output()),
        )
    }

    // Same length, one absolute path that exists on every unix host and one
    // that exists nowhere.
    let (present, present_rendered) = probe("/etc/hostname").await;
    let (absent, absent_rendered) = probe("/etc/nosuchxy").await;
    assert_eq!(
        present, absent,
        "a symlinked leaf reported its target's existence: \
         present={present} absent={absent}\n{present_rendered}\n{absent_rendered}"
    );
    assert_eq!(present, 0, "both are clean checkouts: {present_rendered}");
}

// ───────────────────────────────────────────────────────────────────────────
// The untracked walk never enters a git directory
// ───────────────────────────────────────────────────────────────────────────

/// The walk skips a git directory whatever case it is spelled in.
///
/// On a case-insensitive filesystem — macOS and Windows, where a great many
/// checkouts live — `.GIT` *is* the git directory, and an exact `== ".git"`
/// compare walks straight into `objects/`, `refs/` and `config` and reports
/// them as untracked. The compare is case-insensitive so the rule holds on
/// every platform rather than on the one the test happens to run on; the cost
/// is that a case-sensitive filesystem's unrelated `.GIT` directory is skipped
/// too, which is the safe direction and the one git takes under
/// `core.ignorecase`.
#[tokio::test]
async fn the_walk_does_not_descend_a_case_folded_git_dir() {
    let repo = Repo::init("repo");
    repo.write("tracked.txt", "t\n");
    repo.git(&["add", "."]);
    repo.git(&["commit", "-m", "one file", "--quiet"]);
    repo.write(".GIT/objects/decoy", "not yours\n");
    repo.write(".GIT/config", "[core]\n");

    let j = json(&status(&repo.mount(), "/mnt/repo", &["--untracked", "all", "--json"]).await);
    let paths: BTreeSet<String> = j["entries"]
        .as_array()
        .expect("entries")
        .iter()
        .map(|e| e["path"].as_str().expect("path").to_string())
        .collect();
    assert!(
        paths.iter().all(|p| !p.starts_with(".GIT")),
        "the walk descended a case-folded git dir: {paths:?}"
    );
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
