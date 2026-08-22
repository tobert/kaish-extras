//! `git show` — describe a commit, tag, tree, or blob at a revision
//! (architecture.md B.5).
//!
//! Behavior is chosen by the resolved object's own type, and the type is
//! always stated in the output — `tool.rs` tags every result with
//! `git.show_kind` baggage regardless of which arm below produced it, on top
//! of each structured shape's own `kind` field. Scope for this phase (PR4 of
//! the phasing table): commit metadata, tag metadata, a tree listing, and
//! capped blob bytes — no `--name-only`, `--stat` or `--patch` yet (those are
//! PRs 5 and 6; advertising a flag this build cannot honor is exactly the
//! defect the curl work spent a day removing).
//!
//! The blob form is the highest-value verb here for an agent: read a file at
//! any revision, with no checkout, no worktree, no writes.

use clap::Parser;
use gix_index::hash::ObjectId;
use gix_object::bstr::ByteSlice;
use gix_object::FindExt;
use gix_odb::HeaderExt;

use kaish_tool_api::GlobalFlags;

use crate::error::GitError;
use crate::model::{CommitInfo, EntryKind, LsReport, ShowTag, ShowTarget, TreeRow};
use crate::repo::ReadRepo;
use crate::treewalk::{self, TreeTarget};
use crate::verbs::log;

/// Where `git show` starts when the caller names no revision.
pub(crate) const DEFAULT_REV: &str = "HEAD";

/// How many tag-of-a-tag hops `show` will follow before refusing. Tags
/// pointing at tags are rare and mostly historical; a small bound is what
/// stands between this and a hand-built cycle spinning forever, the same
/// posture as `ReadRepo::peel_to_commit`'s 32-iteration cap.
const MAX_TAG_DEPTH: usize = 8;

/// `git show`'s argv surface (architecture.md B.5).
///
/// `<REV>` is a single bare positional, read off `args.positional` in
/// `tool.rs` (E.1) rather than through clap. It carries the whole flagship
/// spelling, colon path included: `show HEAD:src/lib.rs` is one operand, not
/// two — `git log HEAD:src/lib.rs` is the one place a colon is still refused
/// (D2 in the PR4 design notes), and `log` never hands this parser a colon
/// to begin with.
#[derive(Parser, Debug)]
#[command(name = "show", about = "Show a commit, tag, tree, or blob at a revision")]
pub(crate) struct ShowArgs {
    /// Maximum rows to report for a tree form. Truncation is always
    /// reported (`truncated: true` and a stderr note), never silent. The
    /// blob form's byte cap is this build's `max_blob_bytes`, which
    /// `--limit` does not affect.
    #[arg(short = 'n', long = "limit", value_name = "N", default_value_t = 1000)]
    pub limit: usize,

    /// Repository to inspect. Defaults to the current directory; discovery
    /// searches upward, never past the mount root that contains the path.
    #[arg(long = "repo", value_name = "PATH")]
    pub repo: Option<String>,

    #[command(flatten)]
    pub global: GlobalFlags,

    /// One revision, with an optional `:<path>` inside it:
    /// `git show HEAD:src/lib.rs` is a single operand, not two. Bare
    /// `git show HEAD` shows the commit. A second operand is an error.
    // Bound so clap accepts the `--`-terminated tail `ToolArgs::to_argv()`
    // always emits. The real operands are read off `args.positional` in
    // `tool.rs` — the kernel's own convention, because `to_argv` inserts a
    // `--` of its own and clap cannot tell it from the caller's. Do not read
    // this field; it cannot distinguish them either.
    #[arg(hide = true)]
    pub operands: Vec<String>,
}

/// Everything `run` needs, decoupled from clap so `verbs::show::run` stays a
/// plain function testable without a `ToolCtx`.
pub(crate) struct ShowOptions {
    /// The revision, exactly as the caller's one operand spelled it — a
    /// `<rev>:<path>` colon suffix included, when there is one.
    pub rev: String,
    /// The effective row cap for a tree form: the smaller of `--limit` and
    /// the embedder's `max_rows`.
    pub limit: usize,
    /// The embedder's `max_blob_bytes` — the blob form's cap, never widened
    /// by `--limit`.
    pub max_blob_bytes: u64,
}

/// What `git show` resolved to. The non-blob cases are JSON-safe model types
/// ([`CommitInfo`], [`ShowTag`], [`LsReport`]); the blob case carries raw
/// bytes outside the model on purpose — [`ShowTarget::Blob`] describes a
/// *nested* blob (inside a tag chain) without embedding it, for exactly this
/// reason (see its doc comment).
pub(crate) enum ShowOutcome {
    /// The revision named a commit.
    Commit(CommitInfo),
    /// The revision named an annotated tag.
    Tag(ShowTag),
    /// The revision (or `<rev>:<path>`) named a tree.
    Tree(LsReport),
    /// The revision (or `<rev>:<path>`) named a blob.
    Blob {
        /// The blob's oid.
        oid: String,
        /// The blob's real size in bytes, whether or not `bytes` was capped.
        size: u64,
        /// Whether `bytes` was declined for being over `max_blob_bytes` —
        /// when true, `bytes` is empty, not a truncated prefix: serving part
        /// of an object this build declined to fully materialize would still
        /// let a repository pick an allocation, the same reasoning
        /// `verbs::log`'s `--stat` blob cap already uses.
        truncated: bool,
        /// The blob's content, empty when `truncated` is true.
        bytes: Vec<u8>,
    },
    /// `<rev>:<path>` named a submodule gitlink directly — not a directory
    /// to list, not a file with bytes to read. `git show` has no fifth
    /// object kind for this (B.5 enumerates commit/tag/tree/blob only), so
    /// it is reported as the one row `ls` would show for the same path,
    /// tagged with its own `kind: "commit"` rather than invented as a sixth
    /// shape.
    Gitlink(TreeRow),
}

/// Resolve `repo` under `opts` to whatever `git show` names (architecture.md
/// B.5).
pub(crate) fn run(repo: &ReadRepo, opts: &ShowOptions) -> Result<ShowOutcome, GitError> {
    const OP: &str = "show";

    let (rev, path) = crate::repo::split_revision_and_path(OP, &opts.rev)?;
    let object = repo.resolve_object(&rev)?;

    if let Some(path) = path {
        let root_tree = repo.tree_of_object(object, &rev)?;
        return match treewalk::resolve_path_in_tree(repo, OP, root_tree, &rev, &path)? {
            TreeTarget::Tree(tree) => {
                let mut entries = Vec::new();
                let mut truncated = false;
                let params = treewalk::WalkParams {
                    recursive: false,
                    limit: opts.limit,
                };
                treewalk::list_tree(repo, OP, tree, &path, params, &mut entries, &mut truncated)?;
                Ok(ShowOutcome::Tree(LsReport {
                    rev: opts.rev.clone(),
                    path,
                    recursive: false,
                    entries,
                    truncated,
                }))
            }
            TreeTarget::Entry(row) if row.kind == EntryKind::Commit => Ok(ShowOutcome::Gitlink(row)),
            TreeTarget::Entry(row) => {
                let oid = parse_oid(OP, repo, &row.oid)?;
                let (bytes, size, truncated) = read_capped_blob(repo, OP, oid, opts.max_blob_bytes)?;
                Ok(ShowOutcome::Blob {
                    oid: row.oid,
                    size,
                    truncated,
                    bytes,
                })
            }
        };
    }

    // No path: describe the resolved object by its own kind.
    let kind = repo
        .objects()
        .header(object)
        .map_err(|e| GitError::repository(OP, "reading an object header", repo.git_dir(), e))?
        .kind();
    match kind {
        gix_object::Kind::Commit => Ok(ShowOutcome::Commit(build_commit_info(repo, OP, object)?)),
        gix_object::Kind::Tag => Ok(ShowOutcome::Tag(build_show_tag(repo, OP, object, opts.limit, 0)?)),
        gix_object::Kind::Tree => {
            let mut entries = Vec::new();
            let mut truncated = false;
            let params = treewalk::WalkParams {
                recursive: false,
                limit: opts.limit,
            };
            treewalk::list_tree(repo, OP, object, "", params, &mut entries, &mut truncated)?;
            Ok(ShowOutcome::Tree(LsReport {
                rev: opts.rev.clone(),
                path: String::new(),
                recursive: false,
                entries,
                truncated,
            }))
        }
        gix_object::Kind::Blob => {
            let (bytes, size, truncated) = read_capped_blob(repo, OP, object, opts.max_blob_bytes)?;
            Ok(ShowOutcome::Blob {
                oid: object.to_string(),
                size,
                truncated,
                bytes,
            })
        }
    }
}

/// Build a commit's full detail for `show` — the same [`CommitInfo`] shape
/// `git log` reports per commit, with the body always populated (a single
/// commit is the whole answer `show` gives, so there is no bulk-listing cost
/// to holding it back behind a flag) and `stat` always `None` (this build has
/// no `--stat`/`--patch` for `show` yet — later phases).
fn build_commit_info(repo: &ReadRepo, op: &'static str, oid: ObjectId) -> Result<CommitInfo, GitError> {
    let mut buf = Vec::new();
    let commit = repo
        .objects()
        .find_commit(&oid, &mut buf)
        .map_err(|e| GitError::repository(op, "reading a commit", repo.git_dir(), e))?;
    let parents: Vec<ObjectId> = commit.parents().collect();
    let author_sig = commit
        .author()
        .map_err(|e| GitError::repository(op, "decoding a commit author", repo.git_dir(), e))?;
    let committer_sig = commit
        .committer()
        .map_err(|e| GitError::repository(op, "decoding a commit committer", repo.git_dir(), e))?;
    let message = commit.message.to_str_lossy();
    let (summary, body) = log::split_message(message.as_ref());
    Ok(CommitInfo {
        oid: oid.to_string(),
        short_oid: oid.to_string()[..7].to_string(),
        parents: parents.iter().map(ToString::to_string).collect(),
        author: log::signature(&author_sig),
        committer: log::signature(&committer_sig),
        summary,
        body: Some(body),
        stat: None,
    })
}

/// Build an annotated tag's own metadata plus the object it points at
/// (architecture.md B.5, "then the tagged object"), recursing through a
/// tag-of-a-tag bounded by [`MAX_TAG_DEPTH`].
fn build_show_tag(
    repo: &ReadRepo,
    op: &'static str,
    oid: ObjectId,
    limit: usize,
    depth: usize,
) -> Result<ShowTag, GitError> {
    if depth >= MAX_TAG_DEPTH {
        return Err(GitError::repository(
            op,
            "following a tag chain",
            repo.git_dir(),
            std::io::Error::other(format!("more than {MAX_TAG_DEPTH} levels of tag nesting")),
        ));
    }
    let mut buf = Vec::new();
    let tag = repo
        .objects()
        .find_tag(&oid, &mut buf)
        .map_err(|e| GitError::repository(op, "reading a tag", repo.git_dir(), e))?;
    let name = tag.name.to_str_lossy().into_owned();
    let tagger = tag
        .tagger()
        .map_err(|e| GitError::repository(op, "decoding a tag's tagger", repo.git_dir(), e))?
        .map(|sig| log::signature(&sig));
    let message = tag.message.to_str_lossy().into_owned();
    let target_oid = tag.target();
    drop(buf); // the object store is about to be read again, for the target

    let target = describe_tag_target(repo, op, target_oid, limit, depth)?;

    Ok(ShowTag {
        oid: oid.to_string(),
        name,
        tagger,
        message,
        target_oid: target_oid.to_string(),
        target: Box::new(target),
    })
}

/// Describe what a [`ShowTag`] points at — every case except a gitlink, which
/// is a tree-entry shape, not an object kind, and so cannot be a tag's
/// target. The blob arm is deliberately header-only: [`ShowTarget::Blob`]
/// never carries content (see its doc comment), so there is nothing to gain
/// from reading the object's bytes just to discard them.
fn describe_tag_target(
    repo: &ReadRepo,
    op: &'static str,
    oid: ObjectId,
    limit: usize,
    depth: usize,
) -> Result<ShowTarget, GitError> {
    let header = repo
        .objects()
        .header(oid)
        .map_err(|e| GitError::repository(op, "reading an object header", repo.git_dir(), e))?;
    match header.kind() {
        gix_object::Kind::Commit => Ok(ShowTarget::Commit(build_commit_info(repo, op, oid)?)),
        gix_object::Kind::Tree => {
            let mut entries = Vec::new();
            let mut truncated = false;
            let params = treewalk::WalkParams {
                recursive: false,
                limit,
            };
            treewalk::list_tree(repo, op, oid, "", params, &mut entries, &mut truncated)?;
            Ok(ShowTarget::Tree(LsReport {
                rev: oid.to_string(),
                path: String::new(),
                recursive: false,
                entries,
                truncated,
            }))
        }
        gix_object::Kind::Blob => Ok(ShowTarget::Blob {
            oid: oid.to_string(),
            size: header.size(),
        }),
        // One more hop taken; `build_show_tag` is where the depth bound is
        // checked, once per hop.
        gix_object::Kind::Tag => Ok(ShowTarget::Tag(build_show_tag(repo, op, oid, limit, depth + 1)?)),
    }
}

/// Read a blob's bytes, refusing to materialize one over `cap`.
///
/// The size is checked from the object header before any content is read, so
/// an oversized blob is never allocated — the same discipline
/// `verbs::log::read_blob` and `verbs::status`'s worktree read both follow.
fn read_capped_blob(
    repo: &ReadRepo,
    op: &'static str,
    oid: ObjectId,
    cap: u64,
) -> Result<(Vec<u8>, u64, bool), GitError> {
    let header = repo
        .objects()
        .header(oid)
        .map_err(|e| GitError::repository(op, "reading an object header", repo.git_dir(), e))?;
    let size = header.size();
    if size > cap {
        return Ok((Vec::new(), size, true));
    }
    let mut buf = Vec::new();
    let blob = repo
        .objects()
        .find_blob(&oid, &mut buf)
        .map_err(|e| GitError::repository(op, "reading a blob", repo.git_dir(), e))?;
    Ok((blob.data.to_vec(), size, false))
}

/// Parse a [`TreeRow`]'s hex oid back into an [`ObjectId`].
///
/// The row came from this build's own tree walk moments earlier, so a
/// malformed hex string here is an internal bug, not a caller mistake —
/// reported through the same `Repository` variant every other "this should
/// not happen" read failure in this crate uses.
fn parse_oid(op: &'static str, repo: &ReadRepo, hex: &str) -> Result<ObjectId, GitError> {
    ObjectId::from_hex(hex.as_bytes()).map_err(|e| {
        GitError::repository(op, "parsing an oid this build just produced", repo.git_dir(), e)
    })
}
