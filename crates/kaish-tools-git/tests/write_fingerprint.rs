//! The second fingerprint test (feasibility §7, "two tests, not one"): every
//! REFUSED write spelling leaves the repository byte-identical.
//!
//! `readonly_fingerprint.rs` proves the read verbs write nothing. This file
//! proves the refusals write nothing: a refusal that took a lock, or wrote an
//! object, or appended half a reflog line before it noticed, is exactly the
//! failure that test cannot see, because the verb it runs is not a read verb.
//!
//! **Phase 1a state.** No write verb exists, so every spelling in [`REFUSED`]
//! is refused by the router as "this git is read-only", and the table runs
//! under the read-only config and (with `commit`) under a `WritableGitConfig`
//! whose profile enables no write verb yet. Phase 1b moves a spelling out of
//! [`REFUSED`] when its verb lands and into that verb's own
//! success-allowlist test, and keeps the spellings that STAY refused (`reset
//! --hard`, `commit --amend` under `Refuse`, ...) here. A row's `code` is the
//! exit code it must produce; a row that starts exiting 0 fails the test and
//! forces that decision.

#[path = "support.rs"]
mod support;

use std::path::PathBuf;
use std::sync::Arc;

use kaish_tool_api::Tool;
use kaish_tools_git::{GitConfig, ToolConfig};

use support::{Fingerprint, RichRepo, StrictBackend, TestCtx};

/// `(argv, exit code)`: each is a refusal today.
///
/// The router refuses a known write verb with exit 2 and a reason naming the
/// next read verb. Spellings that are not git verbs at all (the plumbing)
/// refuse the same way, as unknown verbs.
const REFUSED: &[(&[&str], i64)] = &[
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

fn configs() -> Vec<(&'static str, ToolConfig)> {
    #[cfg_attr(not(feature = "commit"), allow(unused_mut))]
    let mut v: Vec<(&'static str, ToolConfig)> = vec![("read-only", GitConfig::read_only().into())];
    #[cfg(feature = "commit")]
    v.push(("commit profile", GitConfig::read_only().with_commit_profile().into()));
    v
}

async fn run(repo: &RichRepo, config: ToolConfig, argv: &[&str]) -> kaish_types::ExecResult {
    let backend = Arc::new(StrictBackend::single(PathBuf::from("/mnt"), repo.scratch()));
    let mut ctx = TestCtx::new(backend, "/mnt/main");
    let git = kaish_tools_git::tool(config).expect("config builds");
    let (verb, rest) = argv.split_first().expect("a verb");
    git.execute(support::verb_words(verb, rest), &mut ctx).await
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn every_refused_write_spelling_leaves_the_repository_unchanged() {
    let repo = RichRepo::build();
    let scratch = repo.scratch();
    for (name, _) in configs() {
        for (argv, code) in REFUSED {
            // Rebuilt per row so a failure names the row, and one row's
            // config cannot carry state into the next.
            let config = configs().into_iter().find(|(n, _)| *n == name).expect("config").1;
            let before = Fingerprint::take(&scratch);
            let result = run(&repo, config, argv).await;
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
