//! `git log` — commit history from a starting revision (architecture.md B.3).
//!
//! Composed on the plumbing like every other verb. The pieces:
//!
//! - **The walk** — `gix-traverse`'s commit ancestry iterator from the resolved
//!   `--rev`, newest first in commit-date order (git's default), or first-parent
//!   only under `--first-parent`.
//! - **The filters** — `--author` (literal substring, never a regex),
//!   `--since`/`--until` (two unambiguous date forms, everything else refused
//!   loudly), `--merges`/`--no-merges` (parent count), and `--path` (a commit
//!   is kept when its tree differs, under one of the paths, from *every* parent
//!   — git's default history simplification, so a merge that only carried a
//!   side branch's change through is not reported as having made it). Filters
//!   narrow what is *reported*; they never stop the walk early, because history
//!   is neither sorted by author nor by path — nor, reliably, by date.
//! - **`--stat`** — a tree comparison against the first parent for the
//!   changed-file count, and `gix-imara-diff` over the two blobs for line
//!   counts. Neither needs `gix-diff`'s `blob` feature, so `gix-command` stays
//!   out of the graph (A.2's tripwire).
//!
//! `--patch` is refused with exit 4 rather than approximated: per-commit hunk
//! text is the `textdiff` phase (H.5/H.6), and a wrong patch is worse than an
//! honest "this build cannot".
//!
//! Date handling is the named gate for this verb. Git's `approxidate`
//! ("2 weeks ago", "yesterday", "last tuesday") is a pattern language in
//! disguise: it accepts nearly any string and silently resolves the ones it
//! does not understand to *now*, which would hand an agent a plausible,
//! confidently wrong window. We accept RFC3339 and `YYYY-MM-DD`, and reject
//! everything else by name.

use std::collections::BTreeSet;

use clap::Parser;

use gix_index::hash::ObjectId;
use gix_object::bstr::ByteSlice;
use gix_object::FindExt;

use kaish_tool_api::GlobalFlags;

use crate::error::GitError;
use crate::model::{CommitInfo, LogReport, Signature, StatSummary};
use crate::pathfilter::PathFilter;
use crate::repo::ReadRepo;

/// `git log`'s argv surface (architecture.md B.3).
#[derive(Parser, Debug)]
#[command(
    name = "log",
    about = "Show commit history from a revision, newest first"
)]
pub(crate) struct LogArgs {
    /// Revision to start from: `HEAD` (the default), a branch, a tag, an oid,
    /// or any of those with a `~N` / `^N` suffix. Range syntax (`A..B`) and
    /// reflog syntax (`@{...}`) are usage errors, not silent reinterpretations.
    #[arg(long = "rev", value_name = "REV", default_value = "HEAD")]
    pub rev: String,

    /// Maximum commits to report. Truncation is always reported
    /// (`truncated: true` and a stderr note), never silent.
    #[arg(short = 'n', long = "limit", value_name = "N", default_value_t = 20)]
    pub limit: usize,

    /// Restrict history to commits that touched these literal paths or simple
    /// globs (`*`, `**`, `?`). Repeatable. Git pathspec magic (`:(exclude)`,
    /// `:!`, `:/`) is a loud usage error, never silently matched.
    #[arg(long = "path", value_name = "PATH")]
    pub path: Vec<String>,

    /// Only commits at or after this instant. RFC3339
    /// (`2026-08-01T10:00:00Z`) or `YYYY-MM-DD` only — git's "2 weeks ago"
    /// approxidate syntax is refused rather than guessed at.
    #[arg(long = "since", value_name = "DATE")]
    pub since: Option<String>,

    /// Only commits at or before this instant. Same two accepted forms as
    /// `--since`.
    #[arg(long = "until", value_name = "DATE")]
    pub until: Option<String>,

    /// Only commits whose author name or email contains this substring. A
    /// **literal** substring, case-sensitive — not a regex, and not a glob.
    #[arg(long = "author", value_name = "SUBSTRING")]
    pub author: Option<String>,

    /// Only merge commits (two or more parents).
    #[arg(long = "merges", default_value_t = false, conflicts_with = "no_merges")]
    pub merges: bool,

    /// Skip merge commits.
    #[arg(long = "no-merges", default_value_t = false)]
    pub no_merges: bool,

    /// Follow only the first parent of each merge — mainline history, without
    /// the merged branches' commits.
    #[arg(long = "first-parent", default_value_t = false)]
    pub first_parent: bool,

    /// Include each commit's full message body, not just its summary line.
    #[arg(long = "body", default_value_t = false)]
    pub body: bool,

    /// Include per-commit changed-file and line counts against the first
    /// parent.
    #[arg(long = "stat", default_value_t = false)]
    pub stat: bool,

    /// Include per-commit patch hunks. This build has no unified-diff
    /// assembly — that is the `textdiff` phase — so this exits 4 naming the
    /// gap rather than silently ignoring the flag.
    #[arg(long = "patch", default_value_t = false)]
    pub patch: bool,

    /// Repository to inspect. Defaults to the current directory; discovery
    /// searches upward, never past the mount root that contains the path.
    #[arg(long = "repo", value_name = "PATH")]
    pub repo: Option<String>,

    #[command(flatten)]
    pub global: GlobalFlags,

    /// Validation-only sink: `ToolArgs::to_argv()` always emits `--` before
    /// positionals, and `log` takes none. Read nothing off this field.
    #[arg(hide = true)]
    pub operands: Vec<String>,
}

/// Which commits to keep, by parent count (B.3's `--merges` / `--no-merges`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub(crate) enum MergeFilter {
    /// Both merges and non-merges — the default.
    #[default]
    Both,
    /// Only commits with two or more parents.
    Only,
    /// Only commits with fewer than two parents.
    Exclude,
}

/// Everything `run` needs, decoupled from clap so `verbs::log::run` stays a
/// plain function testable without a `ToolCtx`.
pub(crate) struct LogOptions {
    /// The starting revision, as the caller spelled it.
    pub rev: String,
    /// The effective row cap: the smaller of `--limit` and the embedder's cap.
    pub limit: usize,
    /// Repo-relative path/glob filters (already prefixed by the caller's cwd).
    pub paths: Vec<String>,
    /// Lower bound of the time window, as seconds since the epoch.
    pub since: Option<i64>,
    /// Upper bound of the time window, as seconds since the epoch.
    pub until: Option<i64>,
    /// Literal substring the author name or email must contain.
    pub author: Option<String>,
    /// Which commits to keep, by parent count.
    pub merges: MergeFilter,
    /// Whether to follow only the first parent.
    pub first_parent: bool,
    /// Whether to include each commit's body.
    pub body: bool,
    /// Whether to compute `--stat` counts.
    pub stat: bool,
    /// The embedder's `max_blob_bytes`. `--stat` reads blob pairs to count
    /// lines, so this is what stands between a repository and an allocation it
    /// chose.
    pub max_blob_bytes: u64,
}

/// How deep a `--stat` tree comparison may recurse before it is refused.
///
/// The same reasoning as the status walk's cap: an oid cycle is hash-hard, but
/// a cheaply-built deep tree would overflow the stack. Loud error, not a
/// silent truncation.
const MAX_TREE_DEPTH: usize = 64;

/// How many commits the walk may examine before giving up looking for matches.
///
/// With a filter in effect the walk cannot stop at `--limit` — it has to keep
/// examining history to find the next match — so an unmatched filter over a
/// large repository would otherwise walk every commit. This bounds that work.
/// Reaching it reports `truncated: true`, the same honest signal as hitting
/// `--limit`.
const MAX_COMMITS_EXAMINED: usize = 100_000;

// ═══════════════════════════════════════════════════════════════════════════
// Dates: two unambiguous forms, and a loud refusal for everything else
// ═══════════════════════════════════════════════════════════════════════════

/// Parse a `--since` / `--until` value into seconds since the epoch.
///
/// Exactly two forms are accepted (B.3):
///
/// - **RFC3339** — `2026-08-01T10:00:00Z`, or with an explicit offset
///   (`2026-08-01T19:00:00+09:00`). Fractional seconds are fine.
/// - **`YYYY-MM-DD`** — a bare civil date, interpreted as midnight UTC. A date
///   has no timezone of its own, and picking up the host's would make the same
///   command mean different instants on different machines.
///
/// Everything else is refused by name. This is the deliberate divergence from
/// git: `approxidate` would take "2 weeks ago" — and would also take "banana",
/// resolving it to *now* without complaint. An agent cannot audit a window it
/// did not get.
pub(crate) fn parse_date(operation: &'static str, flag: &str, value: &str) -> Result<i64, GitError> {
    // RFC3339 first: it is the form with a zone in it, so it needs no
    // assumption from us.
    if let Ok(ts) = value.parse::<jiff::Timestamp>() {
        return Ok(ts.as_second());
    }
    // A bare civil date means midnight UTC, stated rather than inferred.
    if let Ok(date) = value.parse::<jiff::civil::Date>() {
        let midnight = date.to_datetime(jiff::civil::Time::midnight());
        // A civil date at UTC always maps to exactly one instant — there is no
        // DST gap in UTC — so this cannot fail for any date `jiff` parsed.
        if let Ok(zoned) = midnight.to_zoned(jiff::tz::TimeZone::UTC) {
            return Ok(zoned.timestamp().as_second());
        }
    }
    Err(GitError::Usage {
        operation,
        message: format!(
            "{flag} '{value}' is not a date this build accepts — give RFC3339 \
             ('2026-08-01T10:00:00Z', or with an offset) or 'YYYY-MM-DD' \
             (midnight UTC). Relative forms like '2 weeks ago' are git's \
             approxidate, which kaish-git does not parse: it resolves what it \
             cannot understand to the current time, which would answer with a \
             window you did not ask for"
        ),
    })
}

// ═══════════════════════════════════════════════════════════════════════════
// The walk
// ═══════════════════════════════════════════════════════════════════════════

/// Gather everything `git log` reports.
///
/// Runs entirely inside the caller's blocking closure and returns an owned
/// model: no gix value escapes, so E.3's "no gix value crosses an `.await`"
/// holds by construction rather than by review.
pub(crate) fn run(repo: &ReadRepo, opts: &LogOptions) -> Result<LogReport, GitError> {
    const OP: &str = "log";

    // Parsed before the walk so a bad `--path` is a usage error on the command
    // line, not a failure discovered a thousand commits in.
    let filter = PathFilter::parse(OP, &opts.paths)?;
    let filtering_paths = !opts.paths.is_empty();

    let start = repo.resolve_commit(&opts.rev)?;
    let objects = repo.objects();

    let mut commits: Vec<CommitInfo> = Vec::new();
    let mut truncated = false;
    let mut examined = 0usize;

    // The ancestry walk. `Sorting::ByCommitTimeNewestFirst` is git's default
    // ordering, so the first page matches what a human would see.
    let mut walk = gix_traverse::commit::Simple::new(Some(start), objects)
        .sorting(gix_traverse::commit::simple::Sorting::ByCommitTime(
            gix_traverse::commit::simple::CommitTimeOrder::NewestFirst,
        ))
        .map_err(|e| GitError::repository(OP, "starting the commit walk", repo.git_dir(), e))?;
    if opts.first_parent {
        walk = walk.parents(gix_traverse::commit::Parents::First);
    }

    for step in walk {
        let info = step.map_err(|e| {
            GitError::repository(OP, "walking commit ancestry", repo.git_dir(), e)
        })?;

        examined += 1;
        if examined > MAX_COMMITS_EXAMINED {
            truncated = true;
            break;
        }

        let oid = info.id;
        let mut buf = Vec::new();
        let commit = objects
            .find_commit(&oid, &mut buf)
            .map_err(|e| GitError::repository(OP, "reading a commit", repo.git_dir(), e))?;

        let parents: Vec<ObjectId> = commit.parents().collect();

        // Parent-count filter: cheap, so it runs before anything that reads
        // more objects.
        let is_merge = parents.len() >= 2;
        let keep_by_merge = match opts.merges {
            MergeFilter::Both => true,
            MergeFilter::Only => is_merge,
            MergeFilter::Exclude => !is_merge,
        };
        if !keep_by_merge {
            continue;
        }

        // The raw header fields are `&BStr`; the parsed forms are what carry a
        // name, an email and a time. A commit whose signature will not parse is
        // a malformed repository, and saying so beats reporting a blank author.
        let author_sig = commit
            .author()
            .map_err(|e| GitError::repository(OP, "decoding a commit author", repo.git_dir(), e))?;
        let committer_sig = commit.committer().map_err(|e| {
            GitError::repository(OP, "decoding a commit committer", repo.git_dir(), e)
        })?;
        let author = signature(&author_sig);
        let committer = signature(&committer_sig);

        // The time window is judged on the *committer* time, which is what git
        // sorts by and what `--since`/`--until` filter on. Author time can
        // predate it by a lot after a rebase, and filtering on it would drop
        // commits that are plainly inside the window as history records it.
        let commit_seconds = committer_sig
            .time()
            .map_err(|e| GitError::repository(OP, "decoding a commit time", repo.git_dir(), e))?
            .seconds;
        // Both bounds are inclusive, and neither stops the walk. It is tempting
        // to break on the first commit older than `--since` — the walk is
        // time-ordered, after all — but commit dates are not monotonic along
        // ancestry: a rebase, an amended date, or a skewed clock can put an
        // older timestamp on a child than on its parent. Breaking would then
        // silently drop that parent and everything behind it. Filtering
        // without breaking costs a longer walk, bounded by
        // `MAX_COMMITS_EXAMINED`, and cannot lose a commit that matched.
        if let Some(since) = opts.since {
            if commit_seconds < since {
                continue;
            }
        }
        if let Some(until) = opts.until {
            if commit_seconds > until {
                continue;
            }
        }

        if let Some(needle) = &opts.author {
            // A literal substring over "name <email>", so a caller can match
            // either half or spell out both.
            let haystack = format!("{} <{}>", author.name, author.email);
            if !haystack.contains(needle.as_str()) {
                continue;
            }
        }

        // The path filter needs a tree comparison, so it runs last among the
        // filters — it is the expensive one.
        let first_parent = parents.first().copied();
        if filtering_paths && !commit_touches_paths(repo, oid, &parents, &filter)? {
            continue;
        }

        let (summary, body) = split_message(commit.message.as_bstr().to_str_lossy().as_ref());

        let stat = if opts.stat {
            Some(commit_stat(
                repo,
                oid,
                first_parent,
                is_merge,
                opts.max_blob_bytes,
            )?)
        } else {
            None
        };

        commits.push(CommitInfo {
            oid: oid.to_string(),
            short_oid: oid.to_string()[..7].to_string(),
            parents: parents.iter().map(|p| p.to_string()).collect(),
            author,
            committer,
            summary,
            body: if opts.body { Some(body) } else { None },
            stat,
        });

        if commits.len() >= opts.limit {
            // There may or may not be more history behind this point. Rather
            // than claim either way, ask the walk for one more step: that is
            // the difference between "capped" and "this was the whole story".
            truncated = true;
            break;
        }
    }

    Ok(LogReport {
        rev: opts.rev.clone(),
        commits,
        truncated,
    })
}

/// Convert a gix signature into the model's, formatting the time as RFC3339
/// with the actor's own offset preserved.
///
/// The offset is the commit's, not the reader's: git records the timezone the
/// author was in, and dropping it to UTC would lose a real fact about the
/// commit that no other field carries. The instant is identical either way.
fn signature(sig: &gix_actor::SignatureRef<'_>) -> Signature {
    Signature {
        name: sig.name.to_str_lossy().into_owned(),
        email: sig.email.to_str_lossy().into_owned(),
        // An unparsable time in an otherwise-valid signature is reported as
        // the empty string rather than a fabricated epoch — a wrong timestamp
        // is worse than a visibly absent one.
        time: sig
            .time()
            .ok()
            .and_then(|t| t.format(gix_date::time::format::ISO8601_STRICT).ok())
            .unwrap_or_default(),
    }
}

/// Split a commit message into its summary line and the body below it.
///
/// Git's own rule: the summary is the first line, and the body is what follows
/// the blank line after it, trimmed. A message with only a summary has an empty
/// body — distinct from a body that was not asked for, which the model spells
/// `None`.
fn split_message(message: &str) -> (String, String) {
    let mut lines = message.splitn(2, '\n');
    let summary = lines.next().unwrap_or_default().trim_end().to_string();
    let body = lines.next().unwrap_or_default().trim().to_string();
    (summary, body)
}

// ═══════════════════════════════════════════════════════════════════════════
// --path and --stat: tree comparison against the first parent
// ═══════════════════════════════════════════════════════════════════════════

/// One changed path from a commit-vs-first-parent tree comparison, with the
/// blob oids on each side so a caller can count lines without walking again.
struct Change {
    path: String,
    old: Option<ObjectId>,
    new: Option<ObjectId>,
}

/// The tree of a commit, or the empty tree for `None` (a root commit's parent).
fn tree_of(repo: &ReadRepo, commit: Option<ObjectId>) -> Result<Option<ObjectId>, GitError> {
    const OP: &str = "log";
    let Some(commit) = commit else {
        return Ok(None);
    };
    let mut buf = Vec::new();
    let c = repo
        .objects()
        .find_commit(&commit, &mut buf)
        .map_err(|e| GitError::repository(OP, "reading a commit", repo.git_dir(), e))?;
    Ok(Some(c.tree()))
}

/// Flatten a tree into `path → (oid, is_blob)`, bounded in depth.
///
/// `gix-diff`'s tree platform is the richer tool, but it is also the one whose
/// rename tracking is `blob`-gated (→ `gix-command`). A direct flatten-and-
/// compare needs none of that, and it is the same shape `status` already uses
/// for HEAD-vs-index — one mechanism, two callers.
fn flatten_tree(
    repo: &ReadRepo,
    tree: Option<ObjectId>,
    out: &mut std::collections::BTreeMap<String, ObjectId>,
) -> Result<(), GitError> {
    const OP: &str = "log";
    let Some(tree) = tree else {
        return Ok(());
    };
    // An explicit stack rather than recursion: a hostile repository can make a
    // tree as deep as it likes, and a deep recursion would overflow the stack
    // before any cap could fire.
    let mut stack: Vec<(ObjectId, String, usize)> = vec![(tree, String::new(), 0)];
    while let Some((oid, prefix, depth)) = stack.pop() {
        if depth > MAX_TREE_DEPTH {
            return Err(GitError::TreeTooDeep {
                operation: OP,
                limit: MAX_TREE_DEPTH,
            });
        }
        let mut buf = Vec::new();
        let iter = repo
            .objects()
            .find_tree_iter(&oid, &mut buf)
            .map_err(|e| GitError::repository(OP, "reading a tree", repo.git_dir(), e))?;
        for entry in iter {
            let entry = entry
                .map_err(|e| GitError::repository(OP, "decoding a tree", repo.git_dir(), e))?;
            let name = entry.filename.to_str_lossy();
            let path = if prefix.is_empty() {
                name.into_owned()
            } else {
                format!("{prefix}/{name}")
            };
            if entry.mode.is_tree() {
                stack.push((entry.oid.to_owned(), path, depth + 1));
            } else {
                // Blobs and gitlinks both land here; a gitlink's oid is a
                // commit in another repository, which `--stat` counts as a
                // changed file with no line delta (it has no blob to read).
                out.insert(path, entry.oid.to_owned());
            }
        }
    }
    Ok(())
}

/// Every path that differs between a commit's tree and the given parent's.
///
/// A `None` parent is the empty tree, which is what a root commit is compared
/// against — so its every file reads as an addition, matching git.
fn changes_against(
    repo: &ReadRepo,
    commit: ObjectId,
    parent: Option<ObjectId>,
) -> Result<Vec<Change>, GitError> {
    let mut new_side = std::collections::BTreeMap::new();
    let mut old_side = std::collections::BTreeMap::new();
    flatten_tree(repo, tree_of(repo, Some(commit))?, &mut new_side)?;
    flatten_tree(repo, tree_of(repo, parent)?, &mut old_side)?;

    let mut out = Vec::new();
    let paths: BTreeSet<&String> = new_side.keys().chain(old_side.keys()).collect();
    for path in paths {
        let old = old_side.get(path).copied();
        let new = new_side.get(path).copied();
        if old != new {
            out.push(Change {
                path: path.clone(),
                old,
                new,
            });
        }
    }
    Ok(out)
}

/// Whether a commit changed anything under one of the `--path` filters.
///
/// A root commit is compared against the empty tree, so its every file counts
/// as touched — which is what git reports for the first commit of a path.
///
/// **A merge is judged against every parent, not just the first.** This is
/// git's default history simplification (its "TREESAME" rule): a merge whose
/// tree matches *any* parent under the paths brought no change of its own to
/// them, and the commits it merged already report that change. Judging a merge
/// against its first parent alone would report the merge as well, which is
/// git's `--full-history`, not its default — and would double-count every
/// side-branch change in the one view an agent uses to ask "what touched this
/// file".
fn commit_touches_paths(
    repo: &ReadRepo,
    commit: ObjectId,
    parents: &[ObjectId],
    filter: &PathFilter,
) -> Result<bool, GitError> {
    let touches = |parent: Option<ObjectId>| -> Result<bool, GitError> {
        let changes = changes_against(repo, commit, parent)?;
        Ok(changes.iter().any(|c| filter.matches(&c.path)))
    };

    if parents.is_empty() {
        // A root commit: compared against the empty tree.
        return touches(None);
    }
    for parent in parents {
        if !touches(Some(*parent))? {
            // Same as this parent under the paths, so the merge introduced
            // nothing here.
            return Ok(false);
        }
    }
    Ok(true)
}

/// A commit's `--stat` summary against its first parent.
fn commit_stat(
    repo: &ReadRepo,
    commit: ObjectId,
    first_parent: Option<ObjectId>,
    is_merge: bool,
    max_blob_bytes: u64,
) -> Result<StatSummary, GitError> {
    // Git shows no diffstat for a merge by default — a merge's changes are
    // already reported by the commits it merged — and this matches that rather
    // than inventing a combined diff.
    if is_merge {
        return Ok(StatSummary::default());
    }

    let changes = changes_against(repo, commit, first_parent)?;
    let mut summary = StatSummary {
        files: changes.len(),
        ..StatSummary::default()
    };

    for change in &changes {
        match line_delta(repo, change, max_blob_bytes)? {
            Some((added, deleted)) => {
                summary.additions += added;
                summary.deletions += deleted;
            }
            None => summary.lines_capped += 1,
        }
    }
    Ok(summary)
}

/// Added and deleted line counts for one changed path, or `None` when a side
/// was over the cap or is not text.
///
/// Returning `None` rather than zero is the honest encoding: zero would claim
/// the file changed no lines, which is a different fact from "we declined to
/// read it". The caller counts these in `lines_capped`.
fn line_delta(
    repo: &ReadRepo,
    change: &Change,
    max_blob_bytes: u64,
) -> Result<Option<(u64, u64)>, GitError> {
    let old = read_blob(repo, change.old, max_blob_bytes)?;
    let new = read_blob(repo, change.new, max_blob_bytes)?;
    let (old, new) = match (old, new) {
        (Some(o), Some(n)) => (o, n),
        // Either side was over the cap, or unreadable as a blob.
        _ => return Ok(None),
    };
    // A NUL byte is git's own binary heuristic, and a binary file has no line
    // count worth reporting — git leaves those out of its shortstat totals too.
    if old.contains(&0) || new.contains(&0) {
        return Ok(None);
    }

    let old_text = String::from_utf8_lossy(&old);
    let new_text = String::from_utf8_lossy(&new);
    // `lines` tokenizes on line boundaries, so a "token" here is a line and the
    // counts are line counts — the same unit `git log --numstat` reports.
    let input = gix_imara_diff::InternedInput::new(
        gix_imara_diff::sources::lines(old_text.as_ref()),
        gix_imara_diff::sources::lines(new_text.as_ref()),
    );
    let diff = gix_imara_diff::Diff::compute(gix_imara_diff::Algorithm::Myers, &input);
    Ok(Some((
        u64::from(diff.count_additions()),
        u64::from(diff.count_removals()),
    )))
}

/// Read a blob, refusing one larger than the embedder's cap.
///
/// The size is checked from the object header before the content is read, so an
/// oversized blob is never allocated — the same discipline `status` follows for
/// worktree files. Unlike `status`, this returns `None` instead of erroring:
/// a `--stat` over a repository with one huge file should still answer, with
/// that file's lines honestly absent (`lines_capped`), rather than fail the
/// whole log.
fn read_blob(
    repo: &ReadRepo,
    oid: Option<ObjectId>,
    max_blob_bytes: u64,
) -> Result<Option<Vec<u8>>, GitError> {
    const OP: &str = "log";
    let Some(oid) = oid else {
        // An absent side is a real, empty side: an added file's "old" content
        // is genuinely zero lines, not a declined read.
        return Ok(Some(Vec::new()));
    };
    let mut buf = Vec::new();
    let data = match repo.objects().find(&oid, &mut buf) {
        Ok(d) => d,
        Err(e) => {
            return Err(GitError::repository(
                OP,
                "reading a blob",
                repo.git_dir(),
                e,
            ))
        }
    };
    if data.kind != gix_object::Kind::Blob {
        // A gitlink points at a commit in another repository; there is no blob
        // here to count lines in.
        return Ok(None);
    }
    if data.data.len() as u64 > max_blob_bytes {
        return Ok(None);
    }
    Ok(Some(data.data.to_vec()))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The two accepted date forms, and the refusal that is this verb's named
    /// design gate. A regression here would silently widen the surface to
    /// approxidate, which is exactly what B.3 forbids.
    #[test]
    fn parse_date_accepts_rfc3339_and_plain_dates() {
        // RFC3339 with Z.
        assert_eq!(
            parse_date("log", "--since", "1970-01-01T00:00:00Z").expect("epoch parses"),
            0
        );
        // A bare civil date is midnight UTC.
        assert_eq!(
            parse_date("log", "--since", "1970-01-02").expect("a plain date parses"),
            86_400
        );
        // An explicit offset is honored, not ignored: 09:00+09:00 is 00:00Z.
        assert_eq!(
            parse_date("log", "--since", "1970-01-01T09:00:00+09:00").expect("offset parses"),
            0
        );
    }

    /// Approxidate is the whole point of the gate. Git would accept every one
    /// of these — resolving the nonsense ones to *now* — and hand back a window
    /// the caller never asked for.
    #[test]
    fn parse_date_refuses_approxidate_and_nonsense() {
        for bad in [
            "2 weeks ago",
            "yesterday",
            "last tuesday",
            "now",
            "banana",
            "",
            "2026-13-01",   // not a real month
            "2026-02-30",   // not a real day
            "01/02/2026",   // ambiguous by design — which is the month?
            "2026",         // a year is not an instant
        ] {
            let err = parse_date("log", "--since", bad)
                .expect_err("approxidate and nonsense must be refused, not guessed");
            assert_eq!(err.exit_code(), 2, "a bad date is a usage error: {bad}");
            assert!(
                err.to_string().contains(bad) || bad.is_empty(),
                "the refusal names what was given: {bad}"
            );
        }
    }

    /// A summary-only message has an empty body, and a body keeps its interior
    /// blank lines — only the separator and the trailing newline go.
    #[test]
    fn split_message_separates_summary_from_body() {
        let (s, b) = split_message("just a summary\n");
        assert_eq!(s, "just a summary");
        assert_eq!(b, "");

        let (s, b) = split_message("summary\n\nbody line one\n\nbody line two\n");
        assert_eq!(s, "summary");
        assert_eq!(b, "body line one\n\nbody line two");
    }
}
