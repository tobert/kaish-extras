//! `git diff --patch` — hunks and unified patch text, with real git as the
//! oracle (architecture.md B.4, F.1, H PR 6).
//!
//! The whole file is gated on the `textdiff` cargo feature, so a default
//! build compiles it to nothing. `tests/diff.rs` holds the other half: with
//! the feature off, `--patch` and `--context` exit 4 naming it.
//!
//! The phasing gate for this PR names two proofs, and both are here:
//! `git_apply_check_accepts_our_patch` feeds our patch to real
//! `git apply --check`, and the hostile-textconv fixture lives with its
//! family in `tests/hostile_repo.rs`.
//!
//! Two oracles do most of the work. `git diff --patch` is compared
//! line-for-line where we claim fidelity, and `git diff -U<N>` pins the
//! context arithmetic at four widths. Where we diverge on purpose the test
//! asserts *both* answers separately rather than being weakened to pass.

#![cfg(feature = "textdiff")]

#[path = "support.rs"]
mod support;

use std::path::{Path, PathBuf};
use std::sync::Arc;

use kaish_tool_api::Tool;
use kaish_types::{ExecResult, ToolArgs, Value};

use kaish_tools_git::{GitConfig, Limits};

use support::{git, PatchRepo, StrictBackend, TestCtx};

const MOUNT: &str = "/mnt";

const VALUE_FLAGS: &[&str] = &["repo", "limit", "from", "to", "path", "context"];
const BOOL_FLAGS: &[&str] = &[
    "json",
    "staged",
    "name-only",
    "patch",
    "find-renames",
    "no-find-renames",
    "stat",
];

fn tool_args(verb: &str, argv: &[&str]) -> ToolArgs {
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
        if name.is_empty() {
            args.positional.push(Value::String("--".to_string()));
            i += 1;
        } else if VALUE_FLAGS.contains(&name) {
            let value = argv
                .get(i + 1)
                .unwrap_or_else(|| panic!("'--{name}' takes a value, none followed in {argv:?}"));
            args.named
                .insert(name.to_string(), Value::String((*value).to_string()));
            i += 2;
        } else if BOOL_FLAGS.contains(&name) {
            args.flags.insert(name.to_string());
            i += 1;
        } else {
            panic!("'--{name}' is not classified in VALUE_FLAGS or BOOL_FLAGS");
        }
    }
    args
}

async fn run(verb: &str, mount_real: &Path, cwd: &str, argv: &[&str]) -> ExecResult {
    run_with(GitConfig::read_only(), verb, mount_real, cwd, argv).await
}

async fn run_with(
    config: GitConfig,
    verb: &str,
    mount_real: &Path,
    cwd: &str,
    argv: &[&str],
) -> ExecResult {
    let backend = Arc::new(StrictBackend::single(
        PathBuf::from(MOUNT),
        mount_real.to_path_buf(),
    ));
    let mut ctx = TestCtx::new(backend, cwd);
    let tool = kaish_tools_git::tool(config).expect("config");
    tool.execute(tool_args(verb, argv), &mut ctx).await
}

fn json(result: &ExecResult) -> serde_json::Value {
    assert_eq!(result.code, 0, "diff failed: {}", result.err);
    result
        .output()
        .and_then(|o| o.rich_json.clone())
        .expect("--json carries the typed model")
}

/// Our patch text for `argv`, and real git's for `git_argv`, ready to compare.
async fn both(repo: &PatchRepo, argv: &[&str], git_argv: &[&str]) -> (String, String) {
    let result = run("diff", &repo.scratch(), "/mnt/repo", argv).await;
    assert_eq!(result.code, 0, "git diff {argv:?}: {}", result.err);
    let ours = result.text_out().to_string();
    let theirs = format!("{}\n", git(&repo.root, git_argv));
    (ours, theirs)
}

/// Only the fragments for `paths`, in the order they appear — for comparing a
/// subset of a patch without asserting the whole file list.
fn fragments_for(patch: &str, paths: &[&str]) -> String {
    let mut out = String::new();
    let mut keep = false;
    for line in patch.lines() {
        if line.starts_with("diff --git ") {
            keep = paths.iter().any(|p| line.ends_with(&format!("b/{p}")));
        }
        if keep {
            out.push_str(line);
            out.push('\n');
        }
    }
    out
}

// ═══════════════════════════════════════════════════════════════════════════
// The patch text, against `git diff --patch`
// ═══════════════════════════════════════════════════════════════════════════

/// The flagship fidelity claim: for a revision-to-revision diff — the case
/// where both sides have oids and every F.1 header form is reachable — our
/// patch is `git diff --patch`, line for line.
///
/// The fixture is built so this cannot pass by covering nothing: an add, a
/// delete, an exact rename, a mode flip, a binary change, a lost trailing
/// newline, a path with a space, CRLF content, and a two-hunk source file
/// with two section headings.
#[tokio::test]
async fn a_revision_range_patch_is_byte_identical_to_git() {
    let repo = PatchRepo::build();
    let (ours, theirs) = both(
        &repo,
        &["--patch", "--from", "HEAD~1", "--to", "HEAD"],
        &["diff", "--patch", "HEAD~1", "HEAD"],
    )
    .await;
    assert!(
        theirs.contains("similarity index 100%") && theirs.contains("Binary files"),
        "the oracle must exercise the header forms this test claims to cover:\n{theirs}"
    );
    assert_eq!(ours, theirs);
}

/// The same claim for the index→worktree endpoint, minus the one thing that
/// endpoint cannot say: working-tree content has no oid in the model, so the
/// `index` line is omitted rather than invented (F.1, `src/patch.rs`).
#[tokio::test]
async fn a_worktree_patch_matches_git_except_for_the_index_line() {
    let repo = PatchRepo::build();
    let (ours, theirs) = both(&repo, &["--patch"], &["diff", "--patch"]).await;
    let without_index = |patch: &str| -> String {
        patch
            .lines()
            .filter(|l| !l.starts_with("index "))
            .map(|l| format!("{l}\n"))
            .collect()
    };
    assert!(
        theirs.contains("index "),
        "the oracle must carry an index line, or this test proves nothing:\n{theirs}"
    );
    assert!(
        !ours.contains("\nindex "),
        "a worktree side has no oid to name; the index line must be omitted:\n{ours}"
    );
    assert_eq!(without_index(&ours), without_index(&theirs));
}

/// `--staged` is HEAD→index, where both sides are object-backed again, so the
/// index line comes back and the patch is git's.
#[tokio::test]
async fn a_staged_patch_is_byte_identical_to_git() {
    let repo = PatchRepo::build();
    let (ours, theirs) = both(
        &repo,
        &["--patch", "--staged"],
        &["diff", "--patch", "--staged"],
    )
    .await;
    assert_eq!(ours, theirs);
}

/// `--context` is the context arithmetic, and it is the easiest thing to get
/// off by one. Four widths, each against `git diff -U<N>` on the same pair —
/// including `-U0`, where a hunk is nothing but its changes, and a width wide
/// enough to merge the fixture's two hunks into one.
#[tokio::test]
async fn context_widths_match_git_dash_u() {
    let repo = PatchRepo::build();
    for n in ["0", "1", "3", "7", "40"] {
        let (ours, theirs) = both(
            &repo,
            &["--patch", "--context", n, "--from", "HEAD~1", "--to", "HEAD", "--", "src/lib.rs"],
            &["diff", &format!("-U{n}"), "HEAD~1", "HEAD", "--", "src/lib.rs"],
        )
        .await;
        assert_eq!(ours, theirs, "--context {n} disagrees with git -U{n}");
    }

    // The negative control for the loop above: the widths must actually
    // produce different patches, or five identical comparisons would prove
    // only that one of them works.
    let narrow = both(
        &repo,
        &["--patch", "--context", "0", "--from", "HEAD~1", "--to", "HEAD", "--", "src/lib.rs"],
        &["diff", "-U0", "HEAD~1", "HEAD", "--", "src/lib.rs"],
    )
    .await
    .0;
    let wide = both(
        &repo,
        &["--patch", "--context", "40", "--from", "HEAD~1", "--to", "HEAD", "--", "src/lib.rs"],
        &["diff", "-U40", "HEAD~1", "HEAD", "--", "src/lib.rs"],
    )
    .await
    .0;
    assert!(
        narrow.lines().count() < wide.lines().count(),
        "--context 0 and --context 40 produced the same patch; the flag is not wired"
    );
    let hunk_headers = |patch: &str| patch.lines().filter(|l| l.starts_with("@@ ")).count();
    assert_eq!(
        hunk_headers(&narrow),
        2,
        "the fixture must produce two hunks at -U0:\n{narrow}"
    );
    assert_eq!(
        hunk_headers(&wide),
        1,
        "-U40 must merge the fixture's two hunks into one:\n{wide}"
    );
}

/// The phasing gate: our patch, fed to real `git apply --check` against a
/// clean checkout of the revision it claims to apply to.
///
/// This is what makes "we render the patch ourselves" a claim rather than a
/// hope. The binary file is left out by `--path`, because a patch carrying
/// `Binary files … differ` is refused by `git apply` — and it is refused
/// coming out of real `git diff` too, which the next test pins.
#[tokio::test]
async fn git_apply_check_accepts_our_patch() {
    let repo = PatchRepo::build();
    // Every path but the binary one, after `--`. Repeating `--path` is not
    // available in this harness — `ToolArgs::named` is a map, so the second
    // one would replace the first — and `--` is the spelling an agent has for
    // several paths anyway.
    let paths: Vec<&str> = vec![
        "--", "src", "added.txt", "gone.txt", "mode.sh", "moved.txt", "renamed.txt",
        "with space.txt", "crlf.txt", "nonl.txt",
    ];

    for (label, argv, at, must_have) in [
        (
            "HEAD~1 → HEAD",
            [vec!["--patch", "--from", "HEAD~1", "--to", "HEAD"], paths.clone()].concat(),
            "HEAD~1",
            vec!["@@ ", "rename to ", "new file mode ", "deleted file mode ", "old mode "],
        ),
        (
            "HEAD → index",
            [vec!["--patch", "--staged"], paths.clone()].concat(),
            "HEAD",
            vec!["@@ "],
        ),
    ] {
        let result = run("diff", &repo.scratch(), "/mnt/repo", &argv).await;
        assert_eq!(result.code, 0, "git diff {argv:?}: {}", result.err);
        let patch = result.text_out().to_string();
        for needle in &must_have {
            assert!(
                patch.contains(needle),
                "{label}: the patch must carry '{needle}', or apply proves little:\n{patch}"
            );
        }

        let checkout = repo.checkout_of(at);
        let patch_file = checkout.join("..").join(format!("{}.patch", at.replace('~', "_")));
        std::fs::write(&patch_file, &patch).expect("write patch");
        let out = std::process::Command::new("git")
            .args(["apply", "--check", "-v", patch_file.to_str().expect("utf-8")])
            .current_dir(&checkout)
            .output()
            .expect("run git apply");
        assert!(
            out.status.success(),
            "{label}: real git refused our patch ({}):\n{}\n--- the patch ---\n{patch}",
            out.status,
            String::from_utf8_lossy(&out.stderr)
        );
    }
}

/// The negative control for the test above, and the stated non-fidelity it
/// works around: a patch carrying `Binary files … differ` is rejected by
/// `git apply`, and **real git's own patch is rejected the same way**. Ours
/// is not worse than git's here; git's default output simply is not
/// appliable, which is why `--binary` exists and why F.1 declines to emit it.
#[tokio::test]
async fn a_binary_patch_is_unappliable_from_git_as_much_as_from_us() {
    let repo = PatchRepo::build();
    let checkout = repo.checkout_of("HEAD~1");
    let ours = run(
        "diff",
        &repo.scratch(),
        "/mnt/repo",
        &["--patch", "--from", "HEAD~1", "--to", "HEAD", "--path", "data.bin"],
    )
    .await;
    assert_eq!(ours.code, 0, "stderr: {}", ours.err);
    let ours = ours.text_out().to_string();
    let theirs = format!(
        "{}\n",
        git(&repo.root, &["diff", "--patch", "HEAD~1", "HEAD", "--", "data.bin"])
    );
    assert!(ours.contains("Binary files "), "ours:\n{ours}");
    assert_eq!(ours, theirs, "our binary fragment is git's own wording");

    for (label, patch) in [("ours", &ours), ("git's", &theirs)] {
        let file = checkout.join("..").join(format!("binary-{label}.patch"));
        std::fs::write(&file, patch).expect("write patch");
        let out = std::process::Command::new("git")
            .args(["apply", "--check", file.to_str().expect("utf-8")])
            .current_dir(&checkout)
            .output()
            .expect("run git apply");
        assert!(
            !out.status.success(),
            "{label} binary patch applied; the non-fidelity F.1 states is gone"
        );
        assert!(
            String::from_utf8_lossy(&out.stderr).contains("binary patch"),
            "{label}: refused for some other reason: {}",
            String::from_utf8_lossy(&out.stderr)
        );
    }
}

// ═══════════════════════════════════════════════════════════════════════════
// The typed model
// ═══════════════════════════════════════════════════════════════════════════

/// B.4's rule about the JSON shape: `op` is a word, never a sigil, so a
/// consumer never has to tell a leading space from an empty line — and the
/// text carries no sigil of its own for it to be confused with.
#[tokio::test]
async fn op_is_a_word_and_the_text_carries_no_sigil() {
    let repo = PatchRepo::build();
    let model = json(
        &run(
            "diff",
            &repo.scratch(),
            "/mnt/repo",
            &["--patch", "--from", "HEAD~1", "--to", "HEAD", "--", "src/lib.rs"],
        )
        .await,
    );
    let hunks = model["files"][0]["hunks"].as_array().expect("hunks array");
    assert_eq!(hunks.len(), 2, "the fixture's source file has two hunks");
    let mut seen = std::collections::BTreeSet::new();
    let mut lines = 0usize;
    for hunk in hunks {
        assert!(hunk["old_start"].as_u64().expect("old_start") > 0);
        // A hunk's own arithmetic: every `from`-side line is context or a
        // deletion, every `to`-side line is context or an insertion, and the
        // context lines are the same lines counted once each side.
        let count = |op: &str| {
            hunk["lines"]
                .as_array()
                .expect("lines")
                .iter()
                .filter(|l| l["op"] == op)
                .count() as u64
        };
        assert_eq!(
            count("context") + count("delete"),
            hunk["old_lines"].as_u64().expect("old_lines"),
            "the from side does not add up: {hunk}"
        );
        assert_eq!(
            count("context") + count("insert"),
            hunk["new_lines"].as_u64().expect("new_lines"),
            "the to side does not add up: {hunk}"
        );
        for line in hunk["lines"].as_array().expect("lines") {
            let op = line["op"].as_str().expect("op is a string");
            assert!(
                matches!(op, "context" | "delete" | "insert"),
                "op must be a word, got '{op}'"
            );
            seen.insert(op.to_string());
            line["text"].as_str().expect("text is a string");
            lines += 1;
        }
    }
    assert!(lines > 6, "only {lines} hunk lines — the fixture is too small");
    assert_eq!(
        seen.into_iter().collect::<Vec<_>>(),
        vec!["context", "delete", "insert"],
        "the fixture must produce all three ops, or this guard checks one word"
    );

    // And the text payload is the same lines with the sigil put back — which
    // is what makes "no sigil in `text`" a fact rather than a convention. A
    // line whose text kept its sigil would double it here.
    let result = run(
        "diff",
        &repo.scratch(),
        "/mnt/repo",
        &["--patch", "--from", "HEAD~1", "--to", "HEAD", "--", "src/lib.rs"],
    )
    .await;
    let patch = result.text_out().to_string();
    let body: Vec<&str> = patch
        .lines()
        .skip_while(|l| !l.starts_with("@@ "))
        .filter(|l| !l.starts_with("@@ ") && !l.starts_with("\\ No newline"))
        .collect();
    let rebuilt: Vec<String> = hunks
        .iter()
        .flat_map(|h| h["lines"].as_array().expect("lines"))
        .map(|l| {
            let sigil = match l["op"].as_str().expect("op") {
                "context" => ' ',
                "delete" => '-',
                _ => '+',
            };
            format!("{sigil}{}", l["text"].as_str().expect("text"))
        })
        .collect();
    assert_eq!(rebuilt, body);
}

/// The section heading git prints after `@@` is the one the model carries,
/// and it comes from git's default rule — not from `.gitattributes`, which
/// nothing here reads (D.3).
#[tokio::test]
async fn a_hunk_carries_the_section_heading_git_prints() {
    let repo = PatchRepo::build();
    let model = json(
        &run(
            "diff",
            &repo.scratch(),
            "/mnt/repo",
            &["--patch", "--from", "HEAD~1", "--to", "HEAD", "--", "src/lib.rs"],
        )
        .await,
    );
    let sections: Vec<String> = model["files"][0]["hunks"]
        .as_array()
        .expect("hunks")
        .iter()
        .map(|h| h["section"].as_str().unwrap_or_default().to_string())
        .collect();
    assert_eq!(
        sections,
        vec![
            "fn open(path: &str) -> Result<()> {".to_string(),
            "fn close(handle: Handle) -> Result<()> {".to_string()
        ]
    );
    // And the same two strings are what git puts after `@@`.
    let theirs = git(&repo.root, &["diff", "HEAD~1", "HEAD", "--", "src/lib.rs"]);
    let git_sections: Vec<String> = theirs
        .lines()
        .filter_map(|l| l.strip_prefix("@@ "))
        .filter_map(|rest| rest.split_once("@@ ").map(|(_, s)| s.to_string()))
        .collect();
    assert_eq!(sections, git_sections);
}

/// A binary file carries `binary: true` and no hunks — B.4 says so, and the
/// alternative (an empty hunk list) would read as "diffed, no changes".
#[tokio::test]
async fn a_binary_file_carries_binary_true_and_no_hunks() {
    let repo = PatchRepo::build();
    let model = json(
        &run(
            "diff",
            &repo.scratch(),
            "/mnt/repo",
            &["--patch", "--from", "HEAD~1", "--to", "HEAD", "--path", "data.bin"],
        )
        .await,
    );
    let file = &model["files"][0];
    assert_eq!(file["path"], "data.bin");
    assert_eq!(file["binary"], true);
    assert!(file["hunks"].is_null(), "a binary file has no hunks: {file}");
    assert!(file["additions"].is_null(), "and no counts: {file}");
}

/// A file whose trailing newline changed carries the fact on the line, not
/// only in the patch text — a JSON consumer reconstructing content needs it.
#[tokio::test]
async fn a_missing_trailing_newline_is_reported_on_the_line() {
    let repo = PatchRepo::build();
    let result = run(
        "diff",
        &repo.scratch(),
        "/mnt/repo",
        &["--patch", "--from", "HEAD~1", "--to", "HEAD", "--path", "nonl.txt"],
    )
    .await;
    assert_eq!(result.code, 0, "stderr: {}", result.err);
    assert_eq!(
        result.text_out().matches("\\ No newline at end of file").count(),
        2,
        "both sides lost their newline: {}",
        result.text_out()
    );
    let model = json(&result);
    let lines = model["files"][0]["hunks"][0]["lines"]
        .as_array()
        .expect("lines");
    assert!(
        lines.iter().all(|l| l["no_newline"] == true),
        "every line here ends the file without one: {lines:?}"
    );
    // Absent, not false, on an ordinary line — one flag per hunk line is the
    // largest thing this model emits.
    let other = json(
        &run(
            "diff",
            &repo.scratch(),
            "/mnt/repo",
            &["--patch", "--from", "HEAD~1", "--to", "HEAD", "--path", "added.txt"],
        )
        .await,
    );
    let first = &other["files"][0]["hunks"][0]["lines"][0];
    assert!(
        first.get("no_newline").is_none(),
        "a false flag must not be serialized: {first}"
    );
}

/// A rename with no content change has a header and no hunks — the same
/// thing git prints, and the honest shape for a pair that is byte-identical.
#[tokio::test]
async fn an_exact_rename_has_a_header_and_no_hunks() {
    let repo = PatchRepo::build();
    let result = run(
        "diff",
        &repo.scratch(),
        "/mnt/repo",
        &["--patch", "--from", "HEAD~1", "--to", "HEAD", "--", "moved.txt", "renamed.txt"],
    )
    .await;
    let text = result.text_out().to_string();
    assert!(text.contains("similarity index 100%"), "{text}");
    assert!(text.contains("rename from moved.txt"), "{text}");
    assert!(!text.contains("@@ "), "a 100% rename has no hunk: {text}");
    assert!(json(&result)["files"][0]["hunks"].is_null());
}

// ═══════════════════════════════════════════════════════════════════════════
// Bounds
// ═══════════════════════════════════════════════════════════════════════════

/// `max_hunk_bytes_per_file` is a real cap and it is applied **before** the
/// lines are built, not after: a group's cost is summed from the interner and
/// compared to what is left, so the `Vec` a cap would have trimmed is never
/// filled. Observable in the output rather than silent — `lines_capped` says
/// so on the file and in the totals, and a stderr note fires.
#[tokio::test]
async fn hunks_over_the_per_file_cap_are_declined_and_reported() {
    let repo = PatchRepo::build();
    let config = GitConfig::read_only().with_limits(Limits {
        max_hunk_bytes_per_file: 8,
        ..Limits::default()
    });
    let result = run_with(
        config,
        "diff",
        &repo.scratch(),
        "/mnt/repo",
        &["--patch", "--from", "HEAD~1", "--to", "HEAD", "--", "src/lib.rs"],
    )
    .await;
    assert_eq!(result.code, 0, "a cap is not an error: {}", result.err);
    let text = result.text_out().to_string();
    assert!(!text.contains("@@ "), "no hunk fits in 8 bytes: {text}");
    assert!(
        result.err.contains("max_hunk_bytes_per_file"),
        "the cap must name itself on stderr: {}",
        result.err
    );
    let model = json(&result);
    assert_eq!(model["files"][0]["lines_capped"], true);
    assert_eq!(model["totals"]["lines_capped"], 1);
    // The counts are a property of the diff, not of what was emitted, so a
    // cut patch still reports them exactly. This is what tells an agent the
    // difference between "over max_hunk_bytes_per_file" and "over
    // max_blob_bytes", where the counts are declined too.
    assert_eq!(model["files"][0]["additions"], 2);
    assert_eq!(model["files"][0]["deletions"], 2);

    // The negative control: the same diff under the default cap produces the
    // hunks this one declined, so the assertions above are about the cap and
    // not about a fixture that never had hunks.
    let normal = json(
        &run(
            "diff",
            &repo.scratch(),
            "/mnt/repo",
            &["--patch", "--from", "HEAD~1", "--to", "HEAD", "--", "src/lib.rs"],
        )
        .await,
    );
    assert_eq!(normal["files"][0]["lines_capped"], false);
    assert_eq!(normal["files"][0]["hunks"].as_array().expect("hunks").len(), 2);
}

/// A `--context` wide enough to swallow the file is bounded by the same cap
/// rather than by an allocation the caller chose. Nothing here refuses a
/// large `--context`; the byte cap simply answers first.
#[tokio::test]
async fn an_enormous_context_is_bounded_by_the_hunk_cap() {
    let repo = PatchRepo::build();
    let config = GitConfig::read_only().with_limits(Limits {
        max_hunk_bytes_per_file: 64,
        ..Limits::default()
    });
    let result = run_with(
        config,
        "diff",
        &repo.scratch(),
        "/mnt/repo",
        &[
            "--patch", "--context", "1000000", "--from", "HEAD~1", "--to", "HEAD", "--",
            "src/lib.rs",
        ],
    )
    .await;
    assert_eq!(result.code, 0, "stderr: {}", result.err);
    assert_eq!(json(&result)["files"][0]["lines_capped"], true);
    assert!(!result.text_out().contains("@@ "));
}

/// `--limit` still bounds the file list under `--patch`, and truncation is
/// still reported.
#[tokio::test]
async fn the_file_cap_still_applies_under_patch() {
    let repo = PatchRepo::build();
    let result = run(
        "diff",
        &repo.scratch(),
        "/mnt/repo",
        &["--patch", "--from", "HEAD~1", "--to", "HEAD", "--limit", "2"],
    )
    .await;
    let model = json(&result);
    assert_eq!(model["files"].as_array().expect("files").len(), 2);
    assert_eq!(model["truncated"], true);
    assert!(result.err.contains("truncated"), "stderr: {}", result.err);
    assert_eq!(result.text_out().matches("diff --git ").count(), 2);
}

// ═══════════════════════════════════════════════════════════════════════════
// The surface around it
// ═══════════════════════════════════════════════════════════════════════════

/// The patch payload is a patch and nothing else — no endpoint line, no
/// summary — so `git diff --patch | git apply` needs no preamble skipped.
/// B.4 still asks every result to state its endpoints, so they move to
/// stderr and stay in `--json`.
#[tokio::test]
async fn the_patch_payload_is_only_a_patch_and_the_endpoints_move_to_stderr() {
    let repo = PatchRepo::build();
    let result = run(
        "diff",
        &repo.scratch(),
        "/mnt/repo",
        &["--patch", "--from", "HEAD~1", "--to", "HEAD"],
    )
    .await;
    let text = result.text_out().to_string();
    assert!(
        text.starts_with("diff --git "),
        "a patch starts with a patch: {}",
        text.lines().next().unwrap_or_default()
    );
    assert!(!text.contains('→'), "no endpoint line in the payload: {text}");
    assert!(!text.contains("files changed,"), "no summary line: {text}");
    assert!(
        result.err.contains("HEAD~1 (") && result.err.contains('→'),
        "the endpoints must still be stated: {}",
        result.err
    );
    let model = json(&result);
    assert_eq!(model["from"]["rev"], "HEAD~1");
    assert_eq!(model["to"]["rev"], "HEAD");
}

/// Without `--patch` there is no hunk to size, so `--context` is a usage
/// error rather than a flag that is accepted and does nothing.
#[tokio::test]
async fn context_without_patch_is_a_usage_error() {
    let repo = PatchRepo::build();
    let result = run("diff", &repo.scratch(), "/mnt/repo", &["--context", "5"]).await;
    assert_eq!(result.code, 2, "stderr: {}", result.err);
    assert!(result.err.contains("--patch"), "stderr: {}", result.err);
}

/// `--name-only` says "paths, nothing read"; `--patch` says "read everything
/// and render it". clap refuses the pair rather than picking one.
#[tokio::test]
async fn patch_and_name_only_are_refused_together() {
    let repo = PatchRepo::build();
    let result = run(
        "diff",
        &repo.scratch(),
        "/mnt/repo",
        &["--patch", "--name-only"],
    )
    .await;
    assert_eq!(result.code, 2, "stderr: {}", result.err);
}

/// `git log --patch` assembles no patch text even with `textdiff` on, and the
/// refusal must say *that* rather than name a feature which is already
/// enabled. It names the spelling that does work.
#[tokio::test]
async fn log_patch_points_at_diff_patch_rather_than_at_the_feature() {
    let repo = PatchRepo::build();
    let result = run("log", &repo.scratch(), "/mnt/repo", &["--patch"]).await;
    assert_eq!(result.code, 4, "stderr: {}", result.err);
    assert!(
        !result.err.contains("textdiff"),
        "the feature is on; naming it as the fix is a lie: {}",
        result.err
    );
    assert!(
        result.err.contains("git diff --patch"),
        "the refusal must name what does work: {}",
        result.err
    );
}

/// `git info` reports the feature, so an agent that met exit 4 on `--patch`
/// can find out whether the build has the code at all.
#[tokio::test]
async fn git_info_reports_textdiff() {
    let repo = PatchRepo::build();
    let result = run("info", &repo.scratch(), "/mnt/repo", &[]).await;
    let model = result
        .output()
        .and_then(|o| o.rich_json.clone())
        .expect("--json");
    let features: Vec<String> = model["capabilities"]["features"]
        .as_array()
        .expect("features array")
        .iter()
        .map(|v| v.as_str().expect("a feature name").to_string())
        .collect();
    assert!(features.contains(&"textdiff".to_string()), "{features:?}");
    assert!(features.contains(&"read".to_string()), "{features:?}");
}

// ═══════════════════════════════════════════════════════════════════════════
// Divergences, pinned rather than closed
// ═══════════════════════════════════════════════════════════════════════════

/// Content with no NUL byte but invalid UTF-8 is text to git and to this
/// build, and this build renders its hunks lossily — U+FFFD where the bytes
/// were. Both answers are asserted separately: git's patch carries the
/// original byte, ours carries the replacement character, and ours therefore
/// does not apply. Characterization, not a fix (docs/issues.md, T1).
#[tokio::test]
async fn latin1_content_renders_lossily_and_git_does_not() {
    let repo = PatchRepo::build();
    // 0xE9 is `é` in latin-1 and not valid UTF-8 anywhere. No NUL byte, so
    // neither git nor this build calls it binary.
    std::fs::write(repo.root.join("latin1.txt"), b"caf\xe9\n").expect("write latin1");
    git(&repo.root, &["add", "latin1.txt"]);
    git(&repo.root, &["commit", "-m", "latin1", "--quiet"]);
    std::fs::write(repo.root.join("latin1.txt"), b"caf\xe9s\n").expect("write latin1");

    let result = run(
        "diff",
        &repo.scratch(),
        "/mnt/repo",
        &["--patch", "--path", "latin1.txt"],
    )
    .await;
    assert_eq!(result.code, 0, "stderr: {}", result.err);
    let model = json(&result);
    assert_eq!(model["files"][0]["binary"], false, "no NUL byte, so not binary");
    let ours = result.text_out().to_string();
    assert!(
        ours.contains('\u{FFFD}'),
        "our hunk text is lossy UTF-8: {ours:?}"
    );

    // git keeps the byte. Read its raw stdout: `support::git` is lossy itself,
    // so asking it would compare our replacement character against its own.
    let raw = std::process::Command::new("git")
        .args(["diff", "--patch", "--", "latin1.txt"])
        .current_dir(&repo.root)
        .output()
        .expect("run git diff");
    assert!(
        raw.stdout.contains(&0xe9),
        "git must keep the original byte, or this divergence is not real"
    );
    let theirs = String::from_utf8_lossy(&raw.stdout).into_owned();
    assert!(
        theirs.contains('\u{FFFD}'),
        "reading git's patch as UTF-8 is itself lossy — that is the same \
         conversion this build makes, and why the two disagree"
    );

    // And the consequence: ours does not apply where git's does.
    let checkout = repo.checkout_of("HEAD");
    let check = |name: &str, patch: &[u8]| {
        let file = checkout.join("..").join(name);
        std::fs::write(&file, patch).expect("write patch");
        std::process::Command::new("git")
            .args(["apply", "--check", file.to_str().expect("utf-8")])
            .current_dir(&checkout)
            .output()
            .expect("run git apply")
            .status
            .success()
    };
    assert!(check("latin1-git.patch", &raw.stdout), "git's applies");
    assert!(
        !check("latin1-ours.patch", ours.as_bytes()),
        "ours must not apply; if it now does, the lossy conversion is gone and \
         this characterization test should become a fidelity claim"
    );
}

/// `fragments_for` is used by nothing else yet, and a helper that is never
/// exercised is a helper that has never been right. This keeps it honest and
/// documents the one fragment shape a mode-only change produces: a header,
/// two mode lines, and no body.
#[tokio::test]
async fn a_mode_only_change_is_a_header_with_no_body() {
    let repo = PatchRepo::build();
    let (ours, theirs) = both(
        &repo,
        &["--patch", "--from", "HEAD~1", "--to", "HEAD"],
        &["diff", "--patch", "HEAD~1", "HEAD"],
    )
    .await;
    let ours = fragments_for(&ours, &["mode.sh"]);
    assert_eq!(ours, fragments_for(&theirs, &["mode.sh"]));
    assert_eq!(
        ours,
        "diff --git a/mode.sh b/mode.sh\nold mode 100644\nnew mode 100755\n"
    );
}
