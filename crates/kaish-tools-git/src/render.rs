//! Typed model → kaish [`OutputData`] (architecture.md B.10).
//!
//! One pattern for every verb: a table for the text surface, and the full
//! typed model attached as `rich_json` for `--json`. `owns_output` is not used
//! anywhere in this crate — it would buy a bespoke `--json` envelope and cost
//! the kernel's uniform envelope, its `--help` safety net, and its
//! output-format accounting.

use kaish_types::{OutputData, OutputNode};

use crate::model::RepoInfo;

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
