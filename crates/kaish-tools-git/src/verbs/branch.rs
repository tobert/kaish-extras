//! `git branch` — the branch listing, local and remote-tracking
//! (architecture.md B.7).
//!
//! Listing only in the read profile; creation moves to the commit profile.
//!
//! **The cost story is the design.** A plain listing reads refs and nothing
//! else — no commit is decoded, and `commits_examined` is 0. Three flags
//! change that, and they change it in two different ways:
//!
//! - `--contains` and `--merged` are **filters**, so they have to judge every
//!   candidate before `--limit` truncates. Truncating first would cut rows the
//!   filter never looked at and call the result a filtered listing.
//! - `--ahead-behind` is **decoration**, so it runs only on the rows that
//!   survived truncation. Each branch costs a walk of its divergence from its
//!   upstream, and paying that for rows nobody asked to see is the shape
//!   docs/issues.md G7-G10 records three times over: a cap that runs after the
//!   work is not a cap. This is why B.7 makes the counts opt-in.
//!
//! Whatever was spent is reported as `commits_examined`, so an embedder can
//! see the cost rather than infer it.

use std::collections::{BTreeMap, HashSet};

use clap::Parser;

use gix_index::hash::ObjectId;
use gix_object::bstr::ByteSlice;

use kaish_tool_api::GlobalFlags;

use crate::error::GitError;
use crate::model::{BranchKind, BranchReport, BranchRow};
use crate::reach::{ahead_behind, ancestors, refuse_shallow, Budget, Reaches};
use crate::repo::ReadRepo;

const OP: &str = "branch";

/// The ref namespace local branches live in.
const HEADS: &str = "refs/heads/";
/// The ref namespace remote-tracking branches live in.
const REMOTES: &str = "refs/remotes/";

/// `git branch`'s argv surface (architecture.md B.7).
#[derive(Parser, Debug)]
#[command(name = "branch", about = "List the repository's branches")]
pub(crate) struct BranchArgs {
    /// List remote-tracking branches beside the local ones. A remote-tracking
    /// branch is a record of what a fetch last saw; this build performs no
    /// fetch, so it is as old as the last one someone else ran.
    #[arg(long = "all", default_value_t = false, conflicts_with = "remote")]
    pub all: bool,

    /// List remote-tracking branches instead of the local ones.
    #[arg(long = "remote", default_value_t = false)]
    pub remote: bool,

    /// Report only branches whose tip has this revision in its history. Costs
    /// a walk of the history behind every candidate branch, before `--limit`
    /// applies; `commits_examined` in `--json` reports what it spent.
    #[arg(long = "contains", value_name = "REV")]
    pub contains: Option<String>,

    /// Report only branches whose tip is already in this revision's history.
    /// Costs one walk of that revision's whole history, before `--limit`
    /// applies.
    #[arg(long = "merged", value_name = "REV")]
    pub merged: Option<String>,

    /// Count each reported branch's commits against its upstream. Off by
    /// default because it costs a history walk per branch — the plain listing
    /// reads no commit at all. Only the branches `--limit` keeps are counted,
    /// so lowering the limit lowers the cost.
    #[arg(long = "ahead-behind", default_value_t = false)]
    pub ahead_behind: bool,

    /// Maximum branches to report. Truncation is always reported (`truncated:
    /// true` and a stderr note), never silent. It bounds the rows, and the
    /// `--ahead-behind` walks that follow them; it does not bound the history
    /// `--contains` and `--merged` read.
    #[arg(short = 'n', long = "limit", value_name = "N", default_value_t = 1000)]
    pub limit: usize,

    /// Repository to inspect. Defaults to the current directory; discovery
    /// searches upward, never past the mount root that contains the path.
    #[arg(long = "repo", value_name = "PATH")]
    pub repo: Option<String>,

    #[command(flatten)]
    pub global: GlobalFlags,

    /// Takes no operands: `git branch` lists branches, and narrowing is
    /// `--contains` or `--merged`. A single branch's history is
    /// `git log <BRANCH>`.
    // Bound so clap accepts the `--`-terminated tail `ToolArgs::to_argv()`
    // always emits. The real operands are read off `args.positional` in
    // `tool.rs`, which refuses them by name.
    #[arg(hide = true)]
    pub operands: Vec<String>,
}

/// Which namespaces a listing covers.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Scope {
    /// `refs/heads/` only — the default.
    Local,
    /// `refs/remotes/` only — `--remote`.
    Remote,
    /// Both — `--all`.
    All,
}

/// Everything `run` needs, decoupled from clap.
pub(crate) struct BranchOptions {
    /// Which namespaces to list.
    pub scope: Scope,
    /// `--contains`, as the caller spelled it.
    pub contains: Option<String>,
    /// `--merged`, as the caller spelled it.
    pub merged: Option<String>,
    /// Whether to count each reported branch against its upstream.
    pub ahead_behind: bool,
    /// The effective row cap: the smaller of `--limit` and the embedder's cap.
    pub limit: usize,
}

/// Compose the branch listing for `repo` (architecture.md B.7).
pub(crate) fn run(repo: &ReadRepo, opts: &BranchOptions) -> Result<BranchReport, GitError> {
    let mut budget = Budget::new(OP);

    let mut reaches = match &opts.contains {
        Some(spec) => {
            refuse_shallow(repo, OP, "--contains")?;
            Some(Reaches::to(repo.resolve_commit(spec)?))
        }
        None => None,
    };
    let merged_history: Option<HashSet<ObjectId>> = match &opts.merged {
        Some(spec) => {
            refuse_shallow(repo, OP, "--merged")?;
            let tip = repo.resolve_commit(spec)?;
            Some(ancestors(repo, &mut budget, tip)?)
        }
        None => None,
    };

    let head_branch = repo.head()?.branch;
    let upstreams = upstreams(repo)?;

    let platform = repo
        .refs()
        .iter()
        .map_err(|e| GitError::repository(OP, "opening packed-refs", repo.git_dir(), e))?;
    let all = platform
        .all()
        .map_err(|e| GitError::repository(OP, "listing refs", repo.git_dir(), e))?;

    let mut rows: Vec<(BranchRow, ObjectId)> = Vec::new();
    for reference in all {
        let reference = reference
            .map_err(|e| GitError::repository(OP, "reading a ref", repo.git_dir(), e))?;
        let full = reference.name.as_bstr().to_str_lossy().into_owned();
        let (kind, name) = match classify(&full, opts.scope) {
            Some(pair) => pair,
            None => continue,
        };
        let Some(oid) = repo.ref_object(&reference)? else {
            // A symbolic ref whose chain ends at a ref that is not there —
            // `refs/remotes/<r>/HEAD` left behind by a deleted branch. A row
            // naming no commit would be a row about nothing.
            continue;
        };

        // `--contains`/`--merged` ask an ancestry question, which only knows
        // how to walk commit history: `oid` is peeled through any tag chain
        // here, on demand, rather than in `ref_object` itself — `oid` still
        // feeds `row.oid` below unpeeled, matching what `git branch -v`
        // itself prints for a tag-tipped branch (the tag's own oid, not the
        // commit it names).
        if reaches.is_some() || merged_history.is_some() {
            let commit_oid = repo.peel_ref_to_commit(oid, &full)?;
            if let Some(reaches) = reaches.as_mut() {
                if !reaches.from(repo, &mut budget, commit_oid)? {
                    continue;
                }
            }
            if let Some(history) = &merged_history {
                if !history.contains(&commit_oid) {
                    continue;
                }
            }
        }

        let upstream_ref = (kind == BranchKind::Local)
            .then(|| upstreams.get(&name))
            .flatten();
        let upstream_oid = match upstream_ref {
            Some(refname) => repo
                .refs()
                .try_find(refname.as_str())
                .map_err(|e| GitError::repository(OP, "looking up an upstream ref", repo.git_dir(), e))?
                .as_ref()
                .map(|r| repo.ref_object(r))
                .transpose()?
                .flatten(),
            None => None,
        };

        rows.push((
            BranchRow {
                name: name.clone(),
                kind,
                oid: oid.to_string(),
                is_head: kind == BranchKind::Local && head_branch.as_deref() == Some(name.as_str()),
                upstream: upstream_ref.map(|r| shorten(r)),
                upstream_gone: upstream_ref.is_some() && upstream_oid.is_none(),
                ahead: None,
                behind: None,
            },
            oid,
        ));
    }

    // Truncate BEFORE the counts, not after: `--ahead-behind` is decoration,
    // and paying for a row nobody will see is the exact shape G7-G10 records.
    let truncated = rows.len() > opts.limit;
    rows.truncate(opts.limit);

    if opts.ahead_behind {
        refuse_shallow(repo, OP, "--ahead-behind")?;
        for (row, oid) in rows.iter_mut() {
            let Some(refname) = row
                .upstream
                .as_ref()
                .filter(|_| !row.upstream_gone)
                .and_then(|_| upstreams.get(&row.name))
            else {
                continue;
            };
            let Some(upstream_oid) = repo
                .refs()
                .try_find(refname.as_str())
                .map_err(|e| GitError::repository(OP, "looking up an upstream ref", repo.git_dir(), e))?
                .as_ref()
                .map(|r| repo.ref_commit(r))
                .transpose()?
                .flatten()
            else {
                continue;
            };
            // `--ahead-behind` is the same ancestry question as `--contains`/
            // `--merged`, so the branch's own tip is peeled here too.
            let commit_oid = repo.peel_ref_to_commit(*oid, &row.name)?;
            let (ahead, behind) = ahead_behind(repo, &mut budget, commit_oid, upstream_oid)?;
            row.ahead = Some(ahead);
            row.behind = Some(behind);
        }
    }

    Ok(BranchReport {
        branches: rows.into_iter().map(|(row, _)| row).collect(),
        ahead_behind: opts.ahead_behind,
        commits_examined: budget.examined(),
        truncated,
    })
}

/// Which namespace a full refname belongs to, and its short name — or `None`
/// when this listing does not cover it.
fn classify(full: &str, scope: Scope) -> Option<(BranchKind, String)> {
    if let Some(name) = full.strip_prefix(HEADS) {
        return matches!(scope, Scope::Local | Scope::All)
            .then(|| (BranchKind::Local, name.to_string()));
    }
    if let Some(name) = full.strip_prefix(REMOTES) {
        return matches!(scope, Scope::Remote | Scope::All)
            .then(|| (BranchKind::Remote, name.to_string()));
    }
    None
}

/// `refs/remotes/origin/main` → `origin/main`, `refs/heads/main` → `main`.
fn shorten(full: &str) -> String {
    full.strip_prefix(HEADS)
        .or_else(|| full.strip_prefix(REMOTES))
        .unwrap_or(full)
        .to_string()
}

/// Each local branch's configured upstream, as a full refname.
///
/// Git's own three-part rule, and all of it: `branch.<name>.remote` names
/// either `.` (this repository, so `branch.<name>.merge` is the upstream
/// directly) or a remote, in which case `branch.<name>.merge` is matched
/// against that remote's `fetch` refspecs and the destination is the upstream.
/// A refspec carries at most one `*` on each side, which is git's own rule
/// too, so the substitution below is the whole grammar rather than a subset
/// of it.
///
/// The upstream is recorded whether or not the ref exists — git reports a
/// configured-but-absent upstream as `[gone]`, and so does
/// [`BranchRow::upstream_gone`].
fn upstreams(repo: &ReadRepo) -> Result<BTreeMap<String, String>, GitError> {
    // First value wins on a duplicate key, which is git's own rule for
    // `@{upstream}`. `collect()` into a map keeps the *last*, which would
    // count a branch against the wrong upstream in a repository that
    // configures `branch.<name>.merge` more than once.
    let remotes = first_per_subsection(repo.config_values("branch", "remote")?);
    let merges = first_per_subsection(repo.config_values("branch", "merge")?);
    let fetch_specs = repo.config_values("remote", "fetch")?;

    let mut out = BTreeMap::new();
    for (branch, remote) in remotes {
        let Some(merge) = merges.get(&branch) else {
            // `branch.<name>.remote` with no `.merge` names a remote but not
            // what to track on it. Git treats that as no upstream.
            continue;
        };
        if remote == "." {
            out.insert(branch, merge.clone());
            continue;
        }
        for (spec_remote, spec) in &fetch_specs {
            if spec_remote.as_deref() != Some(remote.as_str()) {
                continue;
            }
            if let Some(dst) = apply_refspec(spec, merge) {
                out.insert(branch.clone(), dst);
                break;
            }
        }
    }
    Ok(out)
}

/// Keep the first value configured for each subsection, dropping the rest and
/// dropping the section-wide values that name no subsection.
fn first_per_subsection(
    values: Vec<(Option<String>, String)>,
) -> BTreeMap<String, String> {
    let mut out: BTreeMap<String, String> = BTreeMap::new();
    for (subsection, value) in values {
        let Some(subsection) = subsection else { continue };
        out.entry(subsection).or_insert(value);
    }
    out
}

/// Map a source refname through one fetch refspec, or `None` when it does not
/// match.
///
/// `[+]<src>:<dst>`, where each side carries at most one `*`. A leading `+`
/// is force-update, which says nothing about the mapping.
fn apply_refspec(spec: &str, source: &str) -> Option<String> {
    let spec = spec.strip_prefix('+').unwrap_or(spec);
    let (src, dst) = spec.split_once(':')?;
    match src.split_once('*') {
        None => (src == source).then(|| dst.to_string()),
        Some((prefix, suffix)) => {
            let rest = source.strip_prefix(prefix)?.strip_suffix(suffix)?;
            let (dst_prefix, dst_suffix) = dst.split_once('*')?;
            Some(format!("{dst_prefix}{rest}{dst_suffix}"))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_wildcard_refspec_substitutes_the_matched_part() {
        assert_eq!(
            apply_refspec("+refs/heads/*:refs/remotes/origin/*", "refs/heads/main"),
            Some("refs/remotes/origin/main".to_string())
        );
        // Nested names keep their slashes: the `*` is not a path component.
        assert_eq!(
            apply_refspec("+refs/heads/*:refs/remotes/origin/*", "refs/heads/feature/side"),
            Some("refs/remotes/origin/feature/side".to_string())
        );
        // An exact refspec matches exactly, and nothing else.
        assert_eq!(
            apply_refspec("refs/heads/main:refs/remotes/origin/main", "refs/heads/main"),
            Some("refs/remotes/origin/main".to_string())
        );
        assert_eq!(
            apply_refspec("refs/heads/main:refs/remotes/origin/main", "refs/heads/other"),
            None
        );
        // A source outside the pattern does not map at all.
        assert_eq!(
            apply_refspec("+refs/heads/*:refs/remotes/origin/*", "refs/tags/v1"),
            None
        );
        // A malformed refspec maps nothing rather than half of something.
        assert_eq!(apply_refspec("refs/heads/*", "refs/heads/main"), None);
        assert_eq!(
            apply_refspec("+refs/heads/*:refs/remotes/origin/main", "refs/heads/main"),
            None,
            "a wildcard source needs a wildcard destination to substitute into"
        );
    }

    #[test]
    fn a_scope_selects_one_namespace_or_both() {
        assert_eq!(
            classify("refs/heads/main", Scope::Local),
            Some((BranchKind::Local, "main".to_string()))
        );
        assert_eq!(classify("refs/heads/main", Scope::Remote), None);
        assert_eq!(
            classify("refs/remotes/origin/main", Scope::Remote),
            Some((BranchKind::Remote, "origin/main".to_string()))
        );
        assert_eq!(classify("refs/remotes/origin/main", Scope::Local), None);
        assert!(classify("refs/heads/main", Scope::All).is_some());
        assert!(classify("refs/remotes/origin/main", Scope::All).is_some());
        // Nothing else is a branch, under any scope.
        for scope in [Scope::Local, Scope::Remote, Scope::All] {
            assert_eq!(classify("refs/tags/v1", scope), None);
            assert_eq!(classify("HEAD", scope), None);
        }
    }
}
