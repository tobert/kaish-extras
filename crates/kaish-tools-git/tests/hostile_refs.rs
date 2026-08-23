//! Hostile ref content — docs/issues.md P3.
//!
//! `docs/embedding-git.md` names four attacker-controlled parser surfaces for
//! a long-lived server: `.git/index`, packfiles, refs, and config. The index
//! has `discovery_ceiling.rs` and friends; config has the `include.path`
//! escape set in `repo.rs`'s unit tests. Refs had nothing before this file —
//! no fixture ever handed this crate a `packed-refs` or a `HEAD` that does not
//! parse the way a well-behaved git wrote it, even though both are ordinary
//! text files inside a mount an attacker's repository fully controls.
//!
//! Six shapes, each planted into an otherwise-normal fixture and run against
//! every ref-reading verb ([`VERBS`] — which in a nine-verb, all-reads-HEAD
//! build is all nine):
//!
//! - `packed-refs` as binary garbage (arbitrary non-UTF-8 bytes).
//! - `packed-refs` as a half line (a record truncated mid-oid, no trailing
//!   newline — what a crash mid-`git pack-refs` leaves behind).
//! - `packed-refs` as one 10 MB line (no newlines at all — a parser that
//!   scans for `\n` before bounding a line reads the whole file into one
//!   buffer).
//! - a loose ref (`refs/heads/late`, which survives `pack-refs --all` because
//!   it is created after) containing `zzz\n` — not an oid, not a symref.
//! - `HEAD` containing garbage text — neither `ref: refs/...` nor a 40-hex
//!   oid.
//! - `HEAD` detached at a valid-shaped 40-hex oid that names no object in
//!   this repository — the shape a parser accepts, the object store refuses.
//!
//! **Why the test itself is the no-panic assertion.** Every verb here runs
//! in-process, through [`kaish_tool_api::Tool::execute`] called directly from
//! this test's own async task — the same call an embedder's kernel makes,
//! with nothing in between that catches unwinds (`tool.rs`'s `execute` has no
//! `catch_unwind`, and neither does anything it calls). A parser panic
//! anywhere in the call stack unwinds straight into this test function, and
//! `cargo test` reports that test as failed with the panic message. There is
//! no separate "did it panic" boolean to assert on — the test *failing to
//! complete* **is** the failure mode this file exists to catch, and a test
//! that ran to completion and returned any `ExecResult` at all (any exit
//! code, any error text) has already proven the no-panic half of its claim.
//! What is asserted explicitly on top of that is *stability*: the same
//! hostile input run twice must produce the same exit code both times — a
//! parser that sometimes succeeds and sometimes doesn't on identical bytes
//! (order-of-iteration UB, an uninitialized read) is a defect this file
//! should also catch, and "ran without panicking" alone would miss it.

#[path = "support.rs"]
mod support;

use std::path::PathBuf;
use std::sync::Arc;

use kaish_tool_api::Tool;
use kaish_types::{ExecResult, ToolArgs, Value};

use kaish_tools_git::GitConfig;

use support::{RefsRepo, StrictBackend, TestCtx};

/// The VFS root the fixture is mounted at.
const MOUNT: &str = "/mnt";

/// Every ref-reading verb, invoked bare (no flags) — the argv a caller would
/// type with nothing else. In this crate's nine-verb read profile every verb
/// resolves `HEAD` (directly, or through a revision that defaults to it), so
/// this list is deliberately all of [`kaish_tools_git::Verb::ALL`] rather than
/// a hand-picked subset — see `all_verbs_are_covered` below, which fails loudly
/// if a verb is added here without a matching entry in that enum, or vice
/// versa.
const VERBS: &[&str] = &[
    "info",
    "status",
    "log",
    "ls",
    "show",
    "diff",
    "branch",
    "tag",
    "worktree list",
];

/// Build the `ToolArgs` for a bare verb invocation.
fn bare_args(verb: &str) -> ToolArgs {
    let mut args = ToolArgs::new();
    for word in verb.split_whitespace() {
        args.positional.push(Value::String(word.to_string()));
    }
    args
}

/// Run one bare verb invocation against `repo`, from its own root.
async fn run(repo: &RefsRepo, verb: &str) -> ExecResult {
    let backend = Arc::new(StrictBackend::single(PathBuf::from(MOUNT), repo.scratch()));
    let mut ctx = TestCtx::new(backend, "/mnt/repo");
    let git = kaish_tools_git::tool(GitConfig::read_only()).expect("read-only config builds");
    git.execute(bare_args(verb), &mut ctx).await
}

/// Run every verb in [`VERBS`] against `repo` **twice**, and report `(verb,
/// code_first_run, code_second_run, message_from_first_run)` for each.
///
/// Two runs of the identical hostile input is the stability check: a parser
/// with a determinism bug (iterating a `HashMap`, reading uninitialized
/// memory) can pass once and fail the next call on bytes that never changed.
/// Neither run is expected to panic — see the module doc for why that is not
/// a separate assertion.
async fn probe_all_verbs(repo: &RefsRepo) -> Vec<(&'static str, i64, i64, String)> {
    let mut out = Vec::new();
    for verb in VERBS {
        let first = run(repo, verb).await;
        let second = run(repo, verb).await;
        out.push((*verb, first.code, second.code, first.err.clone()));
    }
    out
}

/// Assert every `(verb, code1, code2, err)` triple ran the same exit code
/// twice, and print what each hostile scenario actually produced — the report
/// this file exists to generate, not merely a pass/fail bit.
fn assert_stable_and_report(scenario: &str, results: &[(&'static str, i64, i64, String)]) {
    for (verb, code1, code2, err) in results {
        assert_eq!(
            code1, code2,
            "{scenario}: `git {verb}` was not stable — exit {code1} on the \
             first run, {code2} on the second, over byte-identical hostile \
             input"
        );
        eprintln!("{scenario}: git {verb} -> exit {code1} ({err:?})");
    }
}

// ── The six hostile shapes ──────────────────────────────────────────────────

/// `packed-refs` replaced with arbitrary binary garbage: NUL bytes, a raw
/// 0xFF/0xFE byte pair (invalid as UTF-8 in either order), and no line
/// structure at all.
#[tokio::test]
async fn packed_refs_binary_garbage() {
    let repo = RefsRepo::build();
    let path = repo.root.join(".git/packed-refs");
    let mut bytes = vec![0u8, 1, 2, 3, 0xFF, 0xFE, 0x80, 0x81, 0x00, 0x0A];
    bytes.extend_from_slice(&[0xC3, 0x28]); // an invalid two-byte UTF-8 sequence
    std::fs::write(&path, &bytes).expect("write binary packed-refs");

    let results = probe_all_verbs(&repo).await;
    assert_stable_and_report("packed_refs_binary_garbage", &results);
}

/// `packed-refs` truncated mid-record: a valid header comment, one complete
/// line, then a line cut off in the middle of its oid with no trailing
/// newline — what a process killed mid-`git pack-refs --all` leaves on disk.
#[tokio::test]
async fn packed_refs_half_line() {
    let repo = RefsRepo::build();
    let path = repo.root.join(".git/packed-refs");
    let content = "# pack-refs with: peeled fully-peeled sorted \n\
                   76b2922d90e7bacfee8e4befd0491f873513dfcd refs/heads/main\n\
                   76b2922d90e7bacfee8e4bef";
    std::fs::write(&path, content).expect("write half-line packed-refs");

    let results = probe_all_verbs(&repo).await;
    assert_stable_and_report("packed_refs_half_line", &results);
}

/// `packed-refs` as a single 10 MB line with no newline anywhere in the file
/// — a parser that reads the whole file before looking for `\n` allocates a
/// 10 MB `String`/`Vec` per call; a parser that assumes a bounded line length
/// could behave very differently.
#[tokio::test]
async fn packed_refs_ten_megabyte_line() {
    let repo = RefsRepo::build();
    let path = repo.root.join(".git/packed-refs");
    let content = "a".repeat(10 * 1024 * 1024);
    std::fs::write(&path, content.as_bytes()).expect("write 10MB packed-refs");

    let results = probe_all_verbs(&repo).await;
    assert_stable_and_report("packed_refs_ten_megabyte_line", &results);
}

/// A loose ref containing `zzz` — three bytes that are neither a 40-hex oid
/// nor a `ref: ` symbolic-ref line. `refs/heads/late` is created in
/// `RefsRepo::build` *after* `pack-refs --all`, so it is guaranteed to still
/// be a loose file on disk rather than folded into `packed-refs`.
#[tokio::test]
async fn loose_ref_containing_zzz() {
    let repo = RefsRepo::build();
    let path = repo.root.join(".git/refs/heads/late");
    assert!(
        path.is_file(),
        "fixture assumption broken: refs/heads/late must still be a loose \
         file after RefsRepo::build's pack-refs — the corruption below would \
         silently land nowhere"
    );
    std::fs::write(&path, "zzz\n").expect("write garbage loose ref");

    let results = probe_all_verbs(&repo).await;
    assert_stable_and_report("loose_ref_containing_zzz", &results);
}

/// `HEAD` replaced with garbage text: neither `ref: refs/...` nor a 40-hex
/// oid — the shape neither of `HEAD`'s two legal forms takes.
#[tokio::test]
async fn head_containing_garbage() {
    let repo = RefsRepo::build();
    let path = repo.root.join(".git/HEAD");
    std::fs::write(&path, "this is not a git HEAD\n").expect("write garbage HEAD");

    let results = probe_all_verbs(&repo).await;
    assert_stable_and_report("head_containing_garbage", &results);
}

/// `HEAD` detached at a well-formed 40-hex oid that names no object in this
/// repository — the shape a parser accepts and the object store refuses. A
/// SHA-1 fixture this small cannot collide with the planted oid by accident.
#[tokio::test]
async fn detached_head_names_an_absent_oid() {
    let repo = RefsRepo::build();
    let path = repo.root.join(".git/HEAD");
    let absent_oid = "deadbeef00112233445566778899aabbccddeeff";
    assert_eq!(absent_oid.len(), 40, "the planted oid must be 40 hex chars");
    assert_ne!(
        absent_oid, repo.a,
        "the planted oid must not collide with a real commit in the fixture"
    );
    std::fs::write(&path, format!("{absent_oid}\n")).expect("detach HEAD at an absent oid");

    let results = probe_all_verbs(&repo).await;
    assert_stable_and_report("detached_head_names_an_absent_oid", &results);
}

/// [`VERBS`] must list every verb this build can execute, or the coverage
/// claim in the module doc ("every ref-reading verb") is unchecked prose. A
/// verb added to [`kaish_tools_git::Verb::ALL`] without a matching entry here
/// fails this rather than silently going untested against hostile refs.
#[test]
fn all_verbs_are_covered() {
    use kaish_tools_git::Verb;
    let covered: Vec<&str> = VERBS.to_vec();
    assert_eq!(
        covered.len(),
        Verb::ALL.len(),
        "VERBS lists {covered:?} but Verb::ALL has {} entries \
         ({:?}) — hostile_refs.rs is not covering every ref-reading verb",
        Verb::ALL.len(),
        Verb::ALL.iter().map(Verb::as_str).collect::<Vec<_>>()
    );
    for verb in Verb::ALL {
        assert!(
            covered.contains(&verb.as_str()),
            "verb '{}' has no entry in VERBS",
            verb.as_str()
        );
    }
}

/// Sanity control: an *uncorrupted* fixture must run every verb at exit 0, or
/// every "stable exit" result above is meaningless — a harness bug that made
/// every invocation fail the same way would look identical to "handled the
/// hostile input cleanly".
#[tokio::test]
async fn uncorrupted_fixture_runs_every_verb_at_exit_zero() {
    let repo = RefsRepo::build();
    for verb in VERBS {
        let result = run(&repo, verb).await;
        assert_eq!(
            result.code, 0,
            "git {verb} failed against the *uncorrupted* fixture: {}",
            result.err
        );
    }
}
