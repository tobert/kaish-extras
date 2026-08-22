//! `git ls` — a tree listing at a revision (architecture.md B.6).
//!
//! Pairs with `show <rev>:<path>` (`verbs/show.rs`) to make "read the repo at
//! revision X" complete without touching the working tree: `ls` finds what is
//! there, `show` reads it. The row shape and the tree walk are both shared
//! with `show`'s tree form through `crate::treewalk` — one vocabulary, read
//! once, rather than two listings that could quietly drift apart.

use clap::Parser;

use kaish_tool_api::GlobalFlags;

use crate::error::GitError;
use crate::model::LsReport;
use crate::repo::ReadRepo;
use crate::treewalk::{self, TreeTarget};

/// Where `git ls` starts when the caller names no revision.
pub(crate) const DEFAULT_REV: &str = "HEAD";

/// `git ls`'s argv surface (architecture.md B.6).
///
/// `<REV>` and `<PATH>` are both bare positionals, read off `args.positional`
/// in `tool.rs` rather than through clap (E.1) — the same reason every verb
/// here carries a hidden `operands` sink instead of typed positional fields.
/// Position, not content, decides which is which: the first operand is
/// always the revision, the second the path — exactly how `git ls-tree <rev>
/// [<path>]` reads its own two positionals, with no `--` needed to tell them
/// apart.
#[derive(Parser, Debug)]
#[command(name = "ls", about = "List a tree's entries at a revision")]
pub(crate) struct LsArgs {
    /// Expand subtrees instead of stopping one level down. A recursive
    /// listing reports only leaves (files, symlinks, submodule gitlinks) at
    /// their full repo-relative path; a directory is a step to more of them,
    /// not a row of its own — the same shape `git ls-tree -r` reports.
    #[arg(long = "recursive", default_value_t = false)]
    pub recursive: bool,

    /// Maximum rows to report. Truncation is always reported (`truncated:
    /// true` and a stderr note), never silent.
    #[arg(short = 'n', long = "limit", value_name = "N", default_value_t = 1000)]
    pub limit: usize,

    /// Repository to inspect. Defaults to the current directory; discovery
    /// searches upward, never past the mount root that contains the path.
    #[arg(long = "repo", value_name = "PATH")]
    pub repo: Option<String>,

    #[command(flatten)]
    pub global: GlobalFlags,

    /// The revision to list, then a path under it: `git ls HEAD src`.
    /// Position decides which is which, so no `--` is needed. The colon form
    /// `git ls HEAD:src` works too. Omit the path to list the repository root.
    // Bound so clap accepts the `--`-terminated tail `ToolArgs::to_argv()`
    // always emits. The real operands are read off `args.positional` in
    // `tool.rs` — the kernel's own convention, because `to_argv` inserts a
    // `--` of its own and clap cannot tell it from the caller's. Do not read
    // this field; it cannot distinguish them either.
    #[arg(hide = true)]
    pub operands: Vec<String>,
}

/// Everything `run` needs, decoupled from clap so `verbs::ls::run` stays a
/// plain function testable without a `ToolCtx`.
pub(crate) struct LsOptions {
    /// The revision, exactly as the caller's first operand spelled it — a
    /// `<rev>:<path>` colon form is accepted here too, the same grammar
    /// `show` uses, so `ls HEAD:src` and `ls HEAD src` both work.
    pub rev: String,
    /// The repo-relative path from the second operand, `""` when none was
    /// given (the repository root, unless `rev` supplies one after a colon).
    pub path: String,
    /// Whether to expand subtrees.
    pub recursive: bool,
    /// The effective row cap: the smaller of `--limit` and the embedder's cap.
    pub limit: usize,
}

/// Compose a tree listing for `repo` under `opts` (architecture.md B.6).
pub(crate) fn run(repo: &ReadRepo, opts: &LsOptions) -> Result<LsReport, GitError> {
    const OP: &str = "ls";

    let (rev, colon_path) = crate::repo::split_revision_and_path(OP, &opts.rev)?;
    // The path can come from a `:` suffix on the first operand or from the
    // second operand, but not both — there is no rule for which one wins, so
    // giving both is a usage error rather than a silent pick (the same
    // reasoning `log` applies to a revision given twice).
    let path = match (&colon_path, opts.path.is_empty()) {
        (Some(p), true) => p.clone(),
        (None, _) => opts.path.clone(),
        (Some(_), false) => {
            return Err(GitError::Usage {
                operation: OP,
                message: format!(
                    "got a path twice — '{}' after the ':' in '{}', and '{}' \
                     as the second operand. Give it once",
                    colon_path.as_deref().unwrap_or_default(),
                    opts.rev,
                    opts.path
                ),
            })
        }
    };

    let object = repo.resolve_object(&rev)?;
    let root_tree = repo.tree_of_object(object, &rev)?;

    let (entries, truncated) = match treewalk::resolve_path_in_tree(repo, OP, root_tree, &rev, &path)? {
        TreeTarget::Tree(tree) => {
            let mut entries = Vec::new();
            let mut truncated = false;
            treewalk::list_tree(
                repo,
                OP,
                tree,
                &path,
                treewalk::WalkParams {
                    recursive: opts.recursive,
                    limit: opts.limit,
                },
                &mut entries,
                &mut truncated,
            )?;
            (entries, truncated)
        }
        // A path naming a single non-tree entry (a file, a symlink, a
        // submodule gitlink) — `--recursive` has nothing to expand, and the
        // listing is that one row, matching `git ls-tree <rev> -- <path>`
        // for a non-directory pathspec.
        TreeTarget::Entry(row) => {
            if opts.limit == 0 {
                (Vec::new(), true)
            } else {
                (vec![row], false)
            }
        }
    };

    Ok(LsReport {
        rev: opts.rev.clone(),
        path,
        recursive: opts.recursive,
        entries,
        truncated,
    })
}
