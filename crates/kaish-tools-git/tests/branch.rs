//! `git branch` against real git's `--format` output (architecture.md B.7).
//!
//! The oracles: `git branch --format='%(refname:short)|%(objectname)|
//! %(upstream)'` for the listing, `git branch --contains` / `--merged` for the
//! filters, and `git rev-list --left-right --count <local>...<upstream>` for
//! the counts.
//!
//! The gate this verb carries is `--ahead-behind`'s **opt-in cost** (H, PR 7).
//! It is asserted, not described: `commits_examined` is 0 for a plain
//! listing, positive with the flag, and lower under `--limit` — which is only
//! possible if the row cap runs before the counting rather than after it.

#[path = "support.rs"]
mod support;

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::Arc;

use kaish_tool_api::Tool;
use kaish_types::{ExecResult, ToolArgs, Value};

use kaish_tools_git::GitConfig;

use support::{git, git_as, require_git, write_file, Fixture, RefsRepo, StrictBackend, TestCtx};

/// Run `git branch` with `mount_real` mounted at `/mnt` and the caller in
/// `cwd`.
async fn run_at(mount_real: &std::path::Path, cwd: &str, argv: &[&str]) -> ExecResult {
    let backend = Arc::new(StrictBackend::single(
        PathBuf::from("/mnt"),
        mount_real.to_path_buf(),
    ));
    let mut ctx = TestCtx::new(backend, cwd);
    let tool = kaish_tools_git::tool(GitConfig::read_only()).expect("config");
    let mut args = ToolArgs::new();
    args.positional.push(Value::String("branch".to_string()));
    let mut i = 0;
    while i < argv.len() {
        match argv[i].strip_prefix("--") {
            Some(name @ ("json" | "all" | "remote" | "ahead-behind")) => {
                args.flags.insert(name.to_string());
                i += 1;
            }
            Some(name) => {
                args.named
                    .insert(name.to_string(), Value::String(argv[i + 1].to_string()));
                i += 2;
            }
            None => {
                args.positional.push(Value::String(argv[i].to_string()));
                i += 1;
            }
        }
    }
    tool.execute(args, &mut ctx).await
}

async fn run(repo: &RefsRepo, argv: &[&str]) -> ExecResult {
    run_at(&repo.scratch(), "/mnt/repo", argv).await
}

fn json(result: &ExecResult) -> serde_json::Value {
    assert_eq!(result.code, 0, "failed: {}", result.err);
    result
        .output()
        .and_then(|o| o.rich_json.clone())
        .expect("--json carries the typed model")
}

fn names(model: &serde_json::Value) -> Vec<String> {
    model["branches"]
        .as_array()
        .expect("branches array")
        .iter()
        .map(|b| b["name"].as_str().expect("name").to_string())
        .collect()
}

fn rows_by_name(model: &serde_json::Value) -> BTreeMap<String, serde_json::Value> {
    model["branches"]
        .as_array()
        .expect("branches array")
        .iter()
        .map(|b| (b["name"].as_str().expect("name").to_string(), b.clone()))
        .collect()
}

/// The local listing, against `git branch --format`: same names in the same
/// order, same oids, same upstreams, same `*` marker.
#[tokio::test]
async fn the_local_listing_matches_real_gits_format() {
    let repo = RefsRepo::build();
    let model = json(&run(&repo, &["--json"]).await);

    let oracle: Vec<Vec<String>> = repo
        .git(&["branch", "--format=%(refname:short)|%(objectname)|%(upstream)|%(HEAD)"])
        .lines()
        .map(|line| line.split('|').map(str::to_string).collect())
        .collect();
    assert!(oracle.len() >= 4, "the fixture must have branches: {oracle:?}");

    let ours = model["branches"].as_array().expect("array");
    assert_eq!(
        ours.len(),
        oracle.len(),
        "one row per branch: ours {:?}",
        names(&model)
    );
    for (ours, theirs) in ours.iter().zip(oracle.iter()) {
        let name = &theirs[0];
        assert_eq!(ours["name"].as_str(), Some(name.as_str()), "name order");
        assert_eq!(ours["oid"].as_str(), Some(theirs[1].as_str()), "oid of {name}");
        assert_eq!(ours["kind"], "local", "kind of {name}");
        let upstream = theirs[2]
            .strip_prefix("refs/remotes/")
            .or_else(|| theirs[2].strip_prefix("refs/heads/"))
            .map(str::to_string);
        assert_eq!(
            ours["upstream"].as_str().map(str::to_string),
            upstream,
            "upstream of {name}"
        );
        assert_eq!(
            ours["is_head"].as_bool(),
            Some(theirs[3] == "*"),
            "HEAD marker on {name}"
        );
    }
}

/// `--remote` and `--all` select the namespaces git selects.
#[tokio::test]
async fn remote_and_all_match_real_git() {
    let repo = RefsRepo::build();

    let remote = names(&json(&run(&repo, &["--json", "--remote"]).await));
    assert!(
        remote.iter().all(|n| n.starts_with("origin/")),
        "--remote lists only remote-tracking branches: {remote:?}"
    );
    assert!(remote.contains(&"origin/main".to_string()), "{remote:?}");

    let all = names(&json(&run(&repo, &["--json", "--all"]).await));
    let local = names(&json(&run(&repo, &["--json"]).await));
    assert_eq!(
        all.len(),
        local.len() + remote.len(),
        "--all is exactly the two namespaces: {all:?}"
    );
    // Local before remote, which is full-refname order.
    assert_eq!(&all[..local.len()], &local[..], "{all:?}");

    // Negative control: the default listing has neither the remote-tracking
    // branches nor a remote kind on any row.
    assert!(!local.iter().any(|n| n.starts_with("origin/")), "{local:?}");
    for row in json(&run(&repo, &["--json", "--remote"]).await)["branches"]
        .as_array()
        .expect("array")
    {
        assert_eq!(row["kind"], "remote", "{row}");
        assert!(
            row["upstream"].is_null(),
            "a remote-tracking branch has no upstream of its own: {row}"
        );
    }
}

/// A `refs/remotes/<r>/HEAD` symbolic ref is listed under the name its
/// namespace gives it. **Divergence, pinned (docs/issues.md B2):** git's
/// `%(refname:short)` shortens that one ref to the bare remote name.
#[tokio::test]
async fn a_symbolic_remote_head_is_named_by_its_ref_not_by_its_remote() {
    let repo = RefsRepo::build();
    let rows = rows_by_name(&json(&run(&repo, &["--json", "--remote"]).await));

    let ours = rows.get("origin/HEAD").unwrap_or_else(|| {
        panic!("we name it origin/HEAD: {:?}", rows.keys().collect::<Vec<_>>())
    });
    assert_eq!(
        ours["oid"], repo.d,
        "and it resolves through the symbolic chain: {ours}"
    );

    // Git's own answer, asserted rather than described.
    let theirs = repo.git(&["branch", "-r", "--format=%(refname)|%(refname:short)"]);
    assert!(
        theirs.contains("refs/remotes/origin/HEAD|origin"),
        "git shortens refs/remotes/origin/HEAD to the bare remote name: {theirs}"
    );
    assert!(
        !rows.contains_key("origin"),
        "we do not, because 'origin' reads like a branch called origin: {:?}",
        rows.keys().collect::<Vec<_>>()
    );
}

/// `--contains` and `--merged` match git, and each has the branch it must
/// leave out.
#[tokio::test]
async fn contains_and_merged_match_real_git() {
    let repo = RefsRepo::build();

    let ours = names(&json(&run(&repo, &["--json", "--contains", &repo.b]).await));
    let theirs: Vec<String> = repo
        .git(&["branch", "--contains", &repo.b, "--format=%(refname:short)"])
        .lines()
        .map(str::to_string)
        .collect();
    assert_eq!(ours, theirs, "--contains B");
    assert!(!ours.contains(&"old".to_string()), "old is at A: {ours:?}");
    assert!(ours.contains(&"main".to_string()), "{ours:?}");

    let ours = names(&json(&run(&repo, &["--json", "--merged", "main"]).await));
    let theirs: Vec<String> = repo
        .git(&["branch", "--merged", "main", "--format=%(refname:short)"])
        .lines()
        .map(str::to_string)
        .collect();
    assert_eq!(ours, theirs, "--merged main");
    assert!(
        !ours.contains(&"feature/side".to_string()),
        "the side branch is not in main: {ours:?}"
    );
    assert!(ours.contains(&"old".to_string()), "{ours:?}");
}

/// The counts match `git rev-list --left-right --count`, and they are `null`
/// unless asked for.
#[tokio::test]
async fn ahead_behind_matches_real_git_and_is_opt_in() {
    let repo = RefsRepo::build();

    // Off by default, and the report says so rather than leaving a caller to
    // guess what a null means.
    let plain = json(&run(&repo, &["--json"]).await);
    assert_eq!(plain["ahead_behind"], false);
    for row in plain["branches"].as_array().expect("array") {
        assert!(row["ahead"].is_null(), "{row}");
        assert!(row["behind"].is_null(), "{row}");
    }

    let model = json(&run(&repo, &["--json", "--ahead-behind"]).await);
    assert_eq!(model["ahead_behind"], true);
    let rows = rows_by_name(&model);

    for branch in ["main", "old"] {
        let counts = repo.git(&[
            "rev-list",
            "--left-right",
            "--count",
            &format!("{branch}...origin/main"),
        ]);
        let mut parts = counts.split_whitespace();
        let ahead: u64 = parts.next().expect("ahead").parse().expect("a number");
        let behind: u64 = parts.next().expect("behind").parse().expect("a number");
        assert_eq!(rows[branch]["ahead"], ahead, "ahead of {branch}");
        assert_eq!(rows[branch]["behind"], behind, "behind of {branch}");
    }
    // The two branches must not have the same answer, or the comparison above
    // would pass over an implementation that returned a constant.
    assert_ne!(rows["main"]["ahead"], rows["old"]["ahead"]);

    // A branch with no upstream has nothing to count against.
    let side = &rows["feature/side"];
    assert!(side["upstream"].is_null(), "{side}");
    assert!(side["ahead"].is_null(), "{side}");

    // A branch whose upstream is configured but absent is `[gone]` to git, and
    // the row says which so the null is not ambiguous.
    let gone = &rows["gone"];
    assert_eq!(gone["upstream"], "origin/nonesuch");
    assert_eq!(gone["upstream_gone"], true);
    assert!(gone["ahead"].is_null(), "{gone}");
    assert!(
        repo.git(&["branch", "--format=%(refname:short)|%(upstream:track)"])
            .contains("gone|[gone]"),
        "git calls it gone too"
    );
}

/// **The PR 7 gate.** `--ahead-behind` costs commits, and `--limit` bounds how
/// many times that cost is paid — because the row cap runs *before* the
/// counting, not after it.
///
/// A cap applied after the work is not a cap; docs/issues.md G7-G10 records
/// that shape three times over. This is the assertion that would fail if the
/// counting moved above the truncation.
#[tokio::test]
async fn the_limit_bounds_the_ahead_behind_cost_because_it_runs_first() {
    let repo = RefsRepo::build();

    let plain = json(&run(&repo, &["--json"]).await);
    assert_eq!(
        plain["commits_examined"], 0,
        "a listing with no ancestry question reads no commit: {plain}"
    );

    let full = json(&run(&repo, &["--json", "--ahead-behind"]).await);
    let full_cost = full["commits_examined"].as_u64().expect("a count");
    assert!(full_cost > 0, "counting reads commits: {full}");
    let counted = full["branches"]
        .as_array()
        .expect("array")
        .iter()
        .filter(|b| !b["ahead"].is_null())
        .count();
    assert!(counted >= 2, "more than one branch has an upstream: {full}");

    // The same flags with the listing cut short. Fewer rows must mean fewer
    // commits read.
    let capped = json(&run(&repo, &["--json", "--ahead-behind", "--limit", "2"]).await);
    let capped_cost = capped["commits_examined"].as_u64().expect("a count");
    assert_eq!(capped["truncated"], true);
    assert!(
        capped_cost < full_cost,
        "--limit must bound the counting, not just the rows: capped \
         {capped_cost} vs full {full_cost}"
    );

    // Negative control on the other half: `--contains` is a FILTER, so it has
    // to judge every candidate before truncation. Its cost must NOT fall when
    // the rows are capped, or the filter would be judging only the survivors.
    let filtered = json(&run(&repo, &["--json", "--contains", &repo.a]).await);
    let filtered_capped =
        json(&run(&repo, &["--json", "--contains", &repo.a, "--limit", "1"]).await);
    assert_eq!(
        filtered["commits_examined"], filtered_capped["commits_examined"],
        "a filter is evaluated for every branch, cap or no cap"
    );
}

/// `--limit` bounds the rows and says so.
#[tokio::test]
async fn limit_truncates_and_reports() {
    let repo = RefsRepo::build();
    let result = run(&repo, &["--json", "--limit", "2"]).await;
    let model = json(&result);
    assert_eq!(model["branches"].as_array().expect("array").len(), 2);
    assert_eq!(model["truncated"], true);
    assert!(result.err.contains("--limit"), "{}", result.err);

    let full = run(&repo, &["--json"]).await;
    assert_eq!(json(&full)["truncated"], false);
    assert!(full.err.is_empty(), "stderr: {}", full.err);
}

/// A revision that does not resolve is a git-level failure naming it, not an
/// empty listing that reads like "no branch matches".
#[tokio::test]
async fn an_unresolvable_filter_revision_is_refused() {
    let repo = RefsRepo::build();
    for flag in ["--contains", "--merged"] {
        let result = run(&repo, &["--json", flag, "no-such-rev"]).await;
        assert_eq!(result.code, 1, "{flag}: {}", result.err);
        assert!(result.err.contains("no-such-rev"), "{flag}: {}", result.err);
    }
}

/// The verb takes no operands rather than answering a different question.
#[tokio::test]
async fn operands_are_refused() {
    let repo = RefsRepo::build();
    let result = run(&repo, &["main"]).await;
    assert_eq!(result.code, 2, "stderr: {}", result.err);
    assert!(result.err.contains("main"), "{}", result.err);
}

/// A repository with a committer clock that runs backwards, so the merge base
/// is *newer* than both tips.
fn skewed_repo() -> Fixture {
    require_git();
    let fixture = Fixture::empty();
    let root = fixture.path("repo");
    std::fs::create_dir_all(&root).expect("create repo");

    let who = ("Fixture Author", "author@example.invalid");
    git(&root, &["init", "--initial-branch=main", "--quiet"]);
    // The base, stamped far in the future relative to what follows.
    write_file(&root, "base.txt", "base\n");
    git(&root, &["add", "base.txt"]);
    git_as(&root, who, "2026-08-01T10:00:00+00:00", &["commit", "-m", "base", "--quiet"]);
    let base = git(&root, &["rev-parse", "HEAD"]);

    // One commit on each side, both older than the base they descend from.
    write_file(&root, "local.txt", "local\n");
    git(&root, &["add", "local.txt"]);
    git_as(&root, who, "2026-01-01T00:00:00+00:00", &["commit", "-m", "local", "--quiet"]);

    git(&root, &["checkout", "--quiet", "-b", "other", &base]);
    write_file(&root, "other.txt", "other\n");
    git(&root, &["add", "other.txt"]);
    git_as(&root, who, "2026-01-02T00:00:00+00:00", &["commit", "-m", "other", "--quiet"]);
    let other = git(&root, &["rev-parse", "HEAD"]);
    git(&root, &["checkout", "--quiet", "main"]);
    git(&root, &["branch", "-D", "other"]);

    git(&root, &["update-ref", "refs/remotes/origin/main", &other]);
    git(&root, &["config", "remote.origin.url", "../nowhere.git"]);
    git(
        &root,
        &["config", "remote.origin.fetch", "+refs/heads/*:refs/remotes/origin/*"],
    );
    git(&root, &["config", "branch.main.remote", "origin"]);
    git(&root, &["config", "branch.main.merge", "refs/heads/main"]);

    fixture
}

/// A committer clock that runs backwards does not change the counts.
///
/// This is the assertion behind `reach.rs`'s design note. The cheap version of
/// `--ahead-behind` pops commits newest-first and stops as soon as everything
/// queued is common history, which is only sound if committer time increases
/// from parent to child. It does not have to. Here the merge base is stamped
/// seven months *after* both tips, and an order-dependent walk reports
/// `behind 2` — the base counted as a commit only the upstream has.
///
/// The walk we ship reads both histories and counts from the flags it
/// finishes with, so no clock can move the answer. That costs the full
/// histories, which is the trade docs/issues.md B1 records.
#[tokio::test]
async fn a_backwards_committer_clock_does_not_move_the_counts() {
    let fixture = skewed_repo();
    let root = fixture.path("repo");

    let counts = git(&root, &["rev-list", "--left-right", "--count", "main...origin/main"]);
    let mut parts = counts.split_whitespace();
    let git_ahead: u64 = parts.next().expect("ahead").parse().expect("a number");
    let git_behind: u64 = parts.next().expect("behind").parse().expect("a number");
    assert_eq!(
        (git_ahead, git_behind),
        (1, 1),
        "git counts the one commit on each side of the base"
    );

    let model = json(&run_at(&fixture.root(), "/mnt/repo", &["--json", "--ahead-behind"]).await);
    let rows = rows_by_name(&model);
    let main = &rows["main"];
    assert_eq!(main["ahead"], git_ahead, "{main}");
    assert_eq!(main["behind"], git_behind, "{main}");

    // The control that gives the fixture its teeth: the base really is newer
    // than the tips, so an order-dependent walk really would pop it first.
    let base_time = git(&root, &["log", "-1", "--format=%ct", "main~1"]);
    let tip_time = git(&root, &["log", "-1", "--format=%ct", "main"]);
    assert!(
        base_time.parse::<i64>().expect("a timestamp")
            > tip_time.parse::<i64>().expect("a timestamp"),
        "the merge base must be stamped after its own child: base {base_time} \
         vs tip {tip_time}"
    );
}

/// The same property for the other clock pathology: every commit sharing one
/// instant, which is what a scripted import or a fixture produces.
///
/// `RefsRepo` pins one committer date across its whole history, so the walk
/// has no time signal to order by at all. An order-dependent walk reported
/// `behind 2` here where git reports 1; this is the regression test for that.
#[tokio::test]
async fn a_history_with_one_committer_instant_still_counts_correctly() {
    let repo = RefsRepo::build();

    // The control: the commits really do share an instant.
    let times = repo.git(&["log", "--format=%ct", "--all"]);
    let distinct: std::collections::BTreeSet<&str> = times.lines().collect();
    assert_eq!(
        distinct.len(),
        1,
        "the fixture must pin one committer instant, or this proves nothing: \
         {distinct:?}"
    );

    let rows = rows_by_name(&json(&run(&repo, &["--json", "--ahead-behind"]).await));
    for branch in ["main", "old"] {
        let counts = repo.git(&[
            "rev-list",
            "--left-right",
            "--count",
            &format!("{branch}...origin/main"),
        ]);
        let mut parts = counts.split_whitespace();
        let ahead: u64 = parts.next().expect("ahead").parse().expect("a number");
        let behind: u64 = parts.next().expect("behind").parse().expect("a number");
        assert_eq!(rows[branch]["ahead"], ahead, "ahead of {branch}");
        assert_eq!(rows[branch]["behind"], behind, "behind of {branch}");
    }
}
