//! `git tag` — the tag listing, both kinds (architecture.md B.7).
//!
//! Listing only in the read profile; creation moves to the commit profile.
//!
//! The row separates two oids git's own porcelain keeps apart with a `*`
//! sigil: `oid` is the object the ref names, and `target_oid` is what the tag
//! ultimately points at once every tag object in the chain is peeled away.
//! For a lightweight tag they are the same object, which is what makes
//! `kind` worth reporting rather than inferring.
//!
//! `--contains` costs commits, not rows, so it runs before `--limit`
//! truncates: a filter applied after truncation would have judged only the
//! rows that survived, and dropped the rest without looking. What it spent is
//! reported as `commits_examined` (`crate::reach`).

use clap::Parser;

use gix_object::bstr::ByteSlice;
use gix_object::FindExt;

use kaish_tool_api::GlobalFlags;

use crate::error::GitError;
use crate::model::{Signature, TagKind, TagReport, TagRow};
use crate::reach::{refuse_shallow, Budget, Reaches};
use crate::repo::ReadRepo;

const OP: &str = "tag";

/// The ref namespace tags live in.
const TAGS: &str = "refs/tags/";

/// `git tag`'s argv surface (architecture.md B.7).
#[derive(Parser, Debug)]
#[command(name = "tag", about = "List the repository's tags")]
pub(crate) struct TagArgs {
    /// Report only tags whose target has this revision in its history. Costs
    /// a walk of the history behind every tag, so it is slower than the plain
    /// listing by an amount that tracks the repository, not the tag count;
    /// `commits_examined` in `--json` reports what it spent.
    #[arg(long = "contains", value_name = "REV")]
    pub contains: Option<String>,

    /// Maximum tags to report. Truncation is always reported (`truncated:
    /// true` and a stderr note), never silent. It bounds the rows, not the
    /// history `--contains` reads.
    #[arg(short = 'n', long = "limit", value_name = "N", default_value_t = 1000)]
    pub limit: usize,

    /// Repository to inspect. Defaults to the current directory; discovery
    /// searches upward, never past the mount root that contains the path.
    #[arg(long = "repo", value_name = "PATH")]
    pub repo: Option<String>,

    #[command(flatten)]
    pub global: GlobalFlags,

    /// Takes no operands: `git tag` lists every tag, and narrowing is
    /// `git tag --contains <REV>`. A single tag's own metadata is
    /// `git show <TAG>`.
    // Bound so clap accepts the `--`-terminated tail `ToolArgs::to_argv()`
    // always emits. The real operands are read off `args.positional` in
    // `tool.rs`, which refuses them by name.
    #[arg(hide = true)]
    pub operands: Vec<String>,
}

/// Everything `run` needs, decoupled from clap.
pub(crate) struct TagOptions {
    /// `--contains`, as the caller spelled it.
    pub contains: Option<String>,
    /// The effective row cap: the smaller of `--limit` and the embedder's cap.
    pub limit: usize,
}

/// Compose the tag listing for `repo` (architecture.md B.7).
pub(crate) fn run(repo: &ReadRepo, opts: &TagOptions) -> Result<TagReport, GitError> {
    let mut budget = Budget::new(OP);
    let mut reaches = match &opts.contains {
        Some(spec) => {
            refuse_shallow(repo, OP, "--contains")?;
            Some(Reaches::to(repo.resolve_commit(spec)?))
        }
        None => None,
    };

    let platform = repo
        .refs()
        .iter()
        .map_err(|e| GitError::repository(OP, "opening packed-refs", repo.git_dir(), e))?;
    let all = platform
        .all()
        .map_err(|e| GitError::repository(OP, "listing refs", repo.git_dir(), e))?;

    let mut rows = Vec::new();
    for reference in all {
        let reference = reference
            .map_err(|e| GitError::repository(OP, "reading a ref", repo.git_dir(), e))?;
        let full = reference.name.as_bstr().to_str_lossy().into_owned();
        let Some(name) = full.strip_prefix(TAGS) else {
            continue;
        };
        let Some(oid) = repo.ref_object(&reference)? else {
            // A tag ref whose symbolic chain ends nowhere. Nothing git writes,
            // and a row naming no object would be a row about nothing.
            continue;
        };

        let (target_oid, target_kind) = repo.peel_tag_chain(oid, "a tag")?;
        let (kind, tagger, message_summary) = if target_oid == oid {
            (TagKind::Lightweight, None, None)
        } else {
            let (tagger, summary) = annotation(repo, oid)?;
            (TagKind::Annotated, tagger, summary)
        };

        if let Some(reaches) = reaches.as_mut() {
            // Only a commit has history to search. A tag of a tree or a blob
            // contains no revision, which is the same answer git gives.
            if target_kind != gix_object::Kind::Commit
                || !reaches.from(repo, &mut budget, target_oid)?
            {
                continue;
            }
        }

        rows.push(TagRow {
            name: name.to_string(),
            oid: oid.to_string(),
            kind,
            target_oid: target_oid.to_string(),
            target_kind: target_kind.to_string(),
            tagger,
            message_summary,
        });
    }

    let truncated = rows.len() > opts.limit;
    rows.truncate(opts.limit);
    Ok(TagReport {
        tags: rows,
        commits_examined: budget.examined(),
        truncated,
    })
}

/// A tag object's tagger and the first line of its message.
///
/// Only the summary is carried: a tag body is an unbounded allocation of
/// repository content (the shape docs/issues.md L7 records for commit
/// messages), and `git show <TAG>` is where the whole message lives.
fn annotation(
    repo: &ReadRepo,
    oid: gix_index::hash::ObjectId,
) -> Result<(Option<Signature>, Option<String>), GitError> {
    let mut buf = Vec::new();
    let tag = repo
        .objects()
        .find_tag(&oid, &mut buf)
        .map_err(|e| GitError::repository(OP, "reading a tag object", repo.git_dir(), e))?;

    let tagger = tag
        .tagger()
        .map_err(|e| GitError::repository(OP, "decoding a tag's tagger", repo.git_dir(), e))?
        .map(|sig| crate::verbs::log::signature(&sig));
    let message = tag.message.to_str_lossy();
    let summary = message.lines().next().unwrap_or_default().trim().to_string();
    Ok((tagger, (!summary.is_empty()).then_some(summary)))
}
