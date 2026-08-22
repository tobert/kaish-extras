//! `git diff` behavior, with real git as the oracle (architecture.md B.4, H
//! PR 5).
//!
//! The phasing gate for this PR names three proofs: bare `diff` is
//! index→worktree, every result states its endpoints, and `--patch` without
//! `textdiff` exits 4. `bare_diff_matches_git_diff_name_status`,
//! `every_endpoint_pair_states_itself` and `patch_exits_four_naming_textdiff`
//! are those three. Everything else cross-checks a flag against
//! `git diff --name-status` / `--numstat`, or pins a divergence we chose not
//! to close.
//!
//! The oracle comparison is deliberately shaped like git's own output: a
//! letter, then the path (or the rename pair, tab-separated, exactly as
//! `--name-status` prints it). Comparing rendered lines rather than fields
//! means a status we spell differently from git fails here loudly.

#[path = "support.rs"]
mod support;

use std::path::{Path, PathBuf};
use std::sync::Arc;

use kaish_tool_api::Tool;
use kaish_types::{ExecResult, ToolArgs, Value};

use kaish_tools_git::{GitConfig, Limits};

use support::{git, DiffRepo, StrictBackend, TestCtx};

const MOUNT: &str = "/mnt";

const VALUE_FLAGS: &[&str] = &["repo", "limit", "from", "to", "path", "context"];
const BOOL_FLAGS: &[&str] = &[
    "json",
    "staged",
    "name-only",
    "patch",
    "find-renames",
    "no-find-renames",
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
            // A literal `--`: the end-of-options marker, which reaches a tool
            // as a positional exactly as the kernel hands it through.
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

async fn diff(mount_real: &Path, cwd: &str, argv: &[&str]) -> ExecResult {
    diff_with(GitConfig::read_only(), mount_real, cwd, argv).await
}

async fn diff_with(config: GitConfig, mount_real: &Path, cwd: &str, argv: &[&str]) -> ExecResult {
    let backend = Arc::new(StrictBackend::single(
        PathBuf::from(MOUNT),
        mount_real.to_path_buf(),
    ));
    let mut ctx = TestCtx::new(backend, cwd);
    let tool = kaish_tools_git::tool(config).expect("config");
    tool.execute(tool_args("diff", argv), &mut ctx).await
}

fn json(result: &ExecResult) -> serde_json::Value {
    assert_eq!(result.code, 0, "diff failed: {}", result.err);
    result
        .output()
        .and_then(|o| o.rich_json.clone())
        .expect("--json carries the typed model")
}

/// Our files rendered the way `git diff --name-status` renders its own: a
/// letter (`R100` included), then the path, or `old\tnew` for a rename.
fn our_name_status(model: &serde_json::Value) -> Vec<String> {
    model["files"]
        .as_array()
        .expect("files is an array")
        .iter()
        .map(|f| {
            let letter = match f["status"].as_str().expect("status word") {
                "added" => "A".to_string(),
                "deleted" => "D".to_string(),
                "modified" => "M".to_string(),
                "typechange" => "T".to_string(),
                "renamed" => format!(
                    "R{}",
                    f["similarity"].as_u64().expect("a rename carries a score")
                ),
                other => panic!("a diff must not produce status '{other}'"),
            };
            match f["old_path"].as_str() {
                Some(old) => format!("{letter}\t{old}\t{}", f["path"].as_str().expect("path")),
                None => format!("{letter}\t{}", f["path"].as_str().expect("path")),
            }
        })
        .collect()
}

/// Our files rendered the way `git diff --numstat` renders its own:
/// `added\tdeleted\tpath`, with `-` for a file git does not count.
fn our_numstat(model: &serde_json::Value) -> Vec<String> {
    model["files"]
        .as_array()
        .expect("files is an array")
        .iter()
        .map(|f| {
            let cell = |v: &serde_json::Value| match v.as_u64() {
                Some(n) => n.to_string(),
                None => "-".to_string(),
            };
            let path = match f["old_path"].as_str() {
                Some(old) => format!("{old} => {}", f["path"].as_str().expect("path")),
                None => f["path"].as_str().expect("path").to_string(),
            };
            format!("{}\t{}\t{path}", cell(&f["additions"]), cell(&f["deletions"]))
        })
        .collect()
}

/// Real git's answer, with its `--numstat` path compression undone and
/// sorted by path the way ours is.
///
/// `git diff --numstat` prints a rename whose halves share a directory as
/// `dir/{old.txt => new.txt}`. That is a display convention, not a different
/// answer, so it is expanded back to the two full paths rather than taught to
/// the tool — this surface has one spelling for a path, and it is the whole
/// path.
fn git_lines(root: &Path, args: &[&str]) -> Vec<String> {
    let out = git(root, args);
    let mut lines: Vec<String> = out.lines().map(expand_rename_braces).collect();
    lines.sort_by_key(|l| l.rsplit('\t').next().unwrap_or(l).to_string());
    lines
}

/// `a/b/{old => new}` → `a/b/old => a/b/new`, leaving every other line alone.
fn expand_rename_braces(line: &str) -> String {
    let Some(open) = line.find('{') else {
        return line.to_string();
    };
    let Some(close) = line.find('}') else {
        return line.to_string();
    };
    let prefix = &line[..open];
    let suffix = &line[close + 1..];
    let Some((old, new)) = line[open + 1..close].split_once(" => ") else {
        return line.to_string();
    };
    format!("{prefix}{old}{suffix} => {prefix}{new}{suffix}")
        // The prefix carries the leading `A\tD\t` columns; only the path half
        // takes them, so strip the copy the second path picked up.
        .replacen(
            &format!(" => {prefix}"),
            &format!(" => {}", prefix.rsplit('\t').next().unwrap_or(prefix)),
            1,
        )
}

fn sorted(mut lines: Vec<String>) -> Vec<String> {
    lines.sort_by_key(|l| l.rsplit('\t').next().unwrap_or(l).to_string());
    lines
}

// ═══════════════════════════════════════════════════════════════════════════
// The five endpoint pairs, each against its own git spelling
// ═══════════════════════════════════════════════════════════════════════════

/// The phasing gate, first half: bare `diff` is index→worktree, and that is
/// exactly what bare `git diff` reports. A build that had kept the earlier
/// HEAD→worktree draft would name the staged files here too and fail.
#[tokio::test]
async fn bare_diff_matches_git_diff_name_status() {
    let repo = DiffRepo::build();
    let model = json(&diff(&repo.scratch(), "/mnt/repo", &[]).await);
    assert_eq!(model["from"]["kind"], "index");
    assert_eq!(model["to"]["kind"], "worktree");
    assert_eq!(
        sorted(our_name_status(&model)),
        git_lines(&repo.root, &["diff", "--name-status"])
    );
}

#[tokio::test]
async fn bare_diff_line_counts_match_git_numstat() {
    let repo = DiffRepo::build();
    let model = json(&diff(&repo.scratch(), "/mnt/repo", &[]).await);
    assert_eq!(
        sorted(our_numstat(&model)),
        git_lines(&repo.root, &["diff", "--numstat"])
    );
}

/// `--staged` is HEAD→index — the F.4 endpoint, answered by flatten-and-
/// compare rather than by building a temporary index.
#[tokio::test]
async fn staged_matches_git_diff_staged() {
    let repo = DiffRepo::build();
    let model = json(&diff(&repo.scratch(), "/mnt/repo", &["--staged"]).await);
    assert_eq!(model["from"]["kind"], "rev");
    assert_eq!(model["from"]["rev"], "HEAD");
    assert_eq!(model["to"]["kind"], "index");
    assert_eq!(
        sorted(our_name_status(&model)),
        git_lines(&repo.root, &["diff", "--staged", "--name-status"])
    );
    assert_eq!(
        sorted(our_numstat(&model)),
        git_lines(&repo.root, &["diff", "--staged", "--numstat"])
    );
}

/// `--from <A>` is A→worktree, which is git's bare `git diff <commit>`.
#[tokio::test]
async fn from_a_revision_matches_git_diff_commit() {
    let repo = DiffRepo::build();
    let model = json(&diff(&repo.scratch(), "/mnt/repo", &["--from", "HEAD"]).await);
    assert_eq!(model["to"]["kind"], "worktree");
    assert_eq!(
        sorted(our_name_status(&model)),
        git_lines(&repo.root, &["diff", "HEAD", "--name-status"])
    );
    assert_eq!(
        sorted(our_numstat(&model)),
        git_lines(&repo.root, &["diff", "HEAD", "--numstat"])
    );
}

/// `--to <B>` is HEAD→B, with `HEAD` supplied rather than guessed.
#[tokio::test]
async fn to_a_revision_compares_head_against_it() {
    let repo = DiffRepo::build();
    let model = json(&diff(&repo.scratch(), "/mnt/repo", &["--to", "HEAD~1"]).await);
    assert_eq!(model["from"]["rev"], "HEAD");
    assert_eq!(model["to"]["rev"], "HEAD~1");
    assert_eq!(
        sorted(our_name_status(&model)),
        git_lines(&repo.root, &["diff", "HEAD", "HEAD~1", "--name-status"])
    );
}

/// `--from A --to B` is the one spelling for a range, and it agrees with
/// `git diff A B` on both the names and the counts.
#[tokio::test]
async fn from_and_to_compare_two_revisions() {
    let repo = DiffRepo::build();
    let model = json(&diff(&repo.scratch(), "/mnt/repo", &["--from", "HEAD~1", "--to", "HEAD"]).await);
    assert_eq!(
        sorted(our_name_status(&model)),
        git_lines(&repo.root, &["diff", "HEAD~1", "HEAD", "--name-status"])
    );
    assert_eq!(
        sorted(our_numstat(&model)),
        git_lines(&repo.root, &["diff", "HEAD~1", "HEAD", "--numstat"])
    );
}

/// The phasing gate, second half: **every** result states its endpoints, in
/// `--json` and on the first line of the text surface. An empty diff has no
/// rows to infer them from, which is exactly when it matters most.
#[tokio::test]
async fn every_endpoint_pair_states_itself() {
    let repo = DiffRepo::build();
    let head = repo.rev_parse("HEAD");
    let cases: Vec<(Vec<&str>, &str, String)> = vec![
        (vec![], "index → worktree", "index/worktree".into()),
        (vec!["--staged"], "HEAD (", "rev/index".into()),
        (vec!["--from", "HEAD"], "HEAD (", "rev/worktree".into()),
        (vec!["--to", "HEAD~1"], "HEAD (", "rev/rev".into()),
        (vec!["--from", "HEAD~1", "--to", "HEAD"], "HEAD~1 (", "rev/rev".into()),
    ];
    for (argv, text_starts_with, shape) in cases {
        let result = diff(&repo.scratch(), "/mnt/repo", &argv).await;
        assert_eq!(result.code, 0, "git diff {argv:?}: {}", result.err);
        let text = result.text_out().to_string();
        let first = text.lines().next().unwrap_or_default();
        assert!(
            first.starts_with(text_starts_with) && first.contains('→'),
            "git diff {argv:?} must state its endpoints on line one, got '{first}'"
        );
        let model = json(&result);
        let kinds = format!(
            "{}/{}",
            model["from"]["kind"].as_str().expect("from kind"),
            model["to"]["kind"].as_str().expect("to kind")
        );
        assert_eq!(kinds, shape, "git diff {argv:?} named the wrong endpoints");
    }

    // And the resolved oid is reported beside the spelling, so an agent can
    // pin what it read without a second call.
    let model = json(&diff(&repo.scratch(), "/mnt/repo", &["--from", "HEAD"]).await);
    assert_eq!(model["from"]["oid"], head);
}

/// An empty diff still names its endpoints and says plainly that nothing
/// changed, rather than returning an empty body an agent has to interpret.
#[tokio::test]
async fn an_empty_diff_still_states_its_endpoints() {
    let repo = DiffRepo::build();
    let result = diff(&repo.scratch(), "/mnt/repo", &["--from", "HEAD", "--to", "HEAD"]).await;
    assert_eq!(result.code, 0, "stderr: {}", result.err);
    let text = result.text_out().to_string();
    assert!(text.starts_with("HEAD ("), "endpoints first: {text}");
    assert!(text.contains("no changes"), "an empty diff must say so: {text}");
    let model = json(&result);
    assert_eq!(model["files"].as_array().expect("files").len(), 0);
    assert_eq!(model["totals"]["files"], 0);
}

// ═══════════════════════════════════════════════════════════════════════════
// Renames — the decided `similarity`, and the divergence it does not cover
// ═══════════════════════════════════════════════════════════════════════════

/// **The `similarity` decision, pinned.** An exact rename carries `100`, not
/// `null`: the two sides are byte-identical, so 100 is measured rather than
/// assumed, and it is the number git's own `R100` carries for the same pair.
/// `null` would say "unscored", which is false here and indistinguishable
/// from the `null` every non-rename row carries.
#[tokio::test]
async fn an_exact_rename_carries_similarity_100() {
    let repo = DiffRepo::build();
    let model = json(&diff(&repo.scratch(), "/mnt/repo", &["--staged"]).await);
    let rename = model["files"]
        .as_array()
        .expect("files")
        .iter()
        .find(|f| f["status"] == "renamed")
        .expect("the staged rename is reported as a rename");
    assert_eq!(rename["similarity"], 100);
    assert_eq!(rename["old_path"], "dir/nested.txt");
    assert_eq!(rename["path"], "dir/moved.txt");
    assert_eq!(rename["additions"], 0, "identical content changes no lines");
    assert_eq!(rename["deletions"], 0);
    // Git spells the same pair `R100`, so the text surface's letter and the
    // oracle's agree character for character.
    assert!(
        git(&repo.root, &["diff", "--staged", "--name-status"]).contains("R100"),
        "the oracle must be scoring this rename 100 too"
    );

    // Every other row carries `null` — the field is not a per-row constant.
    for file in model["files"].as_array().expect("files") {
        if file["status"] != "renamed" {
            assert!(
                file["similarity"].is_null(),
                "only a rename carries a similarity: {file}"
            );
        }
    }
}

/// The permanent limitation, characterized rather than hidden: a file that
/// was **edited and moved** has a different blob, so it never pairs. Git
/// scores it (`R087` here) and folds the pair; we report a delete plus an
/// add. Both behaviors are asserted separately, so the day one of them
/// changes this test says which (docs/issues.md, D1).
#[tokio::test]
async fn a_modified_and_moved_file_is_a_delete_plus_an_add() {
    let repo = DiffRepo::build();
    // Move `long.txt` and change one of its ten lines, staged — enough
    // similarity left that git's own detector scores and folds the pair.
    git(&repo.root, &["mv", "long.txt", "long-moved.txt"]);
    support::write_file(
        &repo.root,
        "long-moved.txt",
        "l1\nl2\nl3\nl4\nl5\nl6\nl7\nl8\nl9\nCHANGED\n",
    );
    git(&repo.root, &["add", "long-moved.txt"]);

    let model = json(&diff(&repo.scratch(), "/mnt/repo", &["--staged"]).await);
    let ours = our_name_status(&model);
    assert!(
        ours.contains(&"D\tlong.txt".to_string())
            && ours.contains(&"A\tlong-moved.txt".to_string()),
        "exact-match rename detection must report the pair separately: {ours:?}"
    );

    // Git's own answer, asserted as git's — the divergence, not our bug.
    let oracle = git(&repo.root, &["diff", "--staged", "--name-status"]);
    assert!(
        oracle.contains("long.txt\tlong-moved.txt") && oracle.contains('R'),
        "git is expected to score and fold this pair; it reported:\n{oracle}"
    );
}

/// `--no-find-renames` reports the pair separately, and agrees with
/// `git diff --no-renames` on exactly that.
#[tokio::test]
async fn no_find_renames_reports_the_pair_separately() {
    let repo = DiffRepo::build();
    let model = json(&diff(&repo.scratch(), "/mnt/repo", &["--staged", "--no-find-renames"]).await);
    assert_eq!(
        sorted(our_name_status(&model)),
        git_lines(
            &repo.root,
            &["diff", "--staged", "--no-renames", "--name-status"]
        )
    );
    assert!(
        model["files"]
            .as_array()
            .expect("files")
            .iter()
            .all(|f| f["status"] != "renamed"),
        "--no-find-renames must not pair anything"
    );
}

// ═══════════════════════════════════════════════════════════════════════════
// Content classes: binary, modes, symlinks, the working tree's missing oid
// ═══════════════════════════════════════════════════════════════════════════

/// A binary file is marked and left uncounted, which is what git's `-` in
/// `--numstat` means. Reporting zeros would claim it changed no lines.
#[tokio::test]
async fn a_binary_file_is_marked_and_not_counted() {
    let repo = DiffRepo::build();
    std::fs::write(repo.root.join("data.bin"), [0x00, 0x09, 0xFE, 0x00]).expect("rewrite binary");
    git(&repo.root, &["add", "data.bin"]);

    let model = json(&diff(&repo.scratch(), "/mnt/repo", &["--staged"]).await);
    let bin = model["files"]
        .as_array()
        .expect("files")
        .iter()
        .find(|f| f["path"] == "data.bin")
        .expect("the binary file changed");
    assert_eq!(bin["binary"], true);
    assert!(bin["additions"].is_null() && bin["deletions"].is_null());
    assert_eq!(
        sorted(our_numstat(&model)),
        git_lines(&repo.root, &["diff", "--staged", "--numstat"]),
        "git prints '-' for a binary file, and so must we"
    );
}

/// A mode-only change is a modification with zero lines on both sides, and
/// the two modes are the six-digit strings git prints. A comparison that
/// looked only at blob oids would miss the row entirely.
#[cfg(unix)]
#[tokio::test]
async fn a_mode_flip_is_a_modification_with_no_lines() {
    let repo = DiffRepo::build();
    let model = json(&diff(&repo.scratch(), "/mnt/repo", &["--staged"]).await);
    let run = model["files"]
        .as_array()
        .expect("files")
        .iter()
        .find(|f| f["path"] == "run.sh")
        .expect("the chmod is a reported change");
    assert_eq!(run["status"], "modified");
    assert_eq!(run["old_mode"], "100644");
    assert_eq!(run["new_mode"], "100755");
    assert_eq!(run["additions"], 0);
    assert_eq!(run["deletions"], 0);
    assert_eq!(run["old_oid"], run["new_oid"], "the blob did not change");
}

/// A symlink's blob is its target string, so retargeting one is an ordinary
/// one-line content change — the same `1 1` git counts.
#[cfg(unix)]
#[tokio::test]
async fn a_retargeted_symlink_counts_as_content() {
    let repo = DiffRepo::build();
    std::fs::remove_file(repo.root.join("link.txt")).expect("remove link");
    std::os::unix::fs::symlink("b.txt", repo.root.join("link.txt")).expect("relink");
    git(&repo.root, &["add", "link.txt"]);

    let model = json(&diff(&repo.scratch(), "/mnt/repo", &["--staged"]).await);
    let link = model["files"]
        .as_array()
        .expect("files")
        .iter()
        .find(|f| f["path"] == "link.txt")
        .expect("the symlink changed");
    assert_eq!(link["new_mode"], "120000");
    assert_eq!(
        sorted(our_numstat(&model)),
        git_lines(&repo.root, &["diff", "--staged", "--numstat"])
    );
}

/// A working-tree side reports no oid. The content is not in the object
/// store, so an oid there would send an agent to `git show` for an object
/// that is not there — `git diff --raw` prints all-zeros in the same place.
#[tokio::test]
async fn a_working_tree_side_reports_no_oid() {
    let repo = DiffRepo::build();
    let model = json(&diff(&repo.scratch(), "/mnt/repo", &[]).await);
    for file in model["files"].as_array().expect("files") {
        assert!(
            file["new_oid"].is_null(),
            "the worktree side must name no object: {file}"
        );
        if file["status"] != "added" {
            assert!(
                file["old_oid"].is_string(),
                "the index side is object-backed and must name its blob: {file}"
            );
        }
    }
}

/// An untracked file is in no diff at all — not bare, not against a
/// revision. It is `git status`'s answer, not `git diff`'s.
#[tokio::test]
async fn an_untracked_file_appears_in_no_diff() {
    let repo = DiffRepo::build();
    for argv in [vec![], vec!["--from", "HEAD"], vec!["--staged"]] {
        let model = json(&diff(&repo.scratch(), "/mnt/repo", &argv).await);
        assert!(
            !model["files"]
                .as_array()
                .expect("files")
                .iter()
                .any(|f| f["path"] == "untracked.txt"),
            "git diff {argv:?} reported an untracked file"
        );
    }
}

/// A path removed from the index but still on disk reads as deleted against
/// a revision, because it is no longer tracked — the same answer
/// `git diff HEAD` gives.
#[tokio::test]
async fn a_path_dropped_from_the_index_reads_as_deleted() {
    let repo = DiffRepo::build();
    git(&repo.root, &["rm", "--cached", "--quiet", "b.txt"]);
    let model = json(&diff(&repo.scratch(), "/mnt/repo", &["--from", "HEAD"]).await);
    assert_eq!(
        sorted(our_name_status(&model)),
        git_lines(&repo.root, &["diff", "HEAD", "--name-status"])
    );
    assert!(our_name_status(&model).contains(&"D\tb.txt".to_string()));
}

// ═══════════════════════════════════════════════════════════════════════════
// Flags: --name-only, --path, --limit, and the two textdiff refusals
// ═══════════════════════════════════════════════════════════════════════════

/// `--name-only` counts nothing, and says so with `null` rather than zero.
/// Zero would claim a file changed no lines, which is a different fact.
#[tokio::test]
async fn name_only_reports_paths_without_inventing_counts() {
    let repo = DiffRepo::build();
    let result = diff(&repo.scratch(), "/mnt/repo", &["--name-only"]).await;
    let model = json(&result);
    assert!(model["totals"]["additions"].is_null());
    assert!(model["totals"]["deletions"].is_null());
    for file in model["files"].as_array().expect("files") {
        assert!(file["additions"].is_null() && file["deletions"].is_null());
        assert!(file["binary"].is_null(), "nothing was read, so nothing is known");
    }
    // The same paths as a counted run, so `--name-only` narrows the answer
    // rather than changing it.
    let counted = json(&diff(&repo.scratch(), "/mnt/repo", &[]).await);
    let paths = |m: &serde_json::Value| -> Vec<String> {
        m["files"]
            .as_array()
            .expect("files")
            .iter()
            .map(|f| f["path"].as_str().expect("path").to_string())
            .collect()
    };
    assert_eq!(paths(&model), paths(&counted));
    // And the text surface drops the count columns rather than printing `-`.
    let text = result.text_out().to_string();
    assert!(text.contains("STATUS") && text.contains("PATH"));
    assert!(!text.contains("+ADD"), "--name-only has no counts to head: {text}");
}

/// `--path` restricts the answer, and does it the way `status` and `log` do —
/// one shared filter, so a literal path also matches everything under it.
#[tokio::test]
async fn a_path_filter_restricts_the_comparison() {
    let repo = DiffRepo::build();
    let model = json(&diff(&repo.scratch(), "/mnt/repo", &["--staged", "--path", "dir"]).await);
    let paths: Vec<&str> = model["files"]
        .as_array()
        .expect("files")
        .iter()
        .map(|f| f["path"].as_str().expect("path"))
        .collect();
    assert_eq!(paths, ["dir/moved.txt"]);

    // The same paths git reports for the same restriction.
    assert_eq!(
        sorted(our_name_status(&model)),
        git_lines(&repo.root, &["diff", "--staged", "--name-status", "--", "dir"])
    );
}

/// Paths after `--` are the same filter as `--path`, which is how git spells
/// it.
#[tokio::test]
async fn paths_after_the_marker_filter_too() {
    let repo = DiffRepo::build();
    let model = json(&diff(&repo.scratch(), "/mnt/repo", &["--staged", "--", "dir"]).await);
    let paths: Vec<&str> = model["files"]
        .as_array()
        .expect("files")
        .iter()
        .map(|f| f["path"].as_str().expect("path"))
        .collect();
    assert_eq!(paths, ["dir/moved.txt"]);
}

/// Git pathspec magic is a loud usage error, never matched as a literal path.
#[tokio::test]
async fn pathspec_magic_is_refused_by_name() {
    let repo = DiffRepo::build();
    let result = diff(&repo.scratch(), "/mnt/repo", &["--path", ":(exclude)dir"]).await;
    assert_eq!(result.code, 2, "pathspec magic is a usage error");
    assert!(result.err.contains(":(exclude)"), "stderr: {}", result.err);
}

/// `--limit` caps the files and says so, in `--json` and on stderr.
#[tokio::test]
async fn limit_truncates_and_reports_it() {
    let repo = DiffRepo::build();
    let result = diff(&repo.scratch(), "/mnt/repo", &["--staged", "--limit", "1"]).await;
    let model = json(&result);
    assert_eq!(model["files"].as_array().expect("files").len(), 1);
    assert_eq!(model["truncated"], true);
    assert!(result.err.contains("truncated"), "stderr: {}", result.err);
}

/// The embedder's `max_diff_files` is a hard cap that `--limit` cannot raise.
#[tokio::test]
async fn the_embedder_cap_is_not_raisable_by_an_argument() {
    let repo = DiffRepo::build();
    let config = GitConfig::read_only().with_limits(Limits {
        max_diff_files: 1,
        ..Limits::default()
    });
    let result = diff_with(config, &repo.scratch(), "/mnt/repo", &["--staged", "--limit", "500"]).await;
    let model = json(&result);
    assert_eq!(model["files"].as_array().expect("files").len(), 1);
    assert_eq!(model["truncated"], true);
}

/// The phasing gate, third half: `--patch` on a build without `textdiff`
/// exits 4, names the feature, and points at what this build does compute.
/// The other half of the pair is in `tests/textdiff.rs`, which is compiled
/// only when the feature is on and asserts that `--patch` then works.
#[cfg(not(feature = "textdiff"))]
#[tokio::test]
async fn patch_exits_four_naming_textdiff() {
    let repo = DiffRepo::build();
    let result = diff(&repo.scratch(), "/mnt/repo", &["--patch"]).await;
    assert_eq!(result.code, 4, "an unbuildable capability is exit 4 (E.5)");
    assert!(result.err.contains("textdiff"), "stderr: {}", result.err);
    assert!(result.err.contains("--patch"), "stderr: {}", result.err);
}

/// `--context` only sizes hunks, and this build produces none — so it is
/// refused the same way rather than accepted as a flag that does nothing.
/// With `textdiff` on it becomes a usage error instead
/// (`textdiff.rs::context_without_patch_is_a_usage_error`).
#[cfg(not(feature = "textdiff"))]
#[tokio::test]
async fn context_exits_four_for_the_same_reason() {
    let repo = DiffRepo::build();
    let result = diff(&repo.scratch(), "/mnt/repo", &["--context", "5"]).await;
    assert_eq!(result.code, 4);
    assert!(result.err.contains("textdiff"), "stderr: {}", result.err);
    assert!(result.err.contains("--context"), "stderr: {}", result.err);
}

// ═══════════════════════════════════════════════════════════════════════════
// Refusals: the spellings this surface does not accept
// ═══════════════════════════════════════════════════════════════════════════

/// `A..B` is not a spelling here. `--from`/`--to` is the one way to name a
/// range, and the refusal says so rather than resolving something surprising.
#[tokio::test]
async fn a_range_revspec_is_refused() {
    let repo = DiffRepo::build();
    for spec in ["HEAD~1..HEAD", "HEAD~1...HEAD"] {
        let result = diff(&repo.scratch(), "/mnt/repo", &["--from", spec]).await;
        assert_eq!(result.code, 2, "'{spec}' must be a usage error");
        assert!(
            result.err.contains(".."),
            "the refusal must name the syntax: {}",
            result.err
        );
    }
}

/// A bare operand is a revision in git and a path here would be a wrong
/// answer, so it is refused with both spellings named.
#[tokio::test]
async fn a_bare_operand_names_from_and_the_marker() {
    let repo = DiffRepo::build();
    let result = diff(&repo.scratch(), "/mnt/repo", &["HEAD"]).await;
    assert_eq!(result.code, 2);
    assert!(result.err.contains("--from"), "stderr: {}", result.err);
    assert!(result.err.contains("--"), "stderr: {}", result.err);
}

/// `--staged` and `--from` name two different comparisons; asking for both is
/// a usage error, not a silent pick.
#[tokio::test]
async fn staged_and_from_together_are_a_usage_error() {
    let repo = DiffRepo::build();
    let result = diff(&repo.scratch(), "/mnt/repo", &["--staged", "--from", "HEAD~1"]).await;
    assert_eq!(result.code, 2, "stderr: {}", result.err);
}

/// A revision that does not exist is a git-level failure about this
/// repository (exit 1), not a usage error.
#[tokio::test]
async fn an_unknown_revision_is_exit_one() {
    let repo = DiffRepo::build();
    let result = diff(&repo.scratch(), "/mnt/repo", &["--from", "no-such-branch"]).await;
    assert_eq!(result.code, 1, "stderr: {}", result.err);
}

// ═══════════════════════════════════════════════════════════════════════════
// Bounds and the embedder's caps
// ═══════════════════════════════════════════════════════════════════════════

/// A blob over `max_blob_bytes` is not counted, and the shortfall is stated
/// rather than reported as zero lines. The file still counts in `files`.
#[tokio::test]
async fn an_oversize_blob_is_declined_and_declared() {
    let repo = DiffRepo::build();
    let config = GitConfig::read_only().with_limits(Limits {
        max_blob_bytes: 8,
        ..Limits::default()
    });
    let result = diff_with(
        config,
        &repo.scratch(),
        "/mnt/repo",
        &["--from", "HEAD~1", "--to", "HEAD"],
    )
    .await;
    let model = json(&result);
    let capped: Vec<&serde_json::Value> = model["files"]
        .as_array()
        .expect("files")
        .iter()
        .filter(|f| f["lines_capped"] == true)
        .collect();
    assert!(!capped.is_empty(), "an 8-byte cap must decline something");
    for file in &capped {
        assert!(file["additions"].is_null() && file["deletions"].is_null());
    }
    assert_eq!(model["totals"]["lines_capped"], capped.len());
    assert!(
        result.text_out().contains("not counted"),
        "the text surface must state the shortfall: {}",
        result.text_out()
    );
}

/// A bare repository has no working tree, so the two endpoints that need one
/// refuse by name instead of answering about nothing.
#[tokio::test]
async fn a_bare_repository_refuses_the_worktree_endpoints() {
    let repo = DiffRepo::build();
    let bare = repo.scratch().join("bare.git");
    git(
        &repo.scratch(),
        &["clone", "--quiet", "--bare", repo.root.to_str().expect("utf-8"), "bare.git"],
    );
    assert!(bare.join("HEAD").exists(), "the bare clone must exist");

    // All three endpoint pairs that touch the index or the working tree
    // refuse. `--staged` is the one that matters most: with no index file to
    // read it would otherwise compare a real tree against an empty map and
    // report every tracked file as deleted.
    for argv in [vec!["--from", "HEAD"], vec!["--staged"], vec![]] {
        let result = diff(&repo.scratch(), "/mnt/bare.git", &argv).await;
        assert_eq!(result.code, 1, "git diff {argv:?}: {}", result.err);
        assert!(
            result.err.contains("working tree"),
            "the refusal must name what is missing: {}",
            result.err
        );
    }

    // Two revisions need no working tree, so that comparison still answers.
    let ok = diff(&repo.scratch(), "/mnt/bare.git", &["--from", "HEAD~1", "--to", "HEAD"]).await;
    assert_eq!(ok.code, 0, "stderr: {}", ok.err);
}

/// A verb the embedder subtracted is absent from the schema and unroutable.
#[tokio::test]
async fn a_subtracted_diff_verb_is_unroutable() {
    let repo = DiffRepo::build();
    let config = GitConfig::read_only().without_verb(kaish_tools_git::Verb::Diff);
    let result = diff_with(config, &repo.scratch(), "/mnt/repo", &[]).await;
    assert_eq!(result.code, 2, "routing has nothing to select: {}", result.err);
}

// ═══════════════════════════════════════════════════════════════════════════
// States that are not an ordinary comparison
// ═══════════════════════════════════════════════════════════════════════════

/// An unborn HEAD has no commit to compare against, which is a first-class
/// state and not an error: every staged file is an addition against the empty
/// tree, and `git diff --staged --name-status` agrees.
#[tokio::test]
async fn staged_against_an_unborn_head_is_all_additions() {
    support::require_git();
    let fixture = support::Fixture::empty();
    let root = fixture.path("fresh");
    std::fs::create_dir_all(&root).expect("create repo");
    git(&root, &["init", "--initial-branch=main", "--quiet"]);
    support::write_file(&root, "new.txt", "one\ntwo\n");
    git(&root, &["add", "new.txt"]);

    let result = diff(&fixture.root(), "/mnt/fresh", &["--staged"]).await;
    assert_eq!(result.code, 0, "stderr: {}", result.err);
    let model = json(&result);
    assert_eq!(model["from"]["rev"], "HEAD");
    assert_eq!(
        model["from"]["oid"], "",
        "an unborn HEAD names no commit, and must not invent one"
    );
    assert_eq!(
        sorted(our_name_status(&model)),
        git_lines(&root, &["diff", "--staged", "--name-status"])
    );
    assert_eq!(model["totals"]["additions"], 2);
}

/// An unmerged path has no stage 0 to compare, so it is left out of the
/// comparison rather than reported as an add or a delete — and the result
/// says how many were left out, in `--json` and on stderr. Silence here would
/// be the worst option: a conflicted file simply missing from a diff is a
/// wrong answer an agent cannot detect.
#[tokio::test]
async fn unmerged_paths_are_declared_not_silently_dropped() {
    support::require_git();
    let fixture = support::Fixture::empty();
    let root = fixture.path("conflict");
    std::fs::create_dir_all(&root).expect("create repo");
    git(&root, &["init", "--initial-branch=main", "--quiet"]);
    support::write_file(&root, "shared.txt", "base\n");
    support::write_file(&root, "other.txt", "untouched\n");
    git(&root, &["add", "shared.txt", "other.txt"]);
    git(&root, &["commit", "-m", "base", "--quiet"]);

    git(&root, &["checkout", "--quiet", "-b", "side"]);
    support::write_file(&root, "shared.txt", "side\n");
    git(&root, &["add", "shared.txt"]);
    git(&root, &["commit", "-m", "side", "--quiet"]);

    git(&root, &["checkout", "--quiet", "main"]);
    support::write_file(&root, "shared.txt", "main\n");
    git(&root, &["add", "shared.txt"]);
    git(&root, &["commit", "-m", "main", "--quiet"]);

    // A conflicting merge, which leaves stages 1..3 in the index.
    let merge = std::process::Command::new("git")
        .args(["merge", "side"])
        .current_dir(&root)
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_SYSTEM", "/dev/null")
        .env("GIT_AUTHOR_NAME", "Fixture Author")
        .env("GIT_AUTHOR_EMAIL", "author@example.invalid")
        .env("GIT_COMMITTER_NAME", "Fixture Author")
        .env("GIT_COMMITTER_EMAIL", "author@example.invalid")
        .output()
        .expect("git merge");
    assert!(!merge.status.success(), "the merge must actually conflict");

    let result = diff(&fixture.root(), "/mnt/conflict", &["--staged"]).await;
    assert_eq!(result.code, 0, "stderr: {}", result.err);
    let model = json(&result);
    assert_eq!(model["unmerged"], 1, "the conflicted path must be counted");
    assert!(
        result.err.contains("unmerged"),
        "stderr must say a path was left out: {}",
        result.err
    );
    assert!(
        !model["files"]
            .as_array()
            .expect("files")
            .iter()
            .any(|f| f["path"] == "shared.txt"),
        "a path with no stage 0 has nothing to compare, and reporting it as \
         deleted would be a wrong answer wearing a normal status"
    );

    // Git's own answer, asserted as git's: it reports a `U` row. This surface
    // has no unmerged row shape — B.4's file carries no `conflicted` field,
    // and adding one is a model change (docs/issues.md, D2) — so it declares
    // the omission instead. Both behaviors are pinned, so the day either one
    // changes this test says which.
    let oracle = git(&root, &["diff", "--staged", "--name-status"]);
    assert!(
        oracle.starts_with("U\t"),
        "git is expected to report an unmerged row; it reported:\n{oracle}"
    );
}

/// A submodule pointer move is one line each side, which is what git counts —
/// the `Subproject commit <oid>` pair a patch would show. Nothing asks the
/// object store about the gitlink's oid: it names a commit in *another*
/// repository, and a header lookup for it fails with "could not be found"
/// rather than answering "not a blob". That lookup is exactly the crash
/// `log --stat` had before PR 5 (docs/issues.md, L9), so this test is also
/// the proof that `diff` never reintroduces it.
#[tokio::test]
async fn a_submodule_pointer_move_counts_one_line_each_side() {
    support::require_git();
    let fixture = support::Fixture::empty();
    let sub = fixture.path("sub");
    std::fs::create_dir_all(&sub).expect("create sub");
    git(&sub, &["init", "--initial-branch=main", "--quiet"]);
    support::write_file(&sub, "f.txt", "one\n");
    git(&sub, &["add", "f.txt"]);
    git(&sub, &["commit", "-m", "s1", "--quiet"]);
    let first = git(&sub, &["rev-parse", "HEAD"]);
    support::write_file(&sub, "f.txt", "one\ntwo\n");
    git(&sub, &["add", "f.txt"]);
    git(&sub, &["commit", "-m", "s2", "--quiet"]);

    let root = fixture.path("super");
    std::fs::create_dir_all(&root).expect("create super");
    git(&root, &["init", "--initial-branch=main", "--quiet"]);
    git(
        &root,
        &[
            "-c",
            "protocol.file.allow=always",
            "submodule",
            "add",
            "--quiet",
            "../sub",
            "sub",
        ],
    );
    git(&root, &["commit", "-m", "add the submodule", "--quiet"]);
    git(&root.join("sub"), &["checkout", "--quiet", &first]);
    git(&root, &["add", "sub"]);

    let model = json(&diff(&fixture.root(), "/mnt/super", &["--staged"]).await);
    let link = model["files"]
        .as_array()
        .expect("files")
        .iter()
        .find(|f| f["path"] == "sub")
        .expect("the gitlink moved");
    assert_eq!(link["status"], "modified");
    assert_eq!(link["old_mode"], "160000");
    assert_eq!(link["additions"], 1);
    assert_eq!(link["deletions"], 1);
    assert_eq!(
        sorted(our_numstat(&model)),
        git_lines(&root, &["diff", "--staged", "--numstat"])
    );
}
