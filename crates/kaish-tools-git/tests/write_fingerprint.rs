//! The second fingerprint test (feasibility §7, "two tests, not one"): every
//! REFUSED write spelling leaves the repository byte-identical.
//!
//! `readonly_fingerprint.rs` proves the read verbs write nothing. This file
//! proves the refusals write nothing: a refusal that took a lock, or wrote an
//! object, or appended half a reflog line before it noticed, is exactly the
//! failure that test cannot see, because the verb it runs is not a read verb.
//!
//! **Phase 1b state.** The five index verbs exist under the commit profile,
//! so the table is split by what refuses each spelling:
//!
//! - [`ROUTER_REFUSED`]: git commands no profile here provides yet (`commit`,
//!   `switch`, the plumbing). Refused by the router, exit 2, in both configs.
//!   The read-only config's text says "read-only"; the commit profile's text
//!   must not, because that would be false for it.
//! - [`READ_ONLY_REFUSED`]: the index verbs' spellings under the read-only
//!   config, where the verbs are not in the schema.
//! - [`COMMIT_REFUSED`]: spellings the index verbs themselves refuse under the
//!   commit profile (usage, policy, git's own safety checks). Each leaves the
//!   whole scratch tree byte-identical, directory mtimes included: a refusal
//!   takes no lock and writes no object.
//!
//! A success test with an exact allowlist is the control: the fingerprint is
//! shown to see a real write, and a verb that wrote more than its allowlist
//! (a stray lock, a temp object) fails there.

#[path = "support.rs"]
mod support;

use std::path::PathBuf;
use std::sync::Arc;

use kaish_tool_api::Tool;
use kaish_tools_git::{GitConfig, ToolConfig};

use support::{Fingerprint, RichRepo, StrictBackend, TestCtx};

/// `(argv, exit code)`: refused by the router, whatever the profile.
const ROUTER_REFUSED: &[(&[&str], i64)] = &[
    (&["commit", "-m", "x"], 2),
    (&["commit", "-am", "x"], 2),
    (&["commit", "--amend", "-m", "x"], 2),
    (&["commit", "--amend", "--no-edit"], 2),
    (&["commit", "--no-verify", "-m", "x"], 2),
    (&["switch", "-c", "topic"], 2),
    (&["checkout", "-b", "topic"], 2),
    (&["update-ref", "HEAD", "HEAD"], 2),
    (&["stash"], 2),
    (&["clean", "-fd"], 2),
    (&["gc"], 2),
    (&["push"], 2),
    (&["commit-tree", "HEAD^{tree}", "-m", "x"], 2),
    (&["write-tree"], 2),
    (&["hash-object", "-w", "README.md"], 2),
];

/// The index verbs' spellings, under the read-only config.
const READ_ONLY_REFUSED: &[(&[&str], i64)] = &[
    (&["add", "README.md"], 2),
    (&["add", "-A"], 2),
    (&["add", "."], 2),
    (&["add", "-u"], 2),
    (&["rm", "README.md"], 2),
    (&["rm", "--cached", "README.md"], 2),
    (&["mv", "README.md", "README2.md"], 2),
    (&["restore", "--staged", "README.md"], 2),
    (&["restore", "README.md"], 2),
    (&["reset"], 2),
    (&["reset", "--hard"], 2),
    (&["reset", "--soft", "HEAD~1"], 2),
];

/// Spellings the index verbs refuse under the commit profile, on `RichRepo`
/// (which has a staged edit to README.md, an unstaged edit to src/lib.rs and
/// an untracked file).
#[cfg(feature = "commit")]
const COMMIT_REFUSED: &[(&[&str], i64)] = &[
    (&["add", "-p"], 2),
    (&["add", "-i", "README.md"], 2),
    (&["add", "--patch", "README.md"], 2),
    (&["add", "nomatch"], 1),
    (&["add", "README.md", "nomatch"], 1),
    (&["add", ":(exclude)src"], 2),
    (&["rm"], 2),
    (&["rm", "nomatch"], 1),
    // README.md has a staged change; src is a directory.
    (&["rm", "README.md"], 1),
    (&["rm", "src"], 1),
    (&["rm", "src/lib.rs"], 1),
    (&["rm", "-r", "nomatch"], 1),
    (&["mv"], 2),
    (&["mv", "README.md"], 2),
    (&["mv", "untracked.txt", "elsewhere.txt"], 1),
    (&["mv", "gone.txt", "elsewhere.txt"], 1),
    (&["mv", "README.md", "src/lib.rs"], 1),
    (&["mv", "README.md", "nodir/README.md"], 1),
    (&["restore", "README.md"], 2),
    (&["restore", "--worktree", "README.md"], 2),
    (&["restore", "-p", "README.md"], 2),
    (&["restore", "--staged"], 2),
    (&["restore", "--staged", "nomatch"], 1),
    (&["reset", "--hard"], 2),
    (&["reset", "--hard", "HEAD"], 2),
    (&["reset", "--soft", "HEAD~1"], 2),
    (&["reset", "--mixed", "HEAD~1"], 2),
    (&["reset", "HEAD~1"], 2),
    (&["reset", "main"], 2),
    (&["reset", "-p"], 2),
    (&["reset", "nomatch"], 1),
];

async fn run(repo: &RichRepo, config: ToolConfig, argv: &[&str]) -> kaish_types::ExecResult {
    let backend = Arc::new(StrictBackend::single(PathBuf::from("/mnt"), repo.scratch()));
    let mut ctx = TestCtx::new(backend, "/mnt/main");
    let git = kaish_tools_git::tool(config).expect("config builds");
    let (verb, rest) = argv.split_first().expect("a verb");
    git.execute(support::verb_words(verb, rest), &mut ctx).await
}

async fn assert_refusals_change_nothing(
    repo: &RichRepo,
    name: &str,
    config: impl Fn() -> ToolConfig,
    table: &[(&[&str], i64)],
) {
    let scratch = repo.scratch();
    for (argv, code) in table {
        let before = Fingerprint::take(&scratch);
        let result = run(repo, config(), argv).await;
        let after = Fingerprint::take(&scratch);

        assert_eq!(
            result.code, *code,
            "[{name}] git {argv:?}: expected exit {code}, got {} ({})",
            result.code, result.err
        );
        assert!(!result.err.is_empty(), "[{name}] git {argv:?}: a refusal must say why");
        let diffs = after.differences_from(&before);
        assert!(
            diffs.is_empty(),
            "[{name}] git {argv:?} was refused but changed the repository:\n  {}",
            diffs.join("\n  ")
        );
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn every_refused_write_spelling_leaves_the_repository_unchanged() {
    let repo = RichRepo::build();
    assert_refusals_change_nothing(&repo, "read-only", || GitConfig::read_only().into(), ROUTER_REFUSED).await;
    assert_refusals_change_nothing(&repo, "read-only", || GitConfig::read_only().into(), READ_ONLY_REFUSED).await;
    #[cfg(feature = "commit")]
    {
        let commit = || GitConfig::read_only().with_commit_profile().into();
        assert_refusals_change_nothing(&repo, "commit profile", commit, ROUTER_REFUSED).await;
        assert_refusals_change_nothing(&repo, "commit profile", commit, COMMIT_REFUSED).await;
    }
}

/// A refusal in the commit profile has the five parts, and the router's text
/// does not call a tool with write verbs read-only.
#[cfg(feature = "commit")]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn commit_profile_refusals_have_five_parts_and_the_router_does_not_say_read_only() {
    let repo = RichRepo::build();
    // Pathspec magic keeps the read verbs' pre-existing, older error text.
    for (argv, _) in COMMIT_REFUSED.iter().filter(|(a, _)| !a.iter().any(|w| w.starts_with(":("))) {
        let r = run(&repo, GitConfig::read_only().with_commit_profile().into(), argv).await;
        let text = r.err.trim_end();
        assert!(text.contains(": refused."), "git {argv:?}: {text}");
        assert!(text.ends_with("Nothing was changed."), "git {argv:?}: {text}");
        assert!(
            text.contains("overrides this") || text.contains("no flag"),
            "git {argv:?} must say whether an override exists: {text}"
        );
        assert!(text.contains("git "), "git {argv:?} must name a next command: {text}");
    }
    for (argv, _) in ROUTER_REFUSED {
        let r = run(&repo, GitConfig::read_only().with_commit_profile().into(), argv).await;
        assert!(!r.err.contains("read-only"), "git {argv:?}: the commit profile is not read-only: {}", r.err);
        if r.err.contains("not supported") {
            assert!(r.err.contains("commit profile"), "git {argv:?}: {}", r.err);
        }
    }
    // And the read-only config keeps its text for every router refusal.
    for (argv, _) in ROUTER_REFUSED.iter().filter(|(a, _)| a[0] == "commit" || a[0] == "switch") {
        let r = run(&repo, GitConfig::read_only().into(), argv).await;
        assert!(r.err.contains("read-only"), "git {argv:?}: {}", r.err);
    }
}

/// What a successful index verb may change under the scratch root, and
/// nothing else: the allowlist, with the negative controls.
#[cfg(feature = "commit")]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_successful_write_changes_only_its_allowlist() {
    let repo = RichRepo::build();
    let scratch = repo.scratch();
    let allowed = |d: &str| {
        d.starts_with("main/.git/index")
            || d.starts_with("main/.git/objects/")
            || d.starts_with("main/.git mtime")
            || d.starts_with("main/.git/objects mtime")
    };
    for (argv, working_tree_change) in [
        (&["add", "untracked.txt"][..], None),
        (&["add", "-A"][..], None),
        (&["restore", "--staged", "README.md"][..], None),
        (&["mv", "src/lib.rs", "src/moved.rs"][..], Some(&["main/src"][..])),
        (&["rm", "--cached", "src/moved.rs"][..], None),
        (&["rm", "-f", "README.md"][..], Some(&["main/README.md", "main mtime"][..])),
    ] {
        let before = Fingerprint::take(&scratch);
        let r = run(&repo, GitConfig::read_only().with_commit_profile().into(), argv).await;
        assert_eq!(r.code, 0, "git {argv:?}: {}", r.err);
        let diffs = Fingerprint::take(&scratch).differences_from(&before);
        assert!(!diffs.is_empty(), "git {argv:?} succeeded but the fingerprint saw nothing");
        for d in &diffs {
            let in_tree = working_tree_change.is_some_and(|ps| ps.iter().any(|p| d.starts_with(p)));
            assert!(allowed(d) || in_tree, "git {argv:?} changed {d}, which is outside its allowlist");
            assert!(!d.contains(".lock"), "git {argv:?} left a lock: {d}");
            assert!(!d.contains("/.tmp"), "git {argv:?} left a temp file: {d}");
        }
        assert!(
            diffs.iter().any(|d| d.starts_with("main/.git/index")),
            "git {argv:?} must have rewritten the index: {diffs:?}"
        );
    }
}

/// The control. If the fingerprint could not see a write, the test above
/// would pass over a refusal that wrote. Real git, run through the same
/// fixture, must move it.
#[test]
fn the_fingerprint_sees_a_real_git_write() {
    let repo = RichRepo::build();
    let scratch = repo.scratch();
    let before = Fingerprint::take(&scratch);
    support::git(&repo.root, &["add", "untracked.txt"]);
    let diffs = Fingerprint::take(&scratch).differences_from(&before);
    assert!(
        diffs.iter().any(|d| d.contains("index")),
        "real `git add` should change .git/index, and the fingerprint must show it: {diffs:?}"
    );
}

/// The table can fail: the verbs it names are real refusals, not typos the
/// router would also reject for being unknown. The first four are in the
/// router's own write-verb list.
#[tokio::test]
async fn the_table_names_real_write_verbs_with_the_read_only_reason() {
    let repo = RichRepo::build();
    for argv in [&["add", "README.md"][..], &["commit", "-m", "x"], &["rm", "a"], &["mv", "a", "b"]] {
        let result = run(&repo, GitConfig::read_only().into(), argv).await;
        assert!(
            result.err.contains("read-only"),
            "git {argv:?} should be refused as read-only, got: {}",
            result.err
        );
    }
}
