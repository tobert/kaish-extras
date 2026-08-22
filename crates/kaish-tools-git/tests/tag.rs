//! `git tag` against real git's `--format` output (architecture.md B.7).
//!
//! The oracle is `git tag --format='%(refname:short)|%(objectname)|
//! %(objecttype)|%(*objectname)'`, which is where the row's two oids come
//! from: `%(objectname)` is the object the ref names and `%(*objectname)` is
//! what it peels to. `git tag --contains <REV>` is the oracle for the filter.

#[path = "support.rs"]
mod support;

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::Arc;

use kaish_tool_api::Tool;
use kaish_types::{ExecResult, ToolArgs, Value};

use kaish_tools_git::GitConfig;

use support::{RefsRepo, StrictBackend, TestCtx};

/// Run `git tag` against the fixture, with the scratch root mounted at `/mnt`.
async fn run(repo: &RefsRepo, argv: &[&str]) -> ExecResult {
    let backend = Arc::new(StrictBackend::single(PathBuf::from("/mnt"), repo.scratch()));
    let mut ctx = TestCtx::new(backend, "/mnt/repo");
    let tool = kaish_tools_git::tool(GitConfig::read_only()).expect("config");
    let mut args = ToolArgs::new();
    args.positional.push(Value::String("tag".to_string()));
    let mut i = 0;
    while i < argv.len() {
        match argv[i].strip_prefix("--") {
            Some("json") => {
                args.flags.insert("json".to_string());
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

fn json(result: &ExecResult) -> serde_json::Value {
    assert_eq!(result.code, 0, "failed: {}", result.err);
    result
        .output()
        .and_then(|o| o.rich_json.clone())
        .expect("--json carries the typed model")
}

fn names(model: &serde_json::Value) -> Vec<String> {
    model["tags"]
        .as_array()
        .expect("tags array")
        .iter()
        .map(|t| t["name"].as_str().expect("name").to_string())
        .collect()
}

fn rows_by_name(model: &serde_json::Value) -> BTreeMap<String, serde_json::Value> {
    model["tags"]
        .as_array()
        .expect("tags array")
        .iter()
        .map(|t| (t["name"].as_str().expect("name").to_string(), t.clone()))
        .collect()
}

/// Every tag, in git's order, with git's two oids and git's object type.
#[tokio::test]
async fn the_listing_matches_real_gits_format() {
    let repo = RefsRepo::build();
    let model = json(&run(&repo, &["--json"]).await);

    let oracle: Vec<Vec<String>> = repo
        .git(&[
            "tag",
            "--format=%(refname:short)|%(objectname)|%(objecttype)|%(*objectname)",
        ])
        .lines()
        .map(|line| line.split('|').map(str::to_string).collect())
        .collect();
    assert!(oracle.len() >= 4, "the fixture must have tags: {oracle:?}");

    let ours = model["tags"].as_array().expect("tags array");
    assert_eq!(
        ours.len(),
        oracle.len(),
        "one row per tag: ours {:?} vs git's {:?}",
        names(&model),
        oracle.iter().map(|r| &r[0]).collect::<Vec<_>>()
    );

    for (ours, theirs) in ours.iter().zip(oracle.iter()) {
        let name = &theirs[0];
        assert_eq!(ours["name"].as_str(), Some(name.as_str()), "name order");
        assert_eq!(
            ours["oid"].as_str(),
            Some(theirs[1].as_str()),
            "the object the ref names, for {name}"
        );
        // `%(*objectname)` is empty for a lightweight tag, where git means
        // "there is nothing to peel"; the row spells that as the two oids
        // being equal, which is one less special case for a caller.
        let peeled = if theirs[3].is_empty() {
            theirs[1].as_str()
        } else {
            theirs[3].as_str()
        };
        assert_eq!(
            ours["target_oid"].as_str(),
            Some(peeled),
            "what {name} ultimately points at"
        );
        let expected_kind = if theirs[2] == "tag" {
            "annotated"
        } else {
            "lightweight"
        };
        assert_eq!(ours["kind"], expected_kind, "kind of {name}");
    }
}

/// A tag of a tag peels all the way through. It is the case that separates
/// "the object the ref names" from "what it points at", and the one a
/// single-step peel gets wrong.
#[tokio::test]
async fn a_tag_of_a_tag_peels_to_the_commit() {
    let repo = RefsRepo::build();
    let rows = rows_by_name(&json(&run(&repo, &["--json"]).await));

    let nested = &rows["nested"];
    assert_eq!(nested["kind"], "annotated");
    assert_eq!(
        nested["target_oid"], repo.b,
        "the chain nested -> v0.1.0 -> B is followed to the end: {nested}"
    );
    assert_eq!(nested["target_kind"], "commit");
    assert_ne!(
        nested["oid"], nested["target_oid"],
        "the tag object is not the commit"
    );
    assert_eq!(nested["message_summary"], "tag of a tag");

    // Negative control: the tag it points at peels one step, to the same
    // commit, and is a different object.
    let inner = &rows["v0.1.0"];
    assert_eq!(inner["target_oid"], repo.b);
    assert_ne!(inner["oid"], nested["oid"]);
}

/// An annotated tag carries its tagger and the first line of its message; a
/// lightweight tag carries neither, because it has no tag object to carry them
/// on.
#[tokio::test]
async fn annotation_is_reported_only_where_there_is_one() {
    let repo = RefsRepo::build();
    let rows = rows_by_name(&json(&run(&repo, &["--json"]).await));

    let annotated = &rows["v0.1.0"];
    assert_eq!(annotated["kind"], "annotated");
    assert_eq!(annotated["message_summary"], "release one");
    assert_eq!(annotated["tagger"]["name"], "Fixture Committer");
    assert_eq!(annotated["tagger"]["email"], "committer@example.invalid");
    assert!(
        annotated["tagger"]["time"]
            .as_str()
            .expect("a tagger time")
            .starts_with("2026-08-01T"),
        "{annotated}"
    );

    let light = &rows["light"];
    assert_eq!(light["kind"], "lightweight");
    assert_eq!(light["oid"], light["target_oid"]);
    assert!(light["tagger"].is_null(), "{light}");
    assert!(
        light["message_summary"].is_null(),
        "a lightweight tag has no message of its own, and git's \
         %(contents:subject) falling back to the target commit's subject is a \
         line nobody wrote about the tag: {light}"
    );
    // The divergence stated as an assertion rather than as prose: git DOES
    // report the commit's subject there.
    assert_eq!(
        repo.git(&["tag", "--format=%(contents:subject)", "--list", "light"]),
        "A",
        "git falls back to the target commit's subject; we report null"
    );
}

/// `--contains` matches `git tag --contains`, and the negative control is the
/// tag it must leave out.
#[tokio::test]
async fn contains_matches_real_git() {
    let repo = RefsRepo::build();
    let model = json(&run(&repo, &["--json", "--contains", &repo.b]).await);
    let ours = names(&model);
    let theirs: Vec<String> = repo
        .git(&["tag", "--contains", &repo.b])
        .lines()
        .map(str::to_string)
        .collect();
    assert_eq!(ours, theirs, "--contains B");
    assert!(
        !ours.contains(&"light".to_string()),
        "the tag at A does not contain B: {ours:?}"
    );
    assert!(
        ours.contains(&"v0.2.0".to_string()),
        "the tag at C does: {ours:?}"
    );

    // A commit no tag descends from filters everything away — a real empty
    // answer, not an error.
    let model = json(&run(&repo, &["--json", "--contains", &repo.d]).await);
    assert!(names(&model).is_empty(), "nothing descends from D");
    assert_eq!(
        repo.git(&["tag", "--contains", &repo.d]),
        "",
        "and git agrees"
    );
}

/// The cost of the walk is reported, and a plain listing does not walk at all.
#[tokio::test]
async fn commits_examined_reports_what_the_filter_cost() {
    let repo = RefsRepo::build();

    let plain = json(&run(&repo, &["--json"]).await);
    assert_eq!(
        plain["commits_examined"], 0,
        "a plain listing reads refs and no commit: {plain}"
    );

    let filtered = json(&run(&repo, &["--json", "--contains", &repo.a]).await);
    assert!(
        filtered["commits_examined"].as_u64().expect("a count") > 0,
        "--contains walks history: {filtered}"
    );
}

/// A revision that does not resolve is a git-level failure naming it, not an
/// empty listing that reads like "no tag contains it".
#[tokio::test]
async fn an_unresolvable_contains_revision_is_refused() {
    let repo = RefsRepo::build();
    let result = run(&repo, &["--json", "--contains", "no-such-rev"]).await;
    assert_eq!(result.code, 1, "stderr: {}", result.err);
    assert!(result.err.contains("no-such-rev"), "{}", result.err);
}

/// `--limit` bounds the rows and says so.
#[tokio::test]
async fn limit_truncates_and_reports() {
    let repo = RefsRepo::build();
    let result = run(&repo, &["--json", "--limit", "2"]).await;
    let model = json(&result);
    assert_eq!(model["tags"].as_array().expect("array").len(), 2);
    assert_eq!(model["truncated"], true);
    assert!(result.err.contains("--limit"), "{}", result.err);

    let full = run(&repo, &["--json"]).await;
    assert_eq!(json(&full)["truncated"], false);
    assert!(full.err.is_empty(), "stderr: {}", full.err);
}

/// Loose and packed refs are one listing. The fixture packs its refs and then
/// adds a loose one, so a reader that saw only one store would be short.
#[tokio::test]
async fn packed_and_loose_tags_are_both_listed() {
    let repo = RefsRepo::build();
    let ours = names(&json(&run(&repo, &["--json"]).await));
    assert!(ours.contains(&"v0.1.0".to_string()), "{ours:?}");
    assert!(
        repo.root.join(".git/packed-refs").is_file(),
        "the fixture must actually have packed its refs"
    );
}

/// The verb takes no operands rather than answering a different question.
#[tokio::test]
async fn operands_are_refused() {
    let repo = RefsRepo::build();
    let result = run(&repo, &["v0.1.0"]).await;
    assert_eq!(result.code, 2, "stderr: {}", result.err);
    assert!(result.err.contains("v0.1.0"), "{}", result.err);
    assert!(
        result.err.contains("--repo"),
        "the refusal points somewhere useful: {}",
        result.err
    );
}
