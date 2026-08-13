//! `git log` behavior, with real git as the oracle (architecture.md B.3, H
//! PR 3).
//!
//! Every test here can fail. The cross-check tests hold our commit selection
//! against `git log` run with the equivalent flags on the same fixture, so a
//! filter that quietly selects the wrong set — the failure mode no
//! read-only-ness check can catch, because a wrong answer touches nothing —
//! fails here.
//!
//! The one place we deliberately diverge from git is date parsing, and that
//! divergence has its own tests: git's `approxidate` accepts "2 weeks ago" and
//! also accepts "banana", resolving what it cannot parse to *now*. We refuse
//! both, loudly.

#[path = "support.rs"]
mod support;

use std::path::{Path, PathBuf};
use std::sync::Arc;

use kaish_tool_api::Tool;
use kaish_types::{ExecResult, ToolArgs, Value};

use kaish_tools_git::{GitConfig, Limits};

use support::{git, git_as, require_git, write_file, Fixture, StrictBackend, TestCtx};

/// The VFS root the fixture scratch directory is mounted at.
const MOUNT: &str = "/mnt";

/// Build the `ToolArgs` the kernel would, from an argv slice. A repeated
/// `--flag value` accumulates into a list, which is how `--path` is passed
/// more than once.
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
                // A repeated named argument is `Json(Array(Array))` — one inner
                // array per occurrence — which is how `ToolArgs::to_argv`
                // renders a `consumes > 1` parameter back into argv. Building
                // it here is what makes a repeated `--path` reach clap as two
                // occurrences rather than one overwriting the other.
                let next = match args.named.remove(token) {
                    Some(Value::Json(serde_json::Value::Array(mut outer))) => {
                        outer.push(serde_json::json!([*value]));
                        Value::Json(serde_json::Value::Array(outer))
                    }
                    Some(Value::String(first)) => Value::Json(serde_json::json!([[first], [*value]])),
                    Some(other) => panic!("unexpected repeat of {token}: {other:?}"),
                    None => Value::String((*value).to_string()),
                };
                args.named.insert(token.to_string(), next);
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

/// Run `git log` against `mount_real` mounted at `/mnt`, from `cwd`.
async fn log(mount_real: &Path, cwd: &str, argv: &[&str]) -> ExecResult {
    log_with(GitConfig::read_only(), mount_real, cwd, argv).await
}

/// The same, with an embedder config the test chose.
async fn log_with(config: GitConfig, mount_real: &Path, cwd: &str, argv: &[&str]) -> ExecResult {
    let backend = Arc::new(StrictBackend::single(
        PathBuf::from(MOUNT),
        mount_real.to_path_buf(),
    ));
    let mut ctx = TestCtx::new(backend, cwd);
    let tool = kaish_tools_git::tool(config).expect("config");
    tool.execute(tool_args("log", argv), &mut ctx).await
}

/// The typed model out of a successful result.
fn json(result: &ExecResult) -> serde_json::Value {
    assert_eq!(result.code, 0, "log failed: {}", result.err);
    result
        .output()
        .and_then(|o| o.rich_json.clone())
        .expect("--json carries the typed model")
}

/// The full oids our log reported, in order.
fn oids(result: &ExecResult) -> Vec<String> {
    json(result)["commits"]
        .as_array()
        .expect("commits is an array")
        .iter()
        .map(|c| c["oid"].as_str().expect("oid is a string").to_string())
        .collect()
}

/// The oids real git reports for the same question — the oracle.
fn git_oids(root: &Path, args: &[&str]) -> Vec<String> {
    let mut argv = vec!["log", "--format=%H"];
    argv.extend_from_slice(args);
    let out = git(root, &argv);
    out.lines().map(|l| l.trim().to_string()).collect()
}

// ═══════════════════════════════════════════════════════════════════════════
// The fixture: branching history, several authors, several dates
// ═══════════════════════════════════════════════════════════════════════════

/// A repository with the shape every `log` filter needs something to bite on:
/// a merge, a side branch, two authors, five distinct months, and files in
/// three directories.
///
/// ```text
///   c1 ── c2 ── c3 ─────── m5 ── c6      (main)
///           \             /
///            c4 ─────────              (side)
/// ```
struct History {
    fixture: Fixture,
    root: PathBuf,
}

impl History {
    fn build() -> Self {
        require_git();
        let fixture = Fixture::empty();
        let root = fixture.path("repo");
        std::fs::create_dir_all(&root).expect("create repo dir");

        let amy = ("Amy Tobey", "amy@example.invalid");
        let bob = ("Bob Reviewer", "bob@example.invalid");

        git(&root, &["init", "--initial-branch=main", "--quiet"]);
        git(&root, &["config", "gc.writeCommitGraph", "false"]);

        // c1 — the root commit.
        write_file(&root, "README.md", "one\n");
        write_file(&root, "src/lib.rs", "fn a() {}\n");
        git_as(&root, amy, "2026-01-01T00:00:00+00:00", &["add", "."]);
        git_as(
            &root,
            amy,
            "2026-01-01T00:00:00+00:00",
            &["commit", "-m", "initial commit", "--quiet"],
        );

        // c2 — src only, Amy.
        write_file(&root, "src/lib.rs", "fn a() {}\nfn b() {}\n");
        git_as(&root, amy, "2026-02-01T00:00:00+00:00", &["add", "."]);
        git_as(
            &root,
            amy,
            "2026-02-01T00:00:00+00:00",
            &["commit", "-m", "add b", "--quiet"],
        );

        // c4 — the side branch, off c2, Bob.
        git(&root, &["checkout", "--quiet", "-b", "side"]);
        write_file(&root, "src/side.rs", "fn side() {}\n");
        git_as(&root, bob, "2026-03-01T00:00:00+00:00", &["add", "."]);
        git_as(
            &root,
            bob,
            "2026-03-01T00:00:00+00:00",
            &["commit", "-m", "side work", "--quiet"],
        );

        // c3 — docs only, on main, Bob.
        git(&root, &["checkout", "--quiet", "main"]);
        write_file(&root, "docs/guide.md", "# guide\n\nbody\n");
        git_as(&root, bob, "2026-04-01T00:00:00+00:00", &["add", "."]);
        git_as(
            &root,
            bob,
            "2026-04-01T00:00:00+00:00",
            &[
                "commit",
                "-m",
                "document the thing\n\nA body paragraph.\n\nAnd a second one.",
                "--quiet",
            ],
        );

        // m5 — the merge, Amy.
        git_as(
            &root,
            amy,
            "2026-05-01T00:00:00+00:00",
            &["merge", "--no-ff", "-m", "merge side", "side", "--quiet"],
        );

        // c6 — the tip, Amy, touching src again.
        write_file(&root, "src/lib.rs", "fn a() {}\nfn b() {}\nfn c() {}\n");
        git_as(&root, amy, "2026-06-01T00:00:00+00:00", &["add", "."]);
        git_as(
            &root,
            amy,
            "2026-06-01T00:00:00+00:00",
            &["commit", "-m", "add c", "--quiet"],
        );

        git(&root, &["tag", "v1.0.0"]);

        Self { fixture, root }
    }

    fn scratch(&self) -> PathBuf {
        self.fixture.root()
    }

    fn rev_parse(&self, rev: &str) -> String {
        git(&self.root, &["rev-parse", rev])
    }
}

// ═══════════════════════════════════════════════════════════════════════════
// The default walk
// ═══════════════════════════════════════════════════════════════════════════

/// The default log is git's default log: same commits, same order.
#[tokio::test]
async fn default_walk_matches_git() {
    let h = History::build();
    let result = log(&h.scratch(), "/mnt/repo", &[]).await;
    assert_eq!(oids(&result), git_oids(&h.root, &["-n", "20"]));
}

/// `--limit` caps the report and says so — in the model and on stderr — and the
/// commits it does report are still git's first N.
#[tokio::test]
async fn limit_truncates_and_reports_it() {
    let h = History::build();
    let result = log(&h.scratch(), "/mnt/repo", &["--limit", "2"]).await;
    assert_eq!(result.code, 0, "a truncated log is still a success");
    assert_eq!(oids(&result), git_oids(&h.root, &["-n", "2"]));
    assert_eq!(json(&result)["truncated"], true);
    assert!(
        result.err.contains("truncated"),
        "truncation is reported on stderr too: {:?}",
        result.err
    );
}

/// A log that reaches the end of history is NOT truncated — the flag has to
/// mean something, so it must be false when the whole story fit.
#[tokio::test]
async fn a_complete_walk_is_not_truncated() {
    let h = History::build();
    let result = log(&h.scratch(), "/mnt/repo", &["--limit", "50"]).await;
    assert_eq!(json(&result)["truncated"], false);
    assert_eq!(oids(&result).len(), git_oids(&h.root, &[]).len());
}

/// The embedder's `max_rows` is a ceiling `--limit` cannot raise.
#[tokio::test]
async fn embedder_row_cap_outranks_the_flag() {
    let h = History::build();
    let config = GitConfig::read_only().with_limits(Limits {
        max_rows: 2,
        ..Default::default()
    });
    let result = log_with(config, &h.scratch(), "/mnt/repo", &["--limit", "50"]).await;
    assert_eq!(oids(&result).len(), 2, "the embedder's cap wins");
    assert_eq!(json(&result)["truncated"], true);
}

// ═══════════════════════════════════════════════════════════════════════════
// Revisions
// ═══════════════════════════════════════════════════════════════════════════

/// A branch, a tag, a full oid and a short oid all resolve, and all agree with
/// `git rev-parse`.
#[tokio::test]
async fn rev_accepts_the_documented_forms() {
    let h = History::build();
    let head = h.rev_parse("HEAD");
    let side = h.rev_parse("side");

    for (spec, expected) in [
        ("HEAD", head.clone()),
        ("main", head.clone()),
        ("v1.0.0", head.clone()),
        ("side", side.clone()),
        (head.as_str(), head.clone()),
        (&head[..8], head.clone()),
    ] {
        let result = log(&h.scratch(), "/mnt/repo", &["--rev", spec, "--limit", "1"]).await;
        assert_eq!(result.code, 0, "--rev {spec} failed: {}", result.err);
        assert_eq!(oids(&result)[0], expected, "--rev {spec} resolved wrong");
    }
}

/// `~N` and `^N` navigate, and they agree with `git rev-parse` — including
/// `^2`, which on the merge is the side branch and NOT the mainline.
#[tokio::test]
async fn rev_navigation_matches_git() {
    let h = History::build();
    for spec in ["HEAD~1", "HEAD~2", "HEAD^", "HEAD~1^2", "HEAD^0"] {
        let expected = h.rev_parse(spec);
        let result = log(&h.scratch(), "/mnt/repo", &["--rev", spec, "--limit", "1"]).await;
        assert_eq!(result.code, 0, "--rev {spec} failed: {}", result.err);
        assert_eq!(oids(&result)[0], expected, "--rev {spec} resolved wrong");
    }
}

/// The report echoes the revision as the caller spelled it, not the oid it
/// became — an agent that asked for `HEAD` should see `HEAD` come back.
#[tokio::test]
async fn the_report_echoes_the_spelling_not_the_oid() {
    let h = History::build();
    let result = log(&h.scratch(), "/mnt/repo", &["--rev", "HEAD~1", "--limit", "1"]).await;
    assert_eq!(json(&result)["rev"], "HEAD~1");
}

/// An unknown revision is a git-level "no" (exit 1), and names what was asked
/// for so the caller can see its own typo.
#[tokio::test]
async fn unknown_revision_exits_one_and_names_it() {
    let h = History::build();
    let result = log(&h.scratch(), "/mnt/repo", &["--rev", "no-such-branch"]).await;
    assert_eq!(result.code, 1, "stderr was: {}", result.err);
    assert!(
        result.err.contains("no-such-branch"),
        "the error names the revision: {}",
        result.err
    );
}

/// Revision syntax outside the small grammar is a usage error (exit 2) that
/// names the unsupported form — never silently resolved to a nearby commit.
#[tokio::test]
async fn unsupported_revspec_is_a_named_usage_error() {
    let h = History::build();
    for (spec, token) in [
        ("main..side", ".."),
        ("main...side", "..."),
        ("HEAD@{1}", "@{"),
        ("HEAD^{tree}", "^{"),
        ("HEAD:src/lib.rs", ":"),
    ] {
        let result = log(&h.scratch(), "/mnt/repo", &["--rev", spec]).await;
        assert_eq!(result.code, 2, "--rev {spec} should be a usage error");
        assert!(
            result.err.contains(token) || result.err.contains(spec),
            "the error names the unsupported form for {spec}: {}",
            result.err
        );
    }
}

// ═══════════════════════════════════════════════════════════════════════════
// Dates — the named design gate
// ═══════════════════════════════════════════════════════════════════════════

/// Both accepted forms select the same commits real git selects. The bounds
/// are picked strictly between commit dates, so an inclusive-vs-exclusive
/// boundary difference cannot make this pass or fail by accident.
#[tokio::test]
async fn date_windows_match_git() {
    let h = History::build();
    for (ours, theirs) in [
        (
            vec!["--since", "2026-03-15T00:00:00Z"],
            vec!["--since", "2026-03-15T00:00:00Z"],
        ),
        (vec!["--since", "2026-03-15"], vec!["--since", "2026-03-15"]),
        (vec!["--until", "2026-03-15"], vec!["--until", "2026-03-15"]),
        (
            vec!["--since", "2026-01-15", "--until", "2026-04-15"],
            vec!["--since", "2026-01-15", "--until", "2026-04-15"],
        ),
    ] {
        let result = log(&h.scratch(), "/mnt/repo", &ours).await;
        assert_eq!(result.code, 0, "log {ours:?} failed: {}", result.err);
        assert_eq!(oids(&result), git_oids(&h.root, &theirs), "window {ours:?}");
    }
}

/// The gate. Git accepts every one of these — resolving the ones it cannot
/// parse to *now* — and would hand back a window the caller never asked for.
/// We refuse, with exit 2 and a message that names the offending value.
#[tokio::test]
async fn approxidate_is_refused_loudly() {
    let h = History::build();
    for bad in ["2 weeks ago", "yesterday", "last tuesday", "now", "banana"] {
        let result = log(&h.scratch(), "/mnt/repo", &["--since", bad]).await;
        assert_eq!(
            result.code, 2,
            "--since '{bad}' must be refused, not guessed at"
        );
        assert!(
            result.err.contains(bad),
            "the refusal names the value: {}",
            result.err
        );
        assert!(
            result.err.contains("approxidate"),
            "the refusal explains why git's answer is not good enough: {}",
            result.err
        );
    }
}

/// A window that cannot contain anything is a usage error, not a confident
/// empty log — an empty result would read as "your history has no such
/// commits" rather than "your two bounds are backwards".
#[tokio::test]
async fn an_inverted_window_is_a_usage_error() {
    let h = History::build();
    let result = log(
        &h.scratch(),
        "/mnt/repo",
        &["--since", "2026-06-01", "--until", "2026-01-01"],
    )
    .await;
    assert_eq!(result.code, 2, "stderr was: {}", result.err);
}

// ═══════════════════════════════════════════════════════════════════════════
// Author, merges, first-parent, paths
// ═══════════════════════════════════════════════════════════════════════════

/// `--author` selects what git's `--author` selects on a literal needle.
#[tokio::test]
async fn author_filter_matches_git() {
    let h = History::build();
    for needle in ["Amy", "Bob", "amy@example.invalid"] {
        let result = log(&h.scratch(), "/mnt/repo", &["--author", needle]).await;
        assert_eq!(result.code, 0, "--author {needle} failed: {}", result.err);
        assert_eq!(
            oids(&result),
            git_oids(&h.root, &["--author", needle, "--fixed-strings"]),
            "--author {needle}"
        );
    }
}

/// `--author` is a **literal** substring, which is the documented divergence
/// from git's regex. `Amy.Tobey` matches nothing here, because the `.` is a
/// dot and not "any character" — git's default would match "Amy Tobey".
#[tokio::test]
async fn author_is_literal_not_a_regex() {
    let h = History::build();
    let result = log(&h.scratch(), "/mnt/repo", &["--author", "Amy.Tobey"]).await;
    assert_eq!(result.code, 0);
    assert!(
        oids(&result).is_empty(),
        "a regex metacharacter must be literal, so this matches nothing"
    );

    // The same needle under git's regex DOES match — which is the divergence
    // this test pins. If git ever stopped matching it, the contrast is gone
    // and this test is no longer proving anything.
    assert!(
        !git_oids(&h.root, &["--author", "Amy.Tobey"]).is_empty(),
        "git's regex author match is the behavior we are diverging from"
    );
}

/// `--merges` and `--no-merges` partition history exactly as git does.
#[tokio::test]
async fn merge_filters_match_git() {
    let h = History::build();

    let merges = log(&h.scratch(), "/mnt/repo", &["--merges"]).await;
    assert_eq!(oids(&merges), git_oids(&h.root, &["--merges"]));
    assert_eq!(oids(&merges).len(), 1, "the fixture has exactly one merge");

    let no_merges = log(&h.scratch(), "/mnt/repo", &["--no-merges"]).await;
    assert_eq!(oids(&no_merges), git_oids(&h.root, &["--no-merges"]));

    // Together they must be the whole history, with no overlap — otherwise one
    // of them is quietly dropping or double-counting a commit.
    let all = log(&h.scratch(), "/mnt/repo", &[]).await;
    assert_eq!(
        oids(&merges).len() + oids(&no_merges).len(),
        oids(&all).len(),
        "the two filters partition history"
    );
}

/// `--first-parent` follows the mainline, which on this fixture means the side
/// branch's commit is absent.
#[tokio::test]
async fn first_parent_matches_git() {
    let h = History::build();
    let result = log(&h.scratch(), "/mnt/repo", &["--first-parent"]).await;
    assert_eq!(oids(&result), git_oids(&h.root, &["--first-parent"]));

    let side_only = h.rev_parse("side");
    assert!(
        !oids(&result).contains(&side_only),
        "the side branch's own commit is off the mainline"
    );
    // And it IS reachable without the flag, so the assertion above is testing
    // the flag rather than a commit that was never there.
    let full = log(&h.scratch(), "/mnt/repo", &[]).await;
    assert!(oids(&full).contains(&side_only));
}

/// `--path` selects the commits that touched a path, agreeing with git.
#[tokio::test]
async fn path_filter_matches_git() {
    let h = History::build();
    for path in ["docs", "src/lib.rs", "src"] {
        let result = log(&h.scratch(), "/mnt/repo", &["--path", path]).await;
        assert_eq!(result.code, 0, "--path {path} failed: {}", result.err);
        assert_eq!(
            oids(&result),
            git_oids(&h.root, &["--", path]),
            "--path {path}"
        );
    }
}

/// `--path` is repeatable, and the result is the union.
#[tokio::test]
async fn repeated_paths_union() {
    let h = History::build();
    let result = log(
        &h.scratch(),
        "/mnt/repo",
        &["--path", "docs", "--path", "src/side.rs"],
    )
    .await;
    assert_eq!(result.code, 0, "stderr: {}", result.err);
    assert_eq!(oids(&result), git_oids(&h.root, &["--", "docs", "src/side.rs"]));
}

/// A `--path` given from a subdirectory is relative to that subdirectory, the
/// same way git's pathspecs are.
#[tokio::test]
async fn path_is_relative_to_the_callers_cwd() {
    let h = History::build();
    let from_sub = log(&h.scratch(), "/mnt/repo/src", &["--path", "lib.rs"]).await;
    assert_eq!(from_sub.code, 0, "stderr: {}", from_sub.err);
    assert_eq!(oids(&from_sub), git_oids(&h.root, &["--", "src/lib.rs"]));
}

/// Pathspec magic is a usage error for `log` exactly as it is for `status` —
/// the shared filter means one answer, not two dialects.
#[tokio::test]
async fn pathspec_magic_is_refused() {
    let h = History::build();
    let result = log(&h.scratch(), "/mnt/repo", &["--path", ":(exclude)docs"]).await;
    assert_eq!(result.code, 2, "stderr was: {}", result.err);
}

// ═══════════════════════════════════════════════════════════════════════════
// --body, --stat, --patch
// ═══════════════════════════════════════════════════════════════════════════

/// Without `--body` the body is absent; with it, the full text arrives and the
/// summary stays the first line either way.
#[tokio::test]
async fn body_is_absent_until_asked_for() {
    let h = History::build();
    let doc_commit = git(&h.root, &["rev-list", "-1", "HEAD", "--", "docs"]);

    let bare = log(&h.scratch(), "/mnt/repo", &["--rev", &doc_commit, "--limit", "1"]).await;
    let commit = &json(&bare)["commits"][0];
    assert_eq!(commit["summary"], "document the thing");
    assert!(commit["body"].is_null(), "no --body, no body");

    let with_body = log(
        &h.scratch(),
        "/mnt/repo",
        &["--rev", &doc_commit, "--limit", "1", "--body"],
    )
    .await;
    let commit = &json(&with_body)["commits"][0];
    assert_eq!(commit["summary"], "document the thing");
    let body = commit["body"].as_str().expect("--body fills the body in");
    assert!(body.contains("A body paragraph."), "body was: {body:?}");
    assert!(body.contains("And a second one."), "body was: {body:?}");
    assert!(
        !body.starts_with("document the thing"),
        "the summary is not repeated in the body: {body:?}"
    );
}

/// A commit with only a summary has an empty body under `--body` — which is a
/// different fact from "no body was requested", and the model spells them
/// differently (`""` vs `null`).
#[tokio::test]
async fn an_empty_body_is_not_a_missing_one() {
    let h = History::build();
    let result = log(&h.scratch(), "/mnt/repo", &["--rev", "HEAD", "--limit", "1", "--body"]).await;
    let commit = &json(&result)["commits"][0];
    assert_eq!(commit["summary"], "add c");
    assert_eq!(commit["body"], "", "asked for, and genuinely empty");
    assert!(!commit["body"].is_null());
}

/// Without `--stat` there are no counts; with it, they agree with
/// `git log --numstat` summed per commit.
#[tokio::test]
async fn stat_counts_match_git_numstat() {
    let h = History::build();

    let bare = log(&h.scratch(), "/mnt/repo", &["--limit", "1"]).await;
    assert!(json(&bare)["commits"][0]["stat"].is_null(), "no --stat, no stat");

    let result = log(&h.scratch(), "/mnt/repo", &["--stat", "--no-merges"]).await;
    let commits = json(&result);
    let commits = commits["commits"].as_array().expect("commits");
    assert!(!commits.is_empty());

    for commit in commits {
        let oid = commit["oid"].as_str().expect("oid");
        let stat = &commit["stat"];

        // `git show --numstat` on one commit: one "added\tdeleted\tpath" line
        // per changed file, against the first parent.
        let numstat = git(
            &h.root,
            &["show", "--numstat", "--format=", "--no-renames", oid],
        );
        let (mut files, mut added, mut deleted) = (0u64, 0u64, 0u64);
        for line in numstat.lines().filter(|l| !l.trim().is_empty()) {
            let mut parts = line.split('\t');
            let a = parts.next().unwrap_or("0");
            let d = parts.next().unwrap_or("0");
            files += 1;
            // git writes "-" for a binary file, which contributes no lines.
            added += a.parse::<u64>().unwrap_or(0);
            deleted += d.parse::<u64>().unwrap_or(0);
        }

        assert_eq!(stat["files"], files, "file count for {oid}: {numstat:?}");
        assert_eq!(stat["additions"], added, "additions for {oid}: {numstat:?}");
        assert_eq!(stat["deletions"], deleted, "deletions for {oid}: {numstat:?}");
    }
}

/// A merge reports zeros, matching git's default of showing no diffstat for a
/// merge — not a combined diff we invented.
#[tokio::test]
async fn a_merge_reports_no_stat_lines() {
    let h = History::build();
    let result = log(&h.scratch(), "/mnt/repo", &["--stat", "--merges"]).await;
    let commits = json(&result);
    let merge = &commits["commits"][0];
    assert_eq!(merge["stat"]["files"], 0);
    assert_eq!(merge["stat"]["additions"], 0);
    assert_eq!(merge["stat"]["deletions"], 0);
}

/// The root commit is diffed against the empty tree, so every one of its files
/// counts as added — matching what git reports for a first commit.
#[tokio::test]
async fn the_root_commit_is_all_additions() {
    let h = History::build();
    let root_commit = git(&h.root, &["rev-list", "--max-parents=0", "HEAD"]);
    let result = log(
        &h.scratch(),
        "/mnt/repo",
        &["--rev", &root_commit, "--limit", "1", "--stat"],
    )
    .await;
    let stat = &json(&result)["commits"][0]["stat"];
    assert_eq!(stat["files"], 2, "README.md and src/lib.rs");
    assert_eq!(stat["additions"], 2, "one line each");
    assert_eq!(stat["deletions"], 0);
}

/// A blob over the embedder's cap is counted as a changed file but contributes
/// no line delta, and says so in `lines_capped` — an honest lower bound rather
/// than a lie or an unbounded read.
#[tokio::test]
async fn an_oversized_blob_is_counted_but_not_read() {
    let h = History::build();
    let config = GitConfig::read_only().with_limits(Limits {
        max_blob_bytes: 8,
        ..Default::default()
    });
    let result = log_with(
        config,
        &h.scratch(),
        "/mnt/repo",
        &["--limit", "1", "--stat"],
    )
    .await;
    let stat = &json(&result)["commits"][0]["stat"];
    assert_eq!(stat["files"], 1, "the file still counts as changed");
    assert_eq!(stat["lines_capped"], 1, "and its lines are declared absent");
    assert_eq!(stat["additions"], 0, "no lines were counted from it");
}

/// `--patch` is refused with exit 4, naming the missing capability. Answering
/// it with a stat, or ignoring the flag, would be a wrong answer rather than a
/// missing one.
#[tokio::test]
async fn patch_exits_four_naming_textdiff() {
    let h = History::build();
    let result = log(&h.scratch(), "/mnt/repo", &["--patch"]).await;
    assert_eq!(result.code, 4, "stderr was: {}", result.err);
    assert!(
        result.err.contains("textdiff"),
        "the refusal names the feature: {}",
        result.err
    );
}

// ═══════════════════════════════════════════════════════════════════════════
// Wiring
// ═══════════════════════════════════════════════════════════════════════════

/// A disabled verb is unroutable, not merely rejected — the schema is built
/// from the config, so there is nothing to route to.
#[tokio::test]
async fn a_disabled_log_verb_is_unroutable() {
    let h = History::build();
    let config = GitConfig::read_only().without_verb(kaish_tools_git::Verb::Log);
    let result = log_with(config, &h.scratch(), "/mnt/repo", &[]).await;
    assert_ne!(result.code, 0, "a disabled verb cannot succeed");
}

/// The text table and `--json` are rendered from one model, so a commit in one
/// is a commit in the other.
#[tokio::test]
async fn the_table_and_the_json_agree() {
    let h = History::build();
    let result = log(&h.scratch(), "/mnt/repo", &[]).await;
    let rows: Vec<String> = result
        .output()
        .expect("output")
        .root
        .iter()
        .map(|node| node.name.clone())
        .collect();
    let short: Vec<String> = json(&result)["commits"]
        .as_array()
        .expect("commits")
        .iter()
        .map(|c| c["short_oid"].as_str().expect("short_oid").to_string())
        .collect();
    assert_eq!(rows, short, "one row per commit, same order, same oid");
    assert!(!rows.is_empty());
}
