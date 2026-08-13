//! The `.git` fingerprint test — architecture.md D.4, layer 4 of the
//! read-only argument.
//!
//! The flagship claim is "true read-only git, which command-line git cannot
//! offer". This is the test that can falsify it: build a fixture repository
//! with packed objects, multiple branches, a dirty working tree and a linked
//! worktree; take a recursive fingerprint of `.git` (every path, its size, its
//! mtime, its content hash); run every read verb across a representative flag
//! matrix; take the fingerprint again; assert byte-identical, mtime-identical,
//! and no new paths.
//!
//! What it catches is the whole class the design intent names: index
//! stat-cache refreshes, reflog appends, `gc --auto`, pack-index rebuilds,
//! `commit-graph` writes. Real git does several of those on a plain
//! `git status` — `real_git_status_writes_to_dot_git` keeps that contrast
//! honest.

#[path = "support.rs"]
mod support;

use std::path::{Path, PathBuf};
use std::sync::Arc;

use kaish_tool_api::Tool;
use kaish_types::{ExecResult, ToolArgs, Value};

use kaish_tools_git::{GitConfig, Verb};

use support::{Fingerprint, RichRepo, StrictBackend, TestCtx};

/// The VFS root the fixture scratch directory is mounted at.
const MOUNT: &str = "/mnt";

/// Every read verb, and the invocations the fingerprint runs it under.
///
/// Keyed by the verb's argv word so it can be cross-checked against
/// `Verb::ALL` — see `every_verb_has_fingerprint_coverage`. Each entry is a
/// full argv *after* the verb word.
///
/// Architecture.md D.4 asks for a table where a new verb that skips the
/// fingerprint is a compile error. `Verb` is `#[non_exhaustive]` (C.1) and an
/// integration test is a separate crate, so an exhaustive `match` on it does
/// not compile; the closest honest equivalent is the length-and-name check
/// below, which fails loudly the moment a verb is added without coverage.
const VERB_MATRIX: &[(&str, &[&[&str]])] = &[
    (
        "info",
        &[
            // Bare, from the repository root: the default everything else
            // varies from.
            &[],
            // Structured output, which walks a different rendering path.
            &["--json"],
            // Upward discovery from a subdirectory — the capability v2 added,
            // and the one the ceiling exists to bound.
            &["--repo", "/mnt/main/src"],
            // An explicit repository selector naming the root.
            &["--repo", "/mnt/main"],
            // A linked worktree, whose git dir is private and whose common dir
            // is the main repository's.
            &["--repo", "/mnt/wt-side"],
            // Both at once, since `--json` and `--repo` bind independently.
            &["--repo", "/mnt/wt-side", "--json"],
        ],
    ),
    (
        "status",
        &[
            // Bare, over a dirty tree (staged, unstaged, untracked all present
            // in the fixture): the index read, the worktree walk, the tree↔
            // index compare — and none of it may touch `.git`. This is the case
            // that catches a persisted stat-cache refresh (D.4).
            &[],
            // Structured output: the words path, and a different renderer.
            &["--json"],
            // The full untracked walk, which recurses every untracked dir.
            &["--untracked", "all", "--json"],
            // No untracked, and ignored included — two more walk modes.
            &["--untracked", "no"],
            &["--ignored", "--json"],
            // A path filter and a hard limit, the two truncation-adjacent paths.
            &["--path", "src", "--json"],
            &["--limit", "1"],
            // A linked worktree has its own index at its private git dir.
            &["--repo", "/mnt/wt-side", "--json"],
        ],
    ),
    (
        "log",
        &[
            // Bare: the commit walk and the object reads under it. The fixture
            // is packed, so this exercises the pack path — a pack-index rebuild
            // would be exactly the kind of `.git` write D.4 exists to catch.
            &[],
            // Structured output: a different renderer over the same model.
            &["--json"],
            // `--stat` reads blob pairs to count lines, the heaviest object
            // traffic this verb generates.
            &["--stat", "--json"],
            // Message bodies, which read the same objects a different way.
            &["--body", "--json"],
            // A revision other than HEAD, resolved through the ref store —
            // where a naive peel could rewrite `packed-refs`.
            &["--rev", "feature/side"],
            // Tag resolution, which peels an annotated object.
            &["--rev", "v0.1.0", "--json"],
            // Navigation suffixes, which read parents.
            &["--rev", "HEAD~1"],
            // The filters, each of which walks trees or signatures.
            &["--path", "src", "--json"],
            &["--author", "Fixture", "--json"],
            &["--since", "2020-01-01", "--json"],
            &["--no-merges", "--json"],
            &["--first-parent", "--json"],
            // Truncation, which stops the walk mid-flight.
            &["--limit", "1"],
            // A linked worktree resolves refs through the common dir.
            &["--repo", "/mnt/wt-side", "--json"],
        ],
    ),
];

/// Split an argv slice into the `ToolArgs` the kernel would have built.
///
/// Mirrors the kernel's binding closely enough for the verbs shipped so far:
/// `--flag value` becomes a named argument, a lone `--flag` becomes a flag,
/// and the verb word leads the positionals.
fn tool_args(verb: &str, argv: &[&str]) -> ToolArgs {
    let mut args = ToolArgs::new();
    args.positional.push(Value::String(verb.to_string()));
    let mut i = 0;
    while i < argv.len() {
        let token = argv[i]
            .strip_prefix("--")
            .unwrap_or_else(|| panic!("matrix argv must use long flags: {}", argv[i]));
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

/// Run one invocation against the fixture, returning the result.
async fn run(repo: &RichRepo, cwd: &str, verb: &str, argv: &[&str]) -> ExecResult {
    let backend = Arc::new(StrictBackend::single(PathBuf::from(MOUNT), repo.scratch()));
    let mut ctx = TestCtx::new(backend, cwd);
    let git = kaish_tools_git::tool(GitConfig::read_only()).expect("read-only config builds");
    git.execute(tool_args(verb, argv), &mut ctx).await
}

/// Every invocation in the matrix, plus each verb run from a subdirectory cwd
/// (which exercises `resolve_path` rather than `--repo`).
async fn run_the_whole_matrix(repo: &RichRepo) {
    for (verb, matrix) in VERB_MATRIX {
        for argv in *matrix {
            let result = run(repo, "/mnt/main", verb, argv).await;
            assert_eq!(result.code, 0, "git {verb} {argv:?} failed: {}", result.err);
        }
        let result = run(repo, "/mnt/main/src", verb, &[]).await;
        assert_eq!(
            result.code, 0,
            "git {verb} from a subdirectory failed: {}",
            result.err
        );
    }
}

fn assert_unchanged(before: &Fingerprint, after: &Fingerprint, what: &str) {
    let diffs = after.differences_from(before);
    assert!(
        diffs.is_empty(),
        "{what} changed after running read verbs — kaish-tools-git wrote to a \
         repository it opened read-only:\n  {}",
        diffs.join("\n  ")
    );
}

/// D.4 itself: the whole read surface, then nothing moved.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn read_verbs_do_not_touch_dot_git() {
    let repo = RichRepo::build();
    let git_dir = repo.git_dir();

    let before = Fingerprint::take(&git_dir);
    assert!(
        before.entries.len() > 10,
        "the fixture's .git has only {} entries — it is not rich enough to \
         prove anything",
        before.entries.len()
    );

    run_the_whole_matrix(&repo).await;

    let after = Fingerprint::take(&git_dir);
    assert_unchanged(&before, &after, ".git");
}

/// The linked worktree's private git dir lives under the main `.git`, but its
/// `.git` *file* and working tree are separate — a second place a
/// worktree-aware verb could write.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn read_verbs_do_not_touch_the_linked_worktree() {
    let repo = RichRepo::build();
    let before = Fingerprint::take(&repo.linked_worktree);

    run_the_whole_matrix(&repo).await;

    let after = Fingerprint::take(&repo.linked_worktree);
    assert_unchanged(&before, &after, "the linked worktree");
}

/// The same surface on a current-thread runtime, which is what an embedder
/// like kaish-web runs. `block_in_place` panics there; the compat helper is
/// what keeps this green, and this asserts it stays that way.
#[tokio::test]
async fn read_verbs_are_read_only_on_a_current_thread_runtime() {
    let repo = RichRepo::build();
    let git_dir = repo.git_dir();
    let before = Fingerprint::take(&git_dir);

    run_the_whole_matrix(&repo).await;

    let after = Fingerprint::take(&git_dir);
    assert_unchanged(&before, &after, ".git");
}

/// The fingerprint must be able to fail, or the tests above prove nothing.
///
/// Each of the three signals is exercised separately: a new path, a content
/// change, and an mtime-only change. Directory mtimes are recorded too, but
/// they are not asserted here — whether a create-then-remove inside one
/// timestamp tick leaves a trace is the filesystem's granularity to decide,
/// and a test that depends on it would be flaky rather than strict.
#[test]
fn the_fingerprint_detects_a_write() {
    let repo = RichRepo::build();
    let git_dir = repo.git_dir();
    let before = Fingerprint::take(&git_dir);

    // 1. A new path.
    let sentinel = git_dir.join("kaish-git-test-sentinel");
    std::fs::write(&sentinel, b"a write happened").expect("write sentinel");
    let diffs = Fingerprint::take(&git_dir).differences_from(&before);
    assert!(
        diffs.iter().any(|d| d.contains("NEW path")),
        "a new file under .git must be reported: {diffs:?}"
    );
    std::fs::remove_file(&sentinel).expect("remove sentinel");

    // 2. Changed content in a file that was already there.
    let head = git_dir.join("HEAD");
    let original = std::fs::read(&head).expect("read HEAD");
    std::fs::write(&head, b"ref: refs/heads/tampered\n").expect("tamper with HEAD");
    let diffs = Fingerprint::take(&git_dir).differences_from(&before);
    assert!(
        diffs.iter().any(|d| d.contains("content changed")),
        "a changed file must be reported: {diffs:?}"
    );
    std::fs::write(&head, &original).expect("restore HEAD");

    // 3. An mtime move with the content left alone — the class that catches
    //    an index stat-cache refresh writing identical bytes back.
    let file = std::fs::File::options()
        .write(true)
        .open(&head)
        .expect("open HEAD");
    let bumped = std::time::SystemTime::now() + std::time::Duration::from_secs(120);
    file.set_times(std::fs::FileTimes::new().set_modified(bumped))
        .expect("bump HEAD's mtime");
    drop(file);
    let diffs = Fingerprint::take(&git_dir).differences_from(&before);
    assert!(
        diffs.iter().any(|d| d.contains("mtime changed")),
        "an mtime move with unchanged content must be reported: {diffs:?}"
    );
}

/// The oracle side: real git, doing an ordinary read, writes. If this ever
/// stops being true the fingerprint has lost its teeth and D.4's claim
/// ("which command-line git cannot offer") needs re-checking.
#[test]
fn real_git_status_writes_to_dot_git() {
    let repo = RichRepo::build();
    let git_dir = repo.git_dir();
    // Prime the index's stat cache first, so what follows is git's own
    // bookkeeping rather than the fixture settling.
    support::git(&repo.root, &["status", "--porcelain"]);

    // Move a clean tracked file's mtime without touching its bytes. `README.md`
    // was `git add`ed in `build`, so its index entry and worktree content
    // agree; only the stat is now stale. That is exactly the racy-clean case
    // git resolves by re-hashing, finding the content unchanged, and rewriting
    // the index with a refreshed stat — a `.git` write on an ordinary `status`,
    // and one git performs regardless of version or mtime granularity. Without
    // it the test rests on git *choosing* to rewrite an already-warm index,
    // which some hosts (coarse mtime, other git builds) skip — a flake, not a
    // loss of the property D.4 relies on.
    let readme = repo.root.join("README.md");
    let bumped = std::time::SystemTime::now() + std::time::Duration::from_secs(120);
    let file = std::fs::File::options()
        .write(true)
        .open(&readme)
        .expect("open README.md");
    file.set_times(std::fs::FileTimes::new().set_modified(bumped))
        .expect("bump README.md's mtime");
    drop(file);

    let before = Fingerprint::take(&git_dir);
    support::git(&repo.root, &["status"]);
    let after = Fingerprint::take(&git_dir);

    let diffs = after.differences_from(&before);
    assert!(
        !diffs.is_empty(),
        "real `git status` left .git byte-identical — the fingerprint may be \
         too coarse to prove anything about our own reads"
    );
}

/// Coverage bookkeeping: every verb the config can enable must appear in
/// [`VERB_MATRIX`], or the fingerprint silently stops covering the surface it
/// claims to.
#[test]
fn every_verb_has_fingerprint_coverage() {
    let covered: Vec<&str> = VERB_MATRIX.iter().map(|(name, _)| *name).collect();
    for verb in Verb::ALL {
        assert!(
            covered.contains(&verb.as_str()),
            "verb '{}' has no entry in VERB_MATRIX — the .git fingerprint \
             test would not cover it (architecture.md D.4)",
            verb.as_str()
        );
    }
    assert_eq!(
        covered.len(),
        Verb::ALL.len(),
        "VERB_MATRIX covers {covered:?} but the config offers {:?}",
        Verb::ALL.iter().map(Verb::as_str).collect::<Vec<_>>()
    );
    for (name, matrix) in VERB_MATRIX {
        assert!(
            !matrix.is_empty(),
            "verb '{name}' has an empty flag matrix, which runs nothing"
        );
    }
}

/// `git info` must report what real git reports, or the fingerprint could be
/// green over a verb that answers wrongly.
#[tokio::test]
async fn info_agrees_with_real_git() {
    let repo = RichRepo::build();
    let result = run(&repo, "/mnt/main", "info", &["--json"]).await;
    assert_eq!(result.code, 0, "git info failed: {}", result.err);

    let json = result
        .output()
        .and_then(|o| o.rich_json.clone())
        .expect("--json output carries the typed model");

    assert_eq!(
        json["head"]["oid"].as_str().expect("head oid"),
        repo.rev_parse("HEAD"),
        "head oid must match real git"
    );
    assert_eq!(json["head"]["branch"], "main");
    assert_eq!(json["head"]["detached"], false);
    assert_eq!(json["bare"], false);
    assert_eq!(json["shallow"], false);
    assert_eq!(json["ref_backend"], "files");
    assert_eq!(
        json["repo_root_real"].as_str().map(Path::new),
        Some(repo.root.as_path())
    );
    assert_eq!(json["repo_root_vfs"], "/mnt/main");
    assert_eq!(
        json["git_dir"].as_str().map(PathBuf::from),
        Some(repo.git_dir())
    );
    assert_eq!(
        json["worktrees"], 2,
        "the main working tree plus one linked worktree"
    );
    assert_eq!(json["submodules"], 0);
    assert_eq!(json["capabilities"]["profiles"][0], "read");
    assert_eq!(json["capabilities"]["verbs"][0], "info");
    assert_eq!(json["capabilities"]["limits"]["max_rows"], 1000);
    assert!(
        json["gix_pins"]["gix-object"].is_string(),
        "gix_pins must name the plumbing crates: {json}"
    );
}

/// Inside the linked worktree, `git info` must report the worktree's private
/// git dir and its own root — not the main repository's.
#[tokio::test]
async fn info_inside_a_linked_worktree_reports_the_worktree() {
    let repo = RichRepo::build();
    let result = run(&repo, "/mnt/wt-side", "info", &["--json"]).await;
    assert_eq!(result.code, 0, "git info failed: {}", result.err);

    let json = result
        .output()
        .and_then(|o| o.rich_json.clone())
        .expect("--json output carries the typed model");

    assert_eq!(
        json["repo_root_real"].as_str().map(Path::new),
        Some(repo.linked_worktree.as_path())
    );
    assert_eq!(
        json["git_dir"].as_str().map(PathBuf::from),
        Some(repo.git_dir().join("worktrees").join(&repo.linked_name)),
        "a linked worktree's git dir is private to it"
    );
    assert_eq!(json["head"]["branch"], "feature/side");
    assert_eq!(
        json["worktrees"], 2,
        "worktree count is a property of the repository, not of which \
         worktree you asked from"
    );
}

/// `--json` must reach the kernel as an output-format request, not merely be
/// swallowed by our clap parse.
#[tokio::test]
async fn json_flag_sets_the_output_format() {
    let repo = RichRepo::build();
    let backend = Arc::new(StrictBackend::single(PathBuf::from(MOUNT), repo.scratch()));
    let mut ctx = TestCtx::new(backend, "/mnt/main");
    let git = kaish_tools_git::tool(GitConfig::read_only()).expect("config");

    git.execute(tool_args("info", &[]), &mut ctx).await;
    assert!(
        ctx.output_format.is_none(),
        "no --json means no format override"
    );

    git.execute(tool_args("info", &["--json"]), &mut ctx).await;
    assert_eq!(
        ctx.output_format,
        Some(kaish_types::OutputFormat::Json),
        "--json must be applied to the context"
    );
}

/// E.4: the result carries enough baggage for an embedder's trace to
/// correlate the call with a repository state.
#[tokio::test]
async fn results_carry_repository_baggage() {
    let repo = RichRepo::build();
    let result = run(&repo, "/mnt/main", "info", &[]).await;
    assert_eq!(
        result.baggage.get("git.repo").map(String::as_str),
        repo.root.to_str()
    );
    assert_eq!(
        result.baggage.get("git.head_oid"),
        Some(&repo.rev_parse("HEAD"))
    );
}
