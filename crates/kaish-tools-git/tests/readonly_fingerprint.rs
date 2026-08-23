//! The `.git` fingerprint test — architecture.md D.4, layer 4 of the
//! read-only argument.
//!
//! The flagship claim is "true read-only git, which command-line git cannot
//! offer". This is the test that can falsify it: build a fixture repository
//! with packed objects, multiple branches, a dirty working tree and a linked
//! worktree; take a recursive fingerprint of the fixture's **scratch root**
//! (every path under it, its size, its mtime, its content hash); run every
//! read verb across a representative flag matrix; take the fingerprint
//! again; assert byte-identical, mtime-identical, and no new paths.
//!
//! What it catches is the whole class the design intent names: index
//! stat-cache refreshes, reflog appends, `gc --auto`, pack-index rebuilds,
//! `commit-graph` writes. Real git does several of those on a plain
//! `git status` — `real_git_status_writes_to_dot_git` keeps that contrast
//! honest.
//!
//! **Why the scratch root, not just `.git` (docs/issues.md P8).** The
//! fingerprint used to sample `.git` and the linked worktree separately, and
//! nothing else. That misses two classes of write by construction, not by
//! bad luck: a write to a file in the *main* working tree (nothing sampled
//! it), and a stray file created directly under the scratch root, outside
//! every worktree and outside `.git` (nothing sampled that either, since it
//! is not under any of the roots the two separate fingerprints took).
//! `RichRepo::scratch()` is the parent directory of both the main working
//! tree and the linked worktree, so one fingerprint of it subsumes both of
//! the old ones and closes both blind spots at once —
//! `the_scratch_root_fingerprint_catches_what_dot_git_alone_cannot` proves
//! the blind spots were real before this file closed them, by taking both
//! fingerprints across the same write and showing the narrower one reports
//! nothing.

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
    (
        "ls",
        &[
            // Bare, from the repository root: the tree walk over HEAD's root.
            &[],
            // Structured output, a different rendering path.
            &["--json"],
            // A bare positional revision and a subdirectory path — position,
            // not content, decides which operand is which.
            &["HEAD", "src", "--json"],
            // Recursive expansion, which omits directory rows and descends.
            &["HEAD", "src", "--recursive", "--json"],
            // A path naming a single file — the one-row, non-tree case.
            &["HEAD", "README.md", "--json"],
            // A tag revision, peeled to its commit's tree.
            &["v0.1.0", "--json"],
            // A hard limit, the truncation-adjacent path.
            &["--limit", "1"],
            // A linked worktree has its own HEAD and tree to walk.
            &["--repo", "/mnt/wt-side", "--json"],
        ],
    ),
    (
        "show",
        &[
            // Bare: HEAD's own commit metadata.
            &[],
            &["--json"],
            // The flagship spelling — a blob at a revision, no checkout.
            &["HEAD:src/lib.rs"],
            &["HEAD:src/lib.rs", "--json"],
            // A tree form, the same row shape `ls` reports.
            &["HEAD:src", "--json"],
            // An annotated tag — its own metadata, then the tagged commit.
            &["v0.1.0", "--json"],
            // A hard limit on the tree form.
            &["HEAD:", "--limit", "1", "--json"],
            // A linked worktree resolves its own HEAD.
            &["--repo", "/mnt/wt-side", "--json"],
        ],
    ),
    (
        "diff",
        &[
            // Bare, over the fixture's dirty tree: the index read plus a
            // worktree hash of every tracked file — the same traffic
            // `status`'s unstaged half generates, and the case that catches a
            // persisted stat-cache refresh (D.4).
            &[],
            &["--json"],
            // HEAD↔index: the F.4 endpoint, a tree flatten against the index.
            &["--staged", "--json"],
            // A revision against the working tree, and two revisions against
            // each other — the object-only path, which reads packs.
            &["--from", "HEAD~1", "--json"],
            &["--from", "HEAD~1", "--to", "HEAD", "--json"],
            &["--to", "HEAD~1"],
            // Rename pairing on and off, which walks the same maps twice.
            &["--staged", "--no-find-renames", "--json"],
            // Paths-only, which reads no blob at all.
            &["--name-only", "--json"],
            // A path filter and a hard limit, the two truncation-adjacent
            // paths.
            &["--path", "src", "--json"],
            &["--limit", "1"],
            // A linked worktree has its own index at its private git dir.
            &["--repo", "/mnt/wt-side", "--json"],
        ],
    ),
    (
        "branch",
        &[
            // The plain listing: refs read, no commit decoded.
            &[],
            &["--json"],
            // Both other namespaces, which read `refs/remotes/` as well.
            &["--remote", "--json"],
            &["--all", "--json"],
            // The two filters, each of which walks history — the heaviest
            // object traffic this verb generates, and where a naive peel
            // could rewrite `packed-refs`.
            &["--contains", "HEAD~1", "--json"],
            &["--merged", "HEAD", "--json"],
            // The counts, which walk both sides of every reported branch.
            &["--ahead-behind", "--json"],
            // Truncation, and the counts under it.
            &["--limit", "1"],
            &["--ahead-behind", "--limit", "1", "--json"],
            // A linked worktree resolves refs through the common dir.
            &["--repo", "/mnt/wt-side", "--json"],
        ],
    ),
    (
        "tag",
        &[
            &[],
            &["--json"],
            // Peeling an annotated tag reads the tag object.
            &["--contains", "HEAD~1", "--json"],
            &["--limit", "1"],
            &["--repo", "/mnt/wt-side", "--json"],
        ],
    ),
    (
        "worktree list",
        &[
            // Reads every registration under `<common>/worktrees/`, plus each
            // one's private HEAD — a second place a worktree-aware verb could
            // write.
            &[],
            &["--json"],
            &["--limit", "1"],
            // From inside the linked worktree, which resolves the same set
            // through a private git dir.
            &["--repo", "/mnt/wt-side", "--json"],
        ],
    ),
];

/// The invocations that exist only under the `textdiff` feature.
///
/// Kept out of [`VERB_MATRIX`] rather than `cfg`-gated inside it: the matrix
/// is a `const` the coverage guard below also reads, and a feature-shaped
/// hole in it would make that guard's verb list depend on the build. Reading
/// blob content and rendering it is a new read path, so the fingerprint has
/// to cover it or D.4's "every read verb" is no longer true of this build.
#[cfg(feature = "textdiff")]
const TEXTDIFF_MATRIX: &[(&str, &[&[&str]])] = &[(
    "diff",
    &[
        // Hunks over the working tree, over the index, and over two
        // revisions — the three content-reading endpoint shapes.
        &["--patch"],
        &["--patch", "--json"],
        &["--patch", "--staged", "--json"],
        &["--patch", "--from", "HEAD~1", "--to", "HEAD", "--json"],
        // A context width that merges hunks, and one that removes context
        // entirely.
        &["--patch", "--context", "0", "--json"],
        &["--patch", "--context", "40", "--json"],
    ],
)];

/// Flags whose next argv token is the flag's *value*, never a bare
/// positional. `ls` and `show`'s flagship spellings are bare positionals
/// (`show HEAD:src/lib.rs`, `ls HEAD src`), so this harness can no longer
/// assume every non-`--` token is a value — it has no `ParamSchema` to
/// consult the way the kernel's real binder does, so the value-taking flags
/// are named here explicitly instead.
///
/// Adding a new value-taking flag anywhere in [`VERB_MATRIX`] means adding
/// its name here too, or `tool_args` panics rather than silently mis-bind it
/// as a bare positional.
const VALUE_FLAGS: &[&str] = &[
    "repo", "rev", "limit", "path", "since", "until", "author", "untracked", "from", "to",
    "context", "contains", "merged",
];

/// Flags that never take a value — a bare `--flag`. Every long flag used
/// anywhere in [`VERB_MATRIX`] must be classified in exactly one of this list
/// or [`VALUE_FLAGS`].
const BOOL_FLAGS: &[&str] = &[
    "json",
    "patch",
    "ignored",
    "merges",
    "no-merges",
    "first-parent",
    "body",
    "stat",
    "patch",
    "recursive",
    "staged",
    "name-only",
    "find-renames",
    "no-find-renames",
    "all",
    "remote",
    "ahead-behind",
];

/// Split an argv slice into the `ToolArgs` the kernel would have built.
///
/// Mirrors the kernel's binding closely enough for the verbs shipped so far:
/// `--flag value` becomes a named argument when `flag` is in
/// [`VALUE_FLAGS`], a lone `--flag` becomes a flag when it is in
/// [`BOOL_FLAGS`], and a `--flag` in neither list is a fail-loud bug in the
/// matrix rather than a guess. Any token that is not itself a flag (and was
/// not just consumed as one's value) is a bare positional, joining the verb
/// word at the front of `positional` — `ls`/`show`'s revision and path
/// operands, and `log`'s bare-positional revision, all take this path.
fn tool_args(verb: &str, argv: &[&str]) -> ToolArgs {
    let mut args = ToolArgs::new();
    // A verb can be two words (`worktree list`), and the kernel binds each as
    // its own positional — the same way it splits the command line.
    for word in verb.split_whitespace() {
        args.positional.push(Value::String(word.to_string()));
    }
    let mut i = 0;
    while i < argv.len() {
        let token = argv[i];
        let Some(name) = token.strip_prefix("--") else {
            args.positional.push(Value::String(token.to_string()));
            i += 1;
            continue;
        };
        if VALUE_FLAGS.contains(&name) {
            let value = argv
                .get(i + 1)
                .unwrap_or_else(|| panic!("matrix flag '--{name}' takes a value, but none followed in {argv:?}"));
            args.named
                .insert(name.to_string(), Value::String((*value).to_string()));
            i += 2;
        } else if BOOL_FLAGS.contains(&name) {
            args.flags.insert(name.to_string());
            i += 1;
        } else {
            panic!(
                "matrix flag '--{name}' is not in VALUE_FLAGS or BOOL_FLAGS — \
                 classify it as one or the other so this harness binds it the \
                 way the kernel's real binder would"
            );
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
    #[cfg(feature = "textdiff")]
    for (verb, matrix) in TEXTDIFF_MATRIX {
        for argv in *matrix {
            let result = run(repo, "/mnt/main", verb, argv).await;
            assert_eq!(result.code, 0, "git {verb} {argv:?} failed: {}", result.err);
        }
    }
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

/// D.4 itself: the whole read surface, then nothing moved anywhere under the
/// fixture's scratch root — `.git`, the main working tree, and the linked
/// worktree at once (docs/issues.md P8; see the module doc for why sampling
/// only `.git` would miss two classes of write). This test subsumes what used
/// to be two separate tests, one fingerprinting `.git` alone and one
/// fingerprinting the linked worktree alone.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn read_verbs_do_not_touch_the_scratch_root() {
    let repo = RichRepo::build();
    let scratch = repo.scratch();

    let before = Fingerprint::take(&scratch);
    assert!(
        before.entries.len() > 10,
        "the fixture's scratch root has only {} entries — it is not rich \
         enough to prove anything",
        before.entries.len()
    );

    run_the_whole_matrix(&repo).await;

    let after = Fingerprint::take(&scratch);
    assert_unchanged(&before, &after, "the scratch root");
}

/// The same surface on a current-thread runtime, which is what an embedder
/// like kaish-web runs. `block_in_place` panics there; the compat helper is
/// what keeps this green, and this asserts it stays that way.
#[tokio::test]
async fn read_verbs_are_read_only_on_a_current_thread_runtime() {
    let repo = RichRepo::build();
    let scratch = repo.scratch();
    let before = Fingerprint::take(&scratch);

    run_the_whole_matrix(&repo).await;

    let after = Fingerprint::take(&scratch);
    assert_unchanged(&before, &after, "the scratch root");
}

/// The blind spots the module doc describes, demonstrated rather than
/// asserted from prose (docs/issues.md P8): a fingerprint of `.git` alone
/// cannot see a write to the main working tree, and cannot see a stray file
/// created directly under the scratch root outside every worktree. The
/// scratch-root fingerprint catches both.
///
/// This does not call into kaish-tools-git at all — it is a fixture-level
/// proof that the *old* narrower sampling was structurally blind, using the
/// exact same [`Fingerprint`] machinery [`read_verbs_do_not_touch_the_scratch_root`]
/// runs, over a write nothing in this crate performed.
#[test]
fn the_scratch_root_fingerprint_catches_what_dot_git_alone_cannot() {
    let repo = RichRepo::build();
    let git_dir = repo.git_dir();
    let scratch = repo.scratch();

    // Class 1: a write to a file in the main working tree, outside `.git`.
    {
        let before_git = Fingerprint::take(&git_dir);
        let before_scratch = Fingerprint::take(&scratch);

        let target = repo.root.join("README.md");
        let original = std::fs::read(&target).expect("read README.md");
        let mut tampered = original.clone();
        tampered.extend_from_slice(b"\nmain-worktree write\n");
        std::fs::write(&target, &tampered).expect("write to main working tree");

        let after_git = Fingerprint::take(&git_dir);
        let after_scratch = Fingerprint::take(&scratch);
        std::fs::write(&target, &original).expect("restore README.md");

        assert!(
            after_git.differences_from(&before_git).is_empty(),
            "a `.git`-only fingerprint unexpectedly saw a main-worktree write \
             — the blind spot this test demonstrates no longer exists, and \
             the module doc's P8 rationale is stale"
        );
        let diffs = after_scratch.differences_from(&before_scratch);
        assert!(
            !diffs.is_empty(),
            "the scratch-root fingerprint must catch a main-worktree write: {diffs:?}"
        );
    }

    // Class 2: a stray file created directly under the scratch root, outside
    // both the main working tree and the linked worktree — the shape a
    // rogue temp file or lock left in the wrong place would take.
    {
        let before_git = Fingerprint::take(&git_dir);
        let before_scratch = Fingerprint::take(&scratch);

        let stray = scratch.join("kaish-git-stray-outside-any-worktree.tmp");
        std::fs::write(&stray, b"should never exist").expect("write stray file");

        let after_git = Fingerprint::take(&git_dir);
        let after_scratch = Fingerprint::take(&scratch);
        std::fs::remove_file(&stray).expect("remove stray file");

        assert!(
            after_git.differences_from(&before_git).is_empty(),
            "a `.git`-only fingerprint unexpectedly saw a stray file created \
             outside .git and outside every worktree"
        );
        let diffs = after_scratch.differences_from(&before_scratch);
        assert!(
            diffs.iter().any(|d| d.contains("NEW path")),
            "the scratch-root fingerprint must catch a stray file created \
             outside every worktree: {diffs:?}"
        );
    }
}

/// The fingerprint must be able to fail, or the tests above prove nothing.
///
/// Each of the three signals is exercised separately: a new path, a content
/// change, and an mtime-only change. Directory mtimes are recorded too, but
/// whether a *real* create-then-remove inside one timestamp tick leaves a
/// trace is the filesystem's granularity to decide, and a test that depended
/// on that would be flaky rather than strict — the mechanism itself
/// (recording and diffing a directory's own mtime) is what
/// [`the_fingerprint_detects_a_transient_create_then_remove`] proves instead,
/// deterministically.
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

/// A positive control for transient create-then-remove detection
/// (docs/issues.md P8): `support.rs`'s [`Fingerprint`] doc claims a lock file
/// created and then removed "leaves no file behind but does move its
/// directory's mtime, and that is a write" — a claim no test exercised before
/// this one.
///
/// A real create-then-remove races the filesystem's timestamp granularity
/// (coarse on some hosts, so the natural mtime bump can land within one
/// tick and vanish — the flakiness `the_fingerprint_detects_a_write`'s doc
/// warns about). This test still performs the create-then-remove, to match
/// the real event, but then bumps the directory's mtime explicitly forward —
/// the same technique `the_fingerprint_detects_a_write`'s case 3 uses for a
/// file — so what is asserted is deterministic: the walker's directory-mtime
/// channel actually gets compared, not merely recorded.
#[test]
fn the_fingerprint_detects_a_transient_create_then_remove() {
    let repo = RichRepo::build();
    let git_dir = repo.git_dir();
    let before = Fingerprint::take(&git_dir);

    let dir = git_dir.join("refs").join("heads");
    let lock = dir.join("kaish-git-test-transient.lock");
    std::fs::write(&lock, b"lock").expect("create transient file");
    std::fs::remove_file(&lock).expect("remove transient file");

    let dir_handle = std::fs::File::open(&dir).expect("open refs/heads for set_times");
    let bumped = std::time::SystemTime::now() + std::time::Duration::from_secs(120);
    dir_handle
        .set_times(std::fs::FileTimes::new().set_modified(bumped))
        .expect("bump refs/heads' mtime");
    drop(dir_handle);

    let diffs = Fingerprint::take(&git_dir).differences_from(&before);
    assert!(
        diffs.iter().any(|d| d.contains("refs/heads") && d.contains("mtime changed")),
        "a directory's mtime move, with no surviving file to point at, must \
         still be reported: {diffs:?}"
    );
}

/// The `.idx` question (docs/issues.md P8): does gix-odb write a pack index
/// back to disk when it opens a pack that has none? Reasoning about
/// gix-odb's internals cannot answer this from outside the crate — running
/// the fingerprint across a read that must open the pack does.
///
/// `RichRepo::build` already runs `git gc --aggressive`, so the fixture has
/// exactly one pack. This test deletes that pack's `.idx`, runs `git log`
/// (which walks commit history through the object store and must open the
/// pack to read packed commits), and reports — via the fingerprint, not a
/// guess — whether anything under `.git` moved.
#[tokio::test]
async fn a_pack_without_its_idx_is_read_only_or_reported_as_not() {
    let repo = RichRepo::build();
    let git_dir = repo.git_dir();
    let pack_dir = git_dir.join("objects").join("pack");

    let idx_path = std::fs::read_dir(&pack_dir)
        .expect("read objects/pack")
        .filter_map(|e| e.ok())
        .map(|e| e.path())
        .find(|p| p.extension().is_some_and(|e| e == "idx"))
        .unwrap_or_else(|| {
            panic!(
                "no .idx found under {} — RichRepo::build must gc before this test runs",
                pack_dir.display()
            )
        });
    std::fs::remove_file(&idx_path).expect("delete the .idx");

    let before = Fingerprint::take(&git_dir);

    let backend = Arc::new(StrictBackend::single(PathBuf::from(MOUNT), repo.scratch()));
    let mut ctx = TestCtx::new(backend, "/mnt/main");
    let git = kaish_tools_git::tool(GitConfig::read_only()).expect("read-only config builds");
    let result = git.execute(tool_args("log", &[]), &mut ctx).await;

    let after = Fingerprint::take(&git_dir);
    let diffs = after.differences_from(&before);

    // Report the observed answer rather than assert one direction: whichever
    // way gix-odb behaves, it must be self-consistent with what it reports.
    // If it silently regenerated the `.idx` (or wrote anything else under
    // `.git`), that is real .git-write activity a read-only claim did not
    // predict, and the assertion below fails loudly rather than passing on
    // an assumption.
    eprintln!(
        "a_pack_without_its_idx: git log exited {} ({:?}); .git diffs: {:?}; \
         .idx recreated: {}",
        result.code,
        result.err,
        diffs,
        idx_path.exists()
    );
    assert!(
        diffs.is_empty(),
        "opening a pack with no .idx wrote to .git — kaish-tools-git is not \
         read-only in this case: {diffs:?}"
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
