//! git's own spellings, through a real kaish kernel (docs/git.md §1, "git's own
//! spellings, when they mean what git means").
//!
//! Terminal-Bench runs on 2026-09-30 and 2026-10-01 showed models typing
//! `git branch -a`, `git log --oneline` and `git diff A B`, and every refusal
//! cost a model call. These tests drive the spellings through
//! `Kernel::execute` rather than `Tool::execute`, because the kernel's parser
//! and argument binder sit between an agent and this tool, and a test that
//! skips them is how `HEAD~1` failing to parse went unnoticed (docs/issues.md
//! K3).
//!
//! Each accepted spelling is asserted equal to the spelling it stands for,
//! with a negative control that the answer is not empty — two empty answers
//! are equal too. Each refusal is asserted to say "not supported" and to
//! carry a next step.

#[path = "support.rs"]
mod support;

use std::path::{Path, PathBuf};
use std::sync::Arc;

use kaish_kernel::{Kernel, KernelConfig};
use kaish_tool_api::{KernelBackend, Tool};
use kaish_types::ToolSchema;

use kaish_tools_git::GitConfig;

use support::{git, require_git, write_file, Fixture, StrictBackend};

/// Three commits on `main`, a branch named `docs` beside a tracked `docs/`
/// directory, a remote-tracking ref, one staged change and one unstaged one.
fn repo() -> (Fixture, PathBuf) {
    require_git();
    let fixture = Fixture::empty();
    let root = fixture.path("repo");
    std::fs::create_dir_all(&root).expect("create repo dir");
    git(&root, &["init", "--initial-branch=main", "--quiet"]);
    write_file(&root, "a.txt", "one\n");
    write_file(&root, "src/lib.rs", "pub fn a() {}\n");
    write_file(&root, "docs/readme.md", "docs\n");
    git(&root, &["add", "."]);
    git(&root, &["commit", "-m", "first", "--quiet"]);
    write_file(&root, "a.txt", "two\n");
    write_file(&root, "src/lib.rs", "pub fn b() {}\n");
    git(&root, &["commit", "-am", "second", "--quiet"]);
    write_file(&root, "a.txt", "three\n");
    git(&root, &["commit", "-am", "third", "--quiet"]);
    // A branch whose name is also a tracked directory: the word `docs` is
    // both a revision and a path, which git refuses to guess about.
    git(&root, &["branch", "docs", "HEAD~1"]);
    git(&root, &["update-ref", "refs/remotes/origin/main", "HEAD~2"]);
    // Staged: docs/readme.md. Unstaged: src/lib.rs.
    write_file(&root, "docs/readme.md", "docs, staged\n");
    git(&root, &["add", "docs/readme.md"]);
    write_file(&root, "src/lib.rs", "pub fn c() {}\n");
    (fixture, root)
}

fn tool() -> Box<dyn Tool> {
    Box::new(kaish_tools_git::tool(GitConfig::read_only()).expect("valid config"))
}

async fn kernel(root: &Path, cwd: &str) -> Kernel {
    let backend: Arc<dyn KernelBackend> =
        Arc::new(StrictBackend::single(PathBuf::from("/mnt"), root.to_path_buf()));
    let git = kaish_tools_git::tool(GitConfig::read_only()).expect("valid config");
    let mut cfg = KernelConfig::transient();
    // Set here rather than with `cd`: the kernel's `cd` stats the directory
    // through the backend, and `StrictBackend` fails any VFS I/O on purpose.
    cfg.cwd = PathBuf::from(cwd);
    Kernel::with_backend(backend, cfg, |_| {}, |tools| tools.register(git))
        .expect("kernel assembles from a valid git tool")
}

/// What a script printed and exited with. A parse error is a test failure
/// here, not a result: every script in this file is valid kaish.
struct Run {
    code: i64,
    out: String,
    err: String,
}

async fn run(root: &Path, script: &str) -> Run {
    run_in(root, "/mnt", script).await
}

async fn run_in(root: &Path, cwd: &str, script: &str) -> Run {
    let k = kernel(root, cwd).await;
    let result = k
        .execute(script)
        .await
        .unwrap_or_else(|e| panic!("kaish refused to run {script:?}: {e}"));
    Run {
        code: result.code,
        out: result.text_out().to_string(),
        err: result.err.clone(),
    }
}

/// Two scripts that must give the same successful, non-empty answer.
async fn same(root: &Path, spelling: &str, stands_for: &str) -> String {
    let a = run(root, spelling).await;
    let b = run(root, stands_for).await;
    assert_eq!(a.code, 0, "{spelling:?} failed: {}", a.err);
    assert_eq!(b.code, 0, "{stands_for:?} failed: {}", b.err);
    assert!(
        !b.out.trim().is_empty() && b.out.trim() != "[]",
        "negative control: {stands_for:?} must answer something, or the \
         equality below proves nothing"
    );
    assert_eq!(a.out, b.out, "{spelling:?} must answer what {stands_for:?} does");
    a.out
}

/// A script that must be refused with exit 2 and a message containing every
/// `needle`.
async fn refused(root: &Path, script: &str, needles: &[&str]) -> String {
    let r = run(root, script).await;
    assert_eq!(r.code, 2, "{script:?} must be a usage error, got {}: {}{}", r.code, r.out, r.err);
    for needle in needles {
        assert!(r.err.contains(needle), "{script:?}'s refusal must say {needle:?}: {}", r.err);
    }
    for misleading in ["Usage:", "try '--help'", "tip:"] {
        assert!(!r.err.contains(misleading), "clap's {misleading:?} leaked: {}", r.err);
    }
    r.err
}

// --- the spellings the bench runs refused ---------------------------------

#[tokio::test]
async fn branch_a_is_branch_all() {
    let (_f, root) = repo();
    let out = same(&root, "git branch -a --json", "git branch --all --json").await;
    assert!(out.contains("origin/main"), "-a includes the remote-tracking branch: {out}");
}

#[tokio::test]
async fn branch_r_is_branch_remote() {
    let (_f, root) = repo();
    let out = same(&root, "git branch -r --json", "git branch --remote --json").await;
    assert!(out.contains("origin/main"), "{out}");
}

#[tokio::test]
async fn branch_a_v_refuses_v_with_its_next_step() {
    let (_f, root) = repo();
    // The bench's exact words. `-a` is accepted now, so the refusal is about
    // `-v`, and it names what to type instead.
    refused(&root, "git branch -a -v", &["-v is not supported", "--ahead-behind", "git log -n 1"]).await;
}

#[tokio::test]
async fn log_oneline_prints_what_git_prints() {
    let (_f, root) = repo();
    let ours = run(&root, "git log --oneline").await;
    assert_eq!(ours.code, 0, "{}", ours.err);
    let theirs = git(&root, &["log", "--oneline", "--no-decorate"]);
    assert_eq!(ours.out.trim_end(), theirs.trim_end(), "--oneline is git's own line");
    assert_eq!(ours.out.lines().count(), 3, "negative control: three commits, three lines");
}

#[tokio::test]
async fn log_oneline_leaves_json_unchanged() {
    let (_f, root) = repo();
    same(&root, "git log --oneline --json", "git log --json").await;
}

#[tokio::test]
async fn log_on_a_branch_with_a_limit_and_oneline() {
    let (_f, root) = repo();
    // The bench's exact words, with this fixture's branch name.
    let r = run(&root, "git log main --limit 10 --oneline").await;
    assert_eq!(r.code, 0, "{}", r.err);
    assert_eq!(r.out.lines().count(), 3, "{}", r.out);
}

#[tokio::test]
async fn log_oneline_and_body_are_refused_together() {
    let (_f, root) = repo();
    refused(&root, "git log --oneline --body", &["--oneline", "--body"]).await;
}

#[tokio::test]
async fn log_p_is_log_patch() {
    let (_f, root) = repo();
    let p = run(&root, "git log -p").await;
    let patch = run(&root, "git log --patch").await;
    assert_ne!(patch.code, 0, "log --patch is refused in every build");
    assert_eq!((p.code, &p.err), (patch.code, &patch.err), "-p must be refused exactly as --patch is");
}

#[tokio::test]
async fn log_oneline_all_graph_decorate_says_what_is_not_supported() {
    let (_f, root) = repo();
    // The bench's exact words. `--oneline` is accepted, so the first refusal
    // is `--all`, with its next step.
    refused(&root, "git log --oneline --all --graph --decorate", &["--all is not supported", "git branch --all"]).await;
}

#[tokio::test]
async fn each_refused_log_flag_names_its_next_step() {
    let (_f, root) = repo();
    refused(&root, "git log --graph", &["--graph is not supported", "--first-parent"]).await;
    refused(&root, "git log --decorate", &["--decorate is not supported", "git tag"]).await;
    refused(&root, "git log --pretty=oneline", &["--pretty is not supported", "--oneline"]).await;
    refused(&root, "git log -n 1 --grep x", &["--grep is not supported", "--author"]).await;
}

#[tokio::test]
async fn an_unknown_flag_without_a_hint_lists_the_verbs_flags() {
    let (_f, root) = repo();
    let err = refused(&root, "git log --frobnicate", &["--frobnicate is not supported", "git log accepts"]).await;
    assert!(err.contains("--limit (-n)"), "{err}");
    assert!(!err.contains("-- --frobnicate"), "clap's advice would make the flag a path: {err}");
}

// --- verbatim binding, through the kernel -----------------------------------

/// `--json` is the kernel's: under verbatim binding it lifts the flag out of
/// the words and applies it itself, so the tool never sees it. What has to
/// hold is the observable half, that `git info --json` answers in JSON.
#[tokio::test]
async fn json_is_applied_by_the_kernel() {
    let (_f, root) = repo();
    let json = run(&root, "git info --json").await;
    assert_eq!(json.code, 0, "{}", json.err);
    let parsed: serde_json::Value = serde_json::from_str(&json.out).expect("--json answers in JSON");
    assert!(parsed.get("head").is_some(), "the info model: {parsed}");
    let text = run(&root, "git info").await;
    assert!(
        serde_json::from_str::<serde_json::Value>(&text.out).is_err(),
        "negative control: without --json the answer is text: {}",
        text.out
    );
}

/// `git log <rev> -- <path>` is git's spelling for "this revision's history of
/// this path". The marker has to reach the tool for the two operands to mean
/// two different things.
#[tokio::test]
async fn log_takes_a_revision_then_a_path_after_the_marker() {
    let (_f, root) = repo();
    let r = run(&root, "git log --oneline main -- src/lib.rs").await;
    assert_eq!(r.code, 0, "{}", r.err);
    assert_eq!(r.out.lines().count(), 2, "first and second touch src/lib.rs, third does not: {}", r.out);
}

// --- diff: git's operands ---------------------------------------------------

#[tokio::test]
async fn diff_cached_is_diff_staged() {
    let (_f, root) = repo();
    let out = same(&root, "git diff --cached --json", "git diff --staged --json").await;
    assert!(out.contains("docs/readme.md"), "{out}");
}

#[tokio::test]
async fn diff_of_two_revisions_is_from_to() {
    let (_f, root) = repo();
    let out = same(&root, "git diff 'HEAD~2' HEAD --json", "git diff --from 'HEAD~2' --to HEAD --json").await;
    assert!(out.contains("a.txt") && out.contains("src/lib.rs"), "{out}");
}

#[tokio::test]
async fn diff_of_a_range_is_from_to() {
    let (_f, root) = repo();
    same(&root, "git diff 'HEAD~2..HEAD' --json", "git diff --from 'HEAD~2' --to HEAD --json").await;
}

#[tokio::test]
async fn diff_of_one_revision_is_from() {
    let (_f, root) = repo();
    same(&root, "git diff 'HEAD~1' --json", "git diff --from 'HEAD~1' --json").await;
}

#[tokio::test]
async fn diff_of_an_open_range_starts_at_head() {
    let (_f, root) = repo();
    same(&root, "git diff '..HEAD~1' --json", "git diff --to 'HEAD~1' --json").await;
}

#[tokio::test]
async fn diff_matches_gits_paths_for_two_revisions() {
    let (_f, root) = repo();
    let ours = run(&root, "git diff 'HEAD~2' 'HEAD~1' --name-only").await;
    assert_eq!(ours.code, 0, "{}", ours.err);
    let theirs = git(&root, &["diff", "--name-only", "HEAD~2", "HEAD~1"]);
    for path in theirs.lines() {
        assert!(ours.out.contains(path), "git names {path}; ours: {}", ours.out);
    }
    assert!(!theirs.trim().is_empty(), "negative control: git saw changes");
}

#[tokio::test]
async fn diff_of_a_tracked_path_is_a_pathspec() {
    let (_f, root) = repo();
    let out = same(&root, "git diff src/lib.rs --json", "git diff --json -- src/lib.rs").await;
    assert!(out.contains("src/lib.rs"), "{out}");
}

#[tokio::test]
async fn diff_cached_with_a_path_is_staged_with_a_pathspec() {
    let (_f, root) = repo();
    same(&root, "git diff --cached docs/readme.md --json", "git diff --staged --json -- docs/readme.md").await;
}

#[tokio::test]
async fn diff_paths_are_relative_to_the_cwd() {
    let (_f, root) = repo();
    // From `src/`, `lib.rs` is `src/lib.rs` — as an operand, after `--`, and
    // as `--path`, the way git and this tool's `status` and `log` read it.
    let from_root = run(&root, "git diff --json -- src/lib.rs").await;
    assert_eq!(from_root.code, 0, "{}", from_root.err);
    assert!(from_root.out.contains("src/lib.rs"), "negative control: {}", from_root.out);
    for script in [
        "git diff lib.rs --json",
        "git diff --json -- lib.rs",
        "git diff --json --path lib.rs",
    ] {
        let r = run_in(&root, "/mnt/src", script).await;
        assert_eq!(r.code, 0, "{script}: {}", r.err);
        assert_eq!(r.out, from_root.out, "{script} must answer about src/lib.rs");
    }
}

#[tokio::test]
async fn a_word_that_is_both_a_revision_and_a_path_is_refused_like_git() {
    let (_f, root) = repo();
    let theirs = std::process::Command::new("git")
        .args(["diff", "docs"])
        .current_dir(&root)
        .output()
        .expect("run git");
    assert!(!theirs.status.success(), "oracle: git refuses the ambiguous word too");
    refused(&root, "git diff docs", &["both a revision and a tracked path", "'git diff docs --'"]).await;
    // `--` settles it: the word before it is the revision.
    same(&root, "git diff --json docs --", "git diff --from docs --json").await;
}

#[tokio::test]
async fn a_word_that_is_neither_is_refused() {
    let (_f, root) = repo();
    refused(&root, "git diff no-such-thing", &["'no-such-thing' is neither a revision nor a tracked path"]).await;
}

#[tokio::test]
async fn a_revision_after_a_path_is_refused_with_the_order_to_type() {
    let (_f, root) = repo();
    refused(&root, "git diff a.txt HEAD", &["'HEAD' is a revision after the path 'a.txt'", "'git diff HEAD -- a.txt'"]).await;
}

#[tokio::test]
async fn the_unsupported_operand_forms_are_refused_saying_why() {
    let (_f, root) = repo();
    refused(&root, "git diff 'HEAD~2' 'HEAD~1' HEAD", &["two revisions at most"]).await;
    refused(&root, "git diff 'HEAD~1...HEAD'", &["merge base", "'git diff \"HEAD~1\" HEAD'"]).await;
    refused(&root, "git diff --staged 'HEAD~1'", &["comparing a revision with the index is not supported"]).await;
    refused(&root, "git diff 'HEAD~1' --to HEAD", &["got revisions twice"]).await;
}

// --- every command the tool publishes parses ---------------------------------

/// Every `git …` command in `text` that sits inside the given delimiter.
fn commands_in(text: &str, delim: char, out: &mut Vec<String>) {
    let mut rest = text;
    while let Some(start) = rest.find(&format!("{delim}git ")) {
        let body = &rest[start + 1..];
        let Some(end) = body.find(delim) else { break };
        out.push(body[..end].to_string());
        rest = &body[end + 1..];
    }
}

/// Every backticked `git …` command in a schema tree's descriptions.
fn schema_commands(schema: &ToolSchema, out: &mut Vec<String>) {
    commands_in(&schema.description, '`', out);
    for param in &schema.params {
        commands_in(&param.description, '`', out);
    }
    for example in &schema.examples {
        out.push(example.code.clone());
    }
    for sub in &schema.subcommands {
        schema_commands(sub, out);
    }
}

/// A placeholder like `<commit>` stands for one word; kaish reads `<` as a
/// redirect, so it is replaced before parsing.
fn fill_placeholders(command: &str) -> String {
    let mut out = String::new();
    let mut rest = command;
    while let Some(start) = rest.find('<') {
        match rest[start..].find('>') {
            Some(len) if rest[start + 1..start + len].chars().all(|c| c.is_ascii_alphabetic() || c == '-') => {
                out.push_str(&rest[..start]);
                out.push_str("main");
                rest = &rest[start + len + 1..];
            }
            _ => {
                out.push_str(&rest[..=start]);
                rest = &rest[start + 1..];
            }
        }
    }
    out.push_str(rest);
    out
}

#[tokio::test]
async fn every_command_the_tool_publishes_parses_with_kaishs_parser() {
    let (_f, root) = repo();
    let mut commands = Vec::new();
    schema_commands(&tool().schema(), &mut commands);
    // The commands a refusal tells an agent to type next.
    for script in [
        "git branch -v",
        "git log --graph",
        "git log --decorate",
        "git log --all",
        "git log --pretty x",
        "git log --grep x",
        "git log --name-only",
        "git log --patch",
        "git status -s",
        "git status -b",
        "git status -u",
        "git tag -d",
        "git diff docs",
        "git diff a.txt HEAD",
        "git diff 'HEAD~1...HEAD'",
        "git diff --staged 'HEAD~1'",
        "git diff 'HEAD~1' --to HEAD",
        "git diff 'HEAD~2' 'HEAD~1' HEAD",
    ] {
        let r = run(&root, script).await;
        assert_ne!(r.code, 0, "{script:?} is a refusal trigger and must be refused");
        commands_in(&r.err, '\'', &mut commands);
    }

    // Negative controls: the harvest is not empty, and it reaches both an
    // example and a refusal hint.
    assert!(commands.len() >= 30, "too few commands harvested to mean anything: {commands:?}");
    assert!(commands.iter().any(|c| c == "git diff v0.1.0 HEAD -- src"), "an example: {commands:?}");
    assert!(commands.iter().any(|c| c.starts_with("git log -n 1 ")), "a hint: {commands:?}");

    let failures: Vec<String> = commands
        .iter()
        .map(|c| fill_placeholders(c))
        .filter_map(|c| kaish_kernel::parser::parse(&c).err().map(|e| format!("{c:?}: {e:?}")))
        .collect();
    assert!(
        failures.is_empty(),
        "commands this tool tells an agent to type do not parse in kaish — \
         quote any word with ~ or ^ in it (docs/issues.md K3):\n{}",
        failures.join("\n")
    );
}

/// K3's canary. kaish 0.17 refuses `~` inside an unquoted word, which is why
/// every published command quotes one. This goes red the day kaish parses
/// `HEAD~1` as one word: that is the signal to unquote the examples, hints and
/// argument docs once our consumers are on that kaish (docs/issues.md K3).
/// It is also the parse check's own negative control: a parser that accepted
/// everything would pass the test above vacuously.
#[test]
fn kaish_still_refuses_an_unquoted_tilde_in_a_word() {
    assert!(
        kaish_kernel::parser::parse("git log HEAD~1").is_err(),
        "kaish now parses HEAD~1 unquoted — K3 is fixed upstream. Unquote the \
         published commands once kaijutsu and kaibo are on this kaish, and \
         retire this canary (docs/issues.md K3)"
    );
    assert!(
        kaish_kernel::parser::parse("git log 'HEAD~1'").is_ok(),
        "the quoted form must parse, or every published command is broken"
    );
}

// --- from the 2026-10-11 review of this branch ------------------------------

/// git normalizes a pathspec: `./x`, `x/../y` and an absolute path inside the
/// repository all name the same file. Matched literally, each one matched
/// nothing and answered "clean" or "no commits" with exit 0.
#[tokio::test]
async fn a_pathspec_is_normalized_the_way_git_normalizes_it() {
    let (_f, root) = repo();
    same(&root, "git status --json ./src/lib.rs", "git status --json src/lib.rs").await;
    same(&root, "git status --json src/../src/lib.rs", "git status --json src/lib.rs").await;
    same(&root, "git status --json /mnt/src/lib.rs", "git status --json src/lib.rs").await;
    same(&root, "git log --json -- ./a.txt", "git log --json -- a.txt").await;
    same(&root, "git log --json --path ./a.txt", "git log --json -- a.txt").await;
    same(&root, "git diff --json ./src/lib.rs", "git diff --json src/lib.rs").await;

    // `..` from a subdirectory climbs to the root, as git's does.
    let up = run_in(&root, "/mnt/src", "git status --json ../docs").await;
    assert_eq!(up.code, 0, "{}", up.err);
    assert!(up.out.contains("docs/readme.md"), "{}", up.out);
}

#[tokio::test]
async fn a_pathspec_outside_the_repository_is_refused() {
    let (_f, root) = repo();
    refused(&root, "git status ../elsewhere", &["outside the repository"]).await;
    refused(&root, "git log -- /etc/passwd", &["outside the repository"]).await;
}

/// git sorts a word before `--` into a revision or a tracked path, and
/// refuses one that is both or neither. `log` now does what `diff` does.
#[tokio::test]
async fn log_reads_a_tracked_path_operand_as_a_path() {
    let (_f, root) = repo();
    same(&root, "git log --json a.txt", "git log --json -- a.txt").await;
    same(&root, "git log --json main a.txt", "git log --json main -- a.txt").await;
    refused(&root, "git log docs", &["git log docs --", "git log -- docs"]).await;
    refused(&root, "git log nosuchthing", &["nosuchthing"]).await;
}

#[tokio::test]
async fn an_operand_to_branch_or_tag_gets_that_verbs_next_step() {
    let (_f, root) = repo();
    let err = refused(&root, "git branch side", &["git log side"]).await;
    assert!(!err.contains("--repo"), "branch's operand is not a repository: {err}");
    let err = refused(&root, "git tag v1", &["git show v1"]).await;
    assert!(!err.contains("--repo"), "tag's operand is not a repository: {err}");
    // The control: info's operand is most likely a repository.
    refused(&root, "git info /mnt", &["--repo"]).await;
}

#[tokio::test]
async fn a_flag_before_the_verb_is_named_as_a_flag() {
    let (_f, root) = repo();
    refused(&root, "git --repo /mnt status", &["flags follow the verb"]).await;
}

#[tokio::test]
async fn a_bare_git_lists_the_verbs() {
    let (_f, root) = repo();
    let err = refused(&root, "git", &["status", "log"]).await;
    assert!(!err.contains("ToolArgs"), "an agent must not be told about the Rust API: {err}");
}

/// The binary screen is the one check on a word's type, now that the tool
/// binds verbatim. A placeholder token would read as a revision.
#[tokio::test]
async fn a_binary_word_is_refused() {
    let (_f, root) = repo();
    let backend: Arc<dyn KernelBackend> =
        Arc::new(StrictBackend::single(PathBuf::from("/mnt"), root.clone()));
    let mut ctx = support::TestCtx::new(backend, "/mnt");
    let mut args = support::words(&["log"]);
    args.words
        .as_mut()
        .expect("words")
        .push(kaish_types::Value::Bytes(vec![0xff, 0x00, 0xfe]));
    let r = tool().execute(args, &mut ctx).await;
    assert_eq!(r.code, 2, "{}", r.err);
    assert!(r.err.contains("binary"), "{}", r.err);
}
