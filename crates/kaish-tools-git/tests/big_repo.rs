//! Real repositories, at real scale — the gap every other fixture leaves.
//!
//! Every other integration test in this crate builds its repository with
//! `git init` and a handful of commits. Nothing in the suite has ever run
//! against packed refs, a commit-graph, thousands of commits, or thousands of
//! files, and those are exactly the shapes an agent's real checkout has. This
//! file is that check, with real `git` as the oracle exactly like the rest of
//! the suite (architecture.md B.2/B.3) — the only difference is the fixture
//! is a repository this crate did not build.
//!
//! **Gated on `KAISH_GIT_BIG_REPO`.** These tests run only when it names a
//! real checkout, e.g.:
//!
//! ```text
//! KAISH_GIT_BIG_REPO=$HOME/src/kaish \
//!   cargo test -p kaish-tools-git --test big_repo -- --nocapture
//! ```
//!
//! Unset, every test here prints why it is skipping (via [`big_repo`]) and
//! passes without asserting anything — visible with `--nocapture`, and never
//! a silent no-op. `--nocapture` also surfaces the timing line each test
//! prints, which is the point: a large repository's real wall-clock cost is
//! itself a finding, not just its correctness.
//!
//! **Read-only, always.** The repository named here is someone's real
//! checkout, not a fixture we own — every git call in this file is a read
//! (`log`, `status`, `rev-parse`, `show --numstat`), and none may write.
//! `support::git`'s hermetic-env discipline is reused for the oracle calls the
//! same way every other test file uses it, which is what keeps a developer's
//! `~/.gitconfig` (color, pager, format aliases) from changing what the
//! oracle reports.

#[path = "support.rs"]
mod support;

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Instant;

use kaish_tool_api::Tool;
use kaish_types::{ExecResult, ToolArgs, Value};

use kaish_tools_git::{GitConfig, Limits};

use support::{git, require_git, StrictBackend, TestCtx};

/// The VFS root the real repository is mounted at.
const MOUNT: &str = "/mnt";

/// The real checkout under test, or `None` after printing why there is
/// nothing to run.
///
/// Deliberately not a panic on a missing env var: these tests are opt-in, and
/// a developer running the ordinary `cargo test -p kaish-tools-git` must not
/// see failures for a repository they never named. A bad *value* (set but not
/// a real directory) still panics — that is a typo worth failing loud on, not
/// a silent skip.
fn big_repo() -> Option<PathBuf> {
    match std::env::var("KAISH_GIT_BIG_REPO") {
        Ok(path) if !path.trim().is_empty() => {
            require_git();
            let root = std::fs::canonicalize(&path).unwrap_or_else(|e| {
                panic!(
                    "KAISH_GIT_BIG_REPO={path:?} does not exist or is not readable: {e}"
                )
            });
            // A cheap, read-only sanity check that this is actually a git
            // repository, so a typo'd path fails here with a clear message
            // rather than deep inside `discover`.
            let dir = git(&root, &["rev-parse", "--git-dir"]);
            assert!(
                !dir.is_empty(),
                "KAISH_GIT_BIG_REPO={} does not look like a git repository \
                 (git rev-parse --git-dir returned nothing)",
                root.display()
            );
            Some(root)
        }
        _ => {
            eprintln!(
                "\n\
                 ┌─ SKIPPED: crates/kaish-tools-git/tests/big_repo.rs ─────────\n\
                 │ KAISH_GIT_BIG_REPO is not set, so this test has no real \n\
                 │ repository to run against and is passing without asserting \n\
                 │ anything. Point it at a real checkout and rerun with \n\
                 │ --nocapture to actually exercise it, e.g.:\n\
                 │\n\
                 │   KAISH_GIT_BIG_REPO=$HOME/src/kaish \\\n\
                 │     cargo test -p kaish-tools-git --test big_repo -- --nocapture\n\
                 └──────────────────────────────────────────────────────────────\n"
            );
            None
        }
    }
}

/// Build the `ToolArgs` the kernel would, from an argv slice — the same
/// shape `log.rs`/`status.rs` build, trimmed to what this file needs
/// (repeatable `--path`, single-value flags, bare flags).
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

/// Run a verb against `repo_root` mounted whole at `/mnt`, under `config`.
async fn run(config: GitConfig, repo_root: &Path, verb: &str, argv: &[&str]) -> ExecResult {
    let backend = Arc::new(StrictBackend::single(PathBuf::from(MOUNT), repo_root.to_path_buf()));
    let mut ctx = TestCtx::new(backend, MOUNT);
    let tool = kaish_tools_git::tool(config).expect("config");
    tool.execute(tool_args(verb, argv), &mut ctx).await
}

/// The typed model out of a successful result.
fn json(result: &ExecResult) -> serde_json::Value {
    assert_eq!(result.code, 0, "verb failed: {}", result.err);
    result
        .output()
        .and_then(|o| o.rich_json.clone())
        .expect("--json carries the typed model")
}

/// The full oids `log --json` reported, in order.
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

/// A config whose row cap is high enough that a real repository's `log`/
/// `status` cannot be truncated by the *embedder* default (1000) when this
/// file deliberately asks for more than that many rows. `--limit` on the CLI
/// can only lower an embedder's cap, never raise it (config.rs), so matching
/// git's unbounded answer on a real repository needs this on the embedder
/// side.
fn generous_config() -> GitConfig {
    GitConfig::read_only().with_limits(Limits {
        max_rows: 200_000,
        ..Limits::default()
    })
}

/// `git status --porcelain=v1` reduced to `(XY, path)` pairs, the same
/// reduction `status.rs`'s oracle uses (renames folded to the destination,
/// trailing `/` on an untracked dir dropped).
fn porcelain_set(repo_root: &Path) -> std::collections::BTreeSet<(String, String)> {
    let out = git(repo_root, &["status", "--porcelain=v1"]);
    let mut set = std::collections::BTreeSet::new();
    for line in out.lines() {
        if line.len() < 3 {
            continue;
        }
        let xy = line[..2].to_string();
        let rest = &line[3..];
        let path = match rest.split_once(" -> ") {
            Some((_orig, new)) => new,
            None => rest,
        };
        let path = path.trim_matches('"').trim_end_matches('/');
        set.insert((xy, path.to_string()));
    }
    set
}

/// Our rendered status rows as the same `(XY, path)` pairs.
fn our_status_set(result: &ExecResult) -> std::collections::BTreeSet<(String, String)> {
    result
        .output()
        .expect("output")
        .root
        .iter()
        .map(|node| {
            let path = node.cells.first().cloned().unwrap_or_default();
            let path = path.split(" ← ").next().unwrap_or(&path).to_string();
            (node.name.clone(), path)
        })
        .collect()
}

// ═══════════════════════════════════════════════════════════════════════════
// git log
// ═══════════════════════════════════════════════════════════════════════════

/// The oid sequence at several `--limit` values matches
/// `git log --format=%H -n N` exactly.
///
/// Confirmed failing against `KAISH_GIT_BIG_REPO=$HOME/src/kaish` at `--limit
/// 200` (2026-08-21, HEAD then): two merge commits, `f8c1b508` and
/// `3a74e505`, unrelated to each other (`git merge-base --is-ancestor` is
/// false in both directions) share the exact same committer instant
/// (`%cI`/`%cd` agree to the second). Git's walk orders them one way; ours —
/// a `BinaryHeap` keyed on committer time alone (log.rs) — has no secondary
/// tiebreaker, so ties resolve however the heap's internal structure happens
/// to pop them, not necessarily git's order. This is a real, if narrow,
/// divergence: any repository with two same-second commits neither of which
/// is the other's ancestor (fast scripted merges, sub-second-granularity
/// clocks, CI-driven series) can hit it. Left failing rather than pinned to
/// this specific pair, since the pair depends on the repository named at
/// `KAISH_GIT_BIG_REPO` and its history at the time it is run.
#[tokio::test]
async fn log_oid_sequence_matches_git_at_several_limits() {
    let Some(root) = big_repo() else { return };
    let t0 = Instant::now();

    for limit in [1usize, 5, 25, 200, 1000] {
        let result = run(
            GitConfig::read_only(),
            &root,
            "log",
            &["--limit", &limit.to_string()],
        )
        .await;
        assert_eq!(result.code, 0, "log --limit {limit} failed: {}", result.err);
        let ours = oids(&result);
        let theirs = git_oids(&root, &["-n", &limit.to_string()]);
        assert_eq!(
            ours, theirs,
            "log --limit {limit} vs `git log --format=%H -n {limit}` on {}",
            root.display()
        );
    }

    eprintln!(
        "big_repo timing: log at limits [1,5,25,200,1000] on {} took {:?}",
        root.display(),
        t0.elapsed()
    );
}

/// `--since`/`--until` over a real date window, split at the repository's own
/// median and quartile commit dates rather than a guessed range — so this
/// works on whatever history `KAISH_GIT_BIG_REPO` names, however old or new
/// it is.
#[tokio::test]
async fn date_window_filters_match_git_on_a_real_repo() {
    let Some(root) = big_repo() else { return };
    let t0 = Instant::now();

    // Newest-first, like git log's default order.
    let all_dates: Vec<String> = git(&root, &["log", "--format=%cI"])
        .lines()
        .map(str::to_string)
        .collect();
    assert!(!all_dates.is_empty(), "the big repo must have at least one commit");
    if all_dates.len() < 4 {
        eprintln!(
            "big_repo: {} has only {} commits, too few for a meaningful quartile \
             date window; skipping the date-window assertion",
            root.display(),
            all_dates.len()
        );
        return;
    }

    // Index 0 is newest. A newer (smaller index) quartile for `--until`, an
    // older (larger index) quartile for `--since` — `since <= until` in time.
    let until_date = &all_dates[all_dates.len() / 4];
    let since_date = &all_dates[all_dates.len() * 3 / 4];

    let config = generous_config();
    let result = run(
        config,
        &root,
        "log",
        &["--since", since_date, "--until", until_date, "--limit", "200000"],
    )
    .await;
    assert_eq!(result.code, 0, "date window failed: {}", result.err);
    let ours = oids(&result);
    let theirs = git_oids(&root, &["--since", since_date, "--until", until_date]);
    assert_eq!(
        ours, theirs,
        "--since {since_date} --until {until_date} on {}",
        root.display()
    );
    assert!(!theirs.is_empty(), "the quartile window must select something real");

    eprintln!(
        "big_repo timing: date window [{since_date}..{until_date}] on {} ({} commits) took {:?}",
        root.display(),
        theirs.len(),
        t0.elapsed()
    );
}

/// `--path <a real directory>` against `git log --format=%H -- <dir>` —
/// the history-simplification walk (a custom commit-time heap, per B.3),
/// which every other fixture in the suite has only run against a 6-commit
/// history. A real repository's merges are what this is actually for.
#[tokio::test]
async fn path_filter_matches_git_history_simplification_on_a_real_repo() {
    let Some(root) = big_repo() else { return };
    let t0 = Instant::now();

    let dir = git(&root, &["ls-tree", "-d", "--name-only", "HEAD"])
        .lines()
        .next()
        .map(str::to_string);
    let Some(dir) = dir else {
        eprintln!(
            "big_repo: {} has no top-level directory to filter on; skipping",
            root.display()
        );
        return;
    };

    let config = generous_config();
    let result = run(config, &root, "log", &["--path", &dir, "--limit", "200000"]).await;
    assert_eq!(result.code, 0, "log --path {dir} failed: {}", result.err);
    let ours = oids(&result);
    let theirs = git_oids(&root, &["--", &dir]);
    assert_eq!(ours, theirs, "--path {dir} on {}", root.display());

    eprintln!(
        "big_repo timing: --path {dir} on {} ({} commits) took {:?}",
        root.display(),
        theirs.len(),
        t0.elapsed()
    );
}

/// `--stat` against `git show --numstat` for a bounded sample of commits —
/// bounded because summing numstat lines shells out once per commit, and a
/// real history can have thousands.
///
/// Confirmed failing (2026-08-21) against all three candidate repositories
/// this file's module doc names, in *both* directions — not a one-way
/// off-by-one (docs/issues.md L6):
///
/// | `KAISH_GIT_BIG_REPO` | commit | we report | git sums to |
/// |---|---|---|---|
/// | `kaish` | `264bc8431f9e...` | 178 additions | 177 |
/// | `kaibo` | `f06d86cee8d0...` | 113 additions | 117 |
/// | `kaish-extras` | `4cc0d1604f7b...` | 829 additions | 826 |
///
/// Every file in every case ends with a real trailing newline on both sides
/// of its commit (not the missing-final-newline edge case).
/// `gix-imara-diff`'s Myers implementation and git's own do not always
/// produce the same edit script when a hunk admits more than one minimal
/// alignment (repeated or blank lines nearby, common in real code) — the
/// *edit distance* can match while the *split* between which lines count as
/// added differs either way. Not isolated to one specific file or line here;
/// that is real follow-up work, not something this fixture pins further.
/// Docs/issues.md L1/L3 already document `--stat` limitations under this
/// dependency set; L6 is a new one in the same family, found only because
/// real, organically-grown diffs exercised it.
#[tokio::test]
async fn stat_matches_git_numstat_on_a_sample_of_commits() {
    let Some(root) = big_repo() else { return };
    let t0 = Instant::now();

    const SAMPLE: usize = 25;
    let result = run(
        GitConfig::read_only(),
        &root,
        "log",
        &["--limit", &SAMPLE.to_string(), "--stat", "--no-merges"],
    )
    .await;
    assert_eq!(result.code, 0, "log --stat failed: {}", result.err);
    let commits = json(&result);
    let commits = commits["commits"].as_array().expect("commits");
    assert!(!commits.is_empty(), "the sample must contain at least one commit");

    for commit in commits {
        let oid = commit["oid"].as_str().expect("oid");
        let stat = &commit["stat"];

        let numstat = git(
            &root,
            &["show", "--numstat", "--format=", "--no-renames", oid],
        );
        let (mut files, mut added, mut deleted) = (0u64, 0u64, 0u64);
        for line in numstat.lines().filter(|l| !l.trim().is_empty()) {
            let mut parts = line.split('\t');
            let a = parts.next().unwrap_or("0");
            let d = parts.next().unwrap_or("0");
            files += 1;
            added += a.parse::<u64>().unwrap_or(0);
            deleted += d.parse::<u64>().unwrap_or(0);
        }

        assert_eq!(stat["files"], files, "file count for {oid}: {numstat:?}");
        assert_eq!(stat["additions"], added, "additions for {oid}: {numstat:?}");
        assert_eq!(stat["deletions"], deleted, "deletions for {oid}: {numstat:?}");
    }

    eprintln!(
        "big_repo timing: --stat over {} commits on {} took {:?}",
        commits.len(),
        root.display(),
        t0.elapsed()
    );
}

/// The L2 bound (docs/issues.md): a filter matched against nothing walks the
/// whole reachable history (bounded by `MAX_COMMITS_EXAMINED = 100_000`) and
/// completes rather than hanging. For a history smaller than that cap — every
/// realistic candidate repository — the walk finishes on its own and reports
/// `truncated: false`, an honest "nothing matched" rather than a truncated
/// guess; only a history at or past the cap would see `truncated: true`. This
/// asserts whichever is actually true for the repository named, rather than
/// assuming one, and asserts the wall clock stays bounded either way.
#[tokio::test]
async fn an_unmatched_filter_is_bounded_not_hung() {
    let Some(root) = big_repo() else { return };

    let total: usize = git(&root, &["rev-list", "--count", "HEAD"])
        .trim()
        .parse()
        .expect("rev-list --count is an integer");

    let t0 = Instant::now();
    let result = run(
        GitConfig::read_only(),
        &root,
        "log",
        &[
            "--author",
            "kaish-tools-git-big-repo-test-nobody-authored-this-2c9e1f",
        ],
    )
    .await;
    let elapsed = t0.elapsed();

    assert_eq!(result.code, 0, "an unmatched filter is still a success: {}", result.err);
    let j = json(&result);
    assert_eq!(
        j["commits"].as_array().expect("commits").len(),
        0,
        "the fabricated author string must match nothing real: {j}"
    );

    const MAX_COMMITS_EXAMINED: usize = 100_000;
    let expected_truncated = total > MAX_COMMITS_EXAMINED;
    assert_eq!(
        j["truncated"], expected_truncated,
        "with {total} reachable commits against a cap of {MAX_COMMITS_EXAMINED}, \
         truncated should be {expected_truncated}: {j}"
    );

    // Generous but real: a walk over a repository with a five-figure commit
    // count should not take minutes. If this ever fires, that is the finding.
    assert!(
        elapsed.as_secs() < 30,
        "an unmatched filter over {total} commits took {elapsed:?} — that is a \
         real performance finding, not a flaky timeout"
    );

    eprintln!(
        "big_repo timing: unmatched --author filter over {total} reachable commits \
         on {} took {:?} (truncated={expected_truncated})",
        root.display(),
        elapsed
    );
}

// ═══════════════════════════════════════════════════════════════════════════
// git status
// ═══════════════════════════════════════════════════════════════════════════

/// Our status porcelain set matches `git status --porcelain=v1` on the real
/// checkout, dirty or clean, whatever state it happens to be in — read-only,
/// so this never changes what it observes.
///
/// Confirmed failing (2026-08-21) against all three candidate repositories:
/// we report paths as `??` that git reports nothing for at all. `kaish` had
/// an empty untracked directory (`crates/kaish-vfs/tests`); `kaish`, `kaibo`
/// and `kaish-extras` all independently have a `.crush/` (a tool cache dir)
/// whose own nested `.gitignore` ignores everything inside it, including
/// itself. Both classes are real, minimized to `status.rs`'s `c7_*`/`c8_*`
/// fixtures (docs/issues.md C7, C8) rather than pinned to these exact
/// repository-relative paths, which are someone's real working tree and can
/// change.
#[tokio::test]
async fn status_matches_git_porcelain_on_a_real_checkout() {
    let Some(root) = big_repo() else { return };
    let t0 = Instant::now();

    let config = generous_config();
    let result = run(config, &root, "status", &["--limit", "200000", "--json"]).await;
    assert_eq!(result.code, 0, "status failed: {}", result.err);

    let ours = our_status_set(&result);
    let theirs = porcelain_set(&root);
    assert_eq!(
        ours, theirs,
        "our porcelain disagrees with `git status --porcelain=v1` on {}",
        root.display()
    );

    eprintln!(
        "big_repo timing: status over {} entries on {} took {:?}",
        theirs.len(),
        root.display(),
        t0.elapsed()
    );
}

// ═══════════════════════════════════════════════════════════════════════════
// git info
// ═══════════════════════════════════════════════════════════════════════════

/// `git info` returns sane, git-agreeing values on a real repository: the
/// reported HEAD oid matches `git rev-parse HEAD`, the repository is not
/// mistakenly flagged bare or shallow, and every declared gix pin round-trips.
#[tokio::test]
async fn info_reports_sane_values_on_a_real_repo() {
    let Some(root) = big_repo() else { return };
    let t0 = Instant::now();

    let result = run(GitConfig::read_only(), &root, "info", &[]).await;
    assert_eq!(result.code, 0, "info failed: {}", result.err);
    let j = json(&result);

    let real_head = git(&root, &["rev-parse", "HEAD"]);
    assert_eq!(j["head"]["oid"], real_head, "{j}");
    assert_eq!(j["bare"], false, "a checkout with a working tree is not bare: {j}");

    let is_shallow = git(&root, &["rev-parse", "--is-shallow-repository"]);
    assert_eq!(
        j["shallow"],
        is_shallow == "true",
        "shallow disagrees with `git rev-parse --is-shallow-repository`: {j}"
    );

    assert!(j["worktrees"].as_u64().is_some(), "{j}");
    assert!(j["submodules"].as_u64().is_some(), "{j}");
    assert!(j["ref_backend"].is_string(), "{j}");

    eprintln!("big_repo timing: info on {} took {:?}", root.display(), t0.elapsed());
}
