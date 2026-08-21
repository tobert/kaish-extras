//! Typed model → kaish [`OutputData`] (architecture.md B.10).
//!
//! One pattern for every verb: a table for the text surface, and the full
//! typed model attached as `rich_json` for `--json`. `owns_output` is not used
//! anywhere in this crate — it would buy a bespoke `--json` envelope and cost
//! the kernel's uniform envelope, its `--help` safety net, and its
//! output-format accounting.

use kaish_types::{OutputData, OutputNode};

use crate::model::{CommitInfo, LogReport, LsReport, RepoInfo, ShowTag, StatusReport, TreeRow};

/// Render [`RepoInfo`] as a `FIELD`/`VALUE` table carrying the full object as
/// `rich_json`.
///
/// The text surface is a summary; `--json` is the whole model. Nothing an
/// agent can only get from one form is missing from the other's structure —
/// `gix_pins` and `capabilities` are nested objects with no useful flat
/// rendering, so the table names them and points at `--json`.
pub fn repo_info(info: &RepoInfo) -> OutputData {
    let row = |field: &str, value: String| OutputNode::new(field).with_cells(vec![value]);

    let head_value = match (&info.head.branch, &info.head.oid, info.head.detached) {
        (_, Some(oid), true) => format!("detached at {oid}"),
        (Some(branch), Some(oid), false) => format!("{branch} ({oid})"),
        (Some(branch), None, false) => format!("{branch} (unborn — no commits yet)"),
        // A detached HEAD always names an object, and an attached one always
        // names a branch; anything else is a repository we misread.
        (branch, oid, detached) => format!("branch={branch:?} oid={oid:?} detached={detached}"),
    };

    let rows = vec![
        row(
            "repo_root_vfs",
            info.repo_root_vfs
                .clone()
                .unwrap_or_else(|| "(outside every mount)".to_string()),
        ),
        row("repo_root_real", info.repo_root_real.clone()),
        row("git_dir", info.git_dir.clone()),
        row("bare", info.bare.to_string()),
        row("shallow", info.shallow.to_string()),
        row(
            "ref_backend",
            serde_json::to_value(info.ref_backend)
                .ok()
                .and_then(|v| v.as_str().map(str::to_string))
                .unwrap_or_else(|| "files".to_string()),
        ),
        row("head", head_value),
        row("worktrees", info.worktrees.to_string()),
        row("submodules", info.submodules.to_string()),
        row("gix_pins", format!("{} crates (see --json)", info.gix_pins.len())),
        row(
            "capabilities",
            format!(
                "profiles={} verbs={} (see --json)",
                info.capabilities.profiles.join(","),
                info.capabilities.verbs.join(",")
            ),
        ),
    ];

    let table = OutputData::table(vec!["FIELD".to_string(), "VALUE".to_string()], rows);
    match serde_json::to_value(info) {
        Ok(json) => table.with_rich_json(json),
        // Serializing an owned model of plain scalars cannot fail in
        // practice; if it somehow did, the table is still a correct answer
        // and losing --json silently would be worse than saying so.
        Err(e) => {
            tracing::warn!(error = %e, "git info: could not build the --json payload");
            table
        }
    }
}

/// Render a [`StatusReport`] as the porcelain `XY`/`PATH` table (B.2), carrying
/// the full word-valued model as `rich_json`.
///
/// Letters in the text surface, words in JSON (decision 9): the table speaks
/// git's `XY` pair — the spelling deep in model training — while `--json`
/// carries `"index":"modified"` and friends. Both are derived from one
/// `StatusReport`, so they cannot disagree.
pub fn status(report: &StatusReport) -> OutputData {
    let rows: Vec<OutputNode> = report
        .entries
        .iter()
        .map(|entry| {
            let xy: String = entry.porcelain.iter().collect();
            let path = match &entry.orig_path {
                Some(orig) => format!("{} ← {orig}", entry.path),
                None => entry.path.clone(),
            };
            OutputNode::new(xy).with_cells(vec![path])
        })
        .collect();

    let table = OutputData::table(vec!["XY".to_string(), "PATH".to_string()], rows);
    match serde_json::to_value(report) {
        Ok(json) => table.with_rich_json(json),
        // A model of owned scalars cannot fail to serialize in practice; the
        // table is still a correct answer, and losing --json silently would be
        // worse than saying so.
        Err(e) => {
            tracing::warn!(error = %e, "git status: could not build the --json payload");
            table
        }
    }
}

/// Render a [`LogReport`] as a one-line-per-commit table (B.3), carrying the
/// full model as `rich_json`.
///
/// The text surface is `git log --oneline` plus the date and author, which is
/// the shape a reader scans; `--json` carries the parents, the full oid, the
/// body and the stat counts. `--stat` adds a counts column rather than a second
/// table, so one row stays one commit whatever flags are in play.
pub fn log(report: &LogReport) -> OutputData {
    let with_stat = report.commits.iter().any(|c| c.stat.is_some());
    let with_body = report.commits.iter().any(|c| c.body.is_some());

    let rows: Vec<OutputNode> = report
        .commits
        .iter()
        .map(|commit| {
            let mut cells = vec![
                // The date alone; the time of day is in --json for anyone who
                // needs it, and this keeps the column scannable.
                commit.author.time.chars().take(10).collect::<String>(),
                commit.author.name.clone(),
                commit.summary.clone(),
            ];
            if with_stat {
                cells.push(match &commit.stat {
                    Some(s) => format!("{} files +{} -{}", s.files, s.additions, s.deletions),
                    None => String::new(),
                });
            }
            if with_body {
                // A body is multi-line by nature; the table shows whether there
                // is one, and --json carries the text.
                cells.push(match &commit.body {
                    Some(b) if !b.is_empty() => format!("{} lines", b.lines().count()),
                    _ => String::new(),
                });
            }
            OutputNode::new(commit.short_oid.clone()).with_cells(cells)
        })
        .collect();

    let mut headers = vec![
        "OID".to_string(),
        "DATE".to_string(),
        "AUTHOR".to_string(),
        "SUMMARY".to_string(),
    ];
    if with_stat {
        headers.push("STAT".to_string());
    }
    if with_body {
        headers.push("BODY".to_string());
    }

    let table = OutputData::table(headers, rows);
    match serde_json::to_value(report) {
        Ok(json) => table.with_rich_json(json),
        // A model of owned scalars cannot fail to serialize in practice; the
        // table is still a correct answer, and losing --json silently would be
        // worse than saying so.
        Err(e) => {
            tracing::warn!(error = %e, "git log: could not build the --json payload");
            table
        }
    }
}

/// Insert `"kind": <kind>` into an already-serialized object.
///
/// `git show`'s "the type is always stated in the output" (B.5) applies to
/// `--json` too — every nested [`crate::model::ShowTarget`] already carries
/// its own `kind` via `#[serde(tag = "kind")]`, and this is what makes the
/// *top-level* result agree with that same convention rather than only
/// stating its kind in `git.show_kind` baggage.
fn tag_kind(mut json: serde_json::Value, kind: &str) -> serde_json::Value {
    if let Some(obj) = json.as_object_mut() {
        obj.insert("kind".to_string(), serde_json::Value::String(kind.to_string()));
    }
    json
}

/// One row of a tree listing, in `git ls-tree`'s own column order: mode,
/// kind, oid, path — shared by [`ls`] and `show`'s tree form so the two
/// tables read identically.
fn tree_row(entry: &TreeRow) -> OutputNode {
    let kind = serde_json::to_value(entry.kind)
        .ok()
        .and_then(|v| v.as_str().map(str::to_string))
        .unwrap_or_else(|| "?".to_string());
    OutputNode::new(entry.mode.clone()).with_cells(vec![kind, entry.oid.clone(), entry.path.clone()])
}

const TREE_HEADERS: [&str; 4] = ["MODE", "KIND", "OID", "PATH"];

/// Render an [`LsReport`] as a `git ls-tree`-shaped table (B.6), carrying the
/// full model as `rich_json`.
pub fn ls(report: &LsReport) -> OutputData {
    let rows: Vec<OutputNode> = report.entries.iter().map(tree_row).collect();
    let table = OutputData::table(TREE_HEADERS.map(String::from).to_vec(), rows);
    match serde_json::to_value(report) {
        Ok(json) => table.with_rich_json(json),
        Err(e) => {
            tracing::warn!(error = %e, "git ls: could not build the --json payload");
            table
        }
    }
}

/// Render a tree form reached through `git show <rev>:<path>` — the same
/// table [`ls`] renders, with `"kind": "tree"` added to `--json` so the
/// result states its type the same way every other `show` case does.
pub fn show_tree(report: &LsReport) -> OutputData {
    let rows: Vec<OutputNode> = report.entries.iter().map(tree_row).collect();
    let table = OutputData::table(TREE_HEADERS.map(String::from).to_vec(), rows);
    match serde_json::to_value(report) {
        Ok(json) => table.with_rich_json(tag_kind(json, "tree")),
        Err(e) => {
            tracing::warn!(error = %e, "git show: could not build the --json payload");
            table
        }
    }
}

/// Render a submodule gitlink named directly by `git show <rev>:<path>` — the
/// one row `ls` would show for the same path (see
/// [`crate::verbs::show::ShowOutcome::Gitlink`]'s doc comment for why this
/// has no dedicated fifth shape).
pub fn show_gitlink(entry: &TreeRow) -> OutputData {
    let table = OutputData::table(TREE_HEADERS.map(String::from).to_vec(), vec![tree_row(entry)]);
    match serde_json::to_value(entry) {
        Ok(json) => table.with_rich_json(tag_kind(json, "commit")),
        Err(e) => {
            tracing::warn!(error = %e, "git show: could not build the --json payload");
            table
        }
    }
}

/// Render a commit reached through `git show` — the same `FIELD`/`VALUE`
/// shape [`repo_info`] uses, tagged `"kind": "commit"`.
pub fn show_commit(commit: &CommitInfo) -> OutputData {
    let row = |field: &str, value: String| OutputNode::new(field).with_cells(vec![value]);
    let rows = vec![
        row("oid", commit.oid.clone()),
        row("parents", commit.parents.join(", ")),
        row(
            "author",
            format!("{} <{}> {}", commit.author.name, commit.author.email, commit.author.time),
        ),
        row(
            "committer",
            format!(
                "{} <{}> {}",
                commit.committer.name, commit.committer.email, commit.committer.time
            ),
        ),
        row("summary", commit.summary.clone()),
        row("body", commit.body.clone().unwrap_or_default()),
    ];
    let table = OutputData::table(vec!["FIELD".to_string(), "VALUE".to_string()], rows);
    match serde_json::to_value(commit) {
        Ok(json) => table.with_rich_json(tag_kind(json, "commit")),
        Err(e) => {
            tracing::warn!(error = %e, "git show: could not build the --json payload");
            table
        }
    }
}

/// Render an annotated tag reached through `git show` — its own metadata,
/// then the tagged object's oid and kind (the full nested description is in
/// `--json`; the text table stays a summary, matching every other verb's
/// text/JSON split in this crate).
pub fn show_tag(tag: &ShowTag) -> OutputData {
    let row = |field: &str, value: String| OutputNode::new(field).with_cells(vec![value]);
    let target_kind = serde_json::to_value(tag.target.as_ref())
        .ok()
        .and_then(|v| v.get("kind").and_then(|k| k.as_str()).map(str::to_string))
        .unwrap_or_else(|| "?".to_string());
    let rows = vec![
        row("oid", tag.oid.clone()),
        row("name", tag.name.clone()),
        row(
            "tagger",
            tag.tagger
                .as_ref()
                .map(|s| format!("{} <{}> {}", s.name, s.email, s.time))
                .unwrap_or_else(|| "(none)".to_string()),
        ),
        row("message", tag.message.clone()),
        row("target", format!("{} ({}, see --json)", tag.target_oid, target_kind)),
    ];
    let table = OutputData::table(vec!["FIELD".to_string(), "VALUE".to_string()], rows);
    match serde_json::to_value(tag) {
        Ok(json) => table.with_rich_json(tag_kind(json, "tag")),
        Err(e) => {
            tracing::warn!(error = %e, "git show: could not build the --json payload");
            table
        }
    }
}
