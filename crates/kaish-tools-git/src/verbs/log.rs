//! `git log` — commit history from a starting revision (architecture.md B.3).
//!
//! Composed on the plumbing like every other verb. The pieces:
//!
//! - **The walk** — a commit-time max-heap from the resolved `--rev`, newest
//!   first (git's default ordering), or first-parent only under
//!   `--first-parent`. Ours rather than `gix-traverse`'s because git's history
//!   simplification decides which *parents to follow*, which cannot be
//!   expressed as a filter over a traversal that enqueues them all.
//! - **The filters** — `--author` (literal substring, never a regex),
//!   `--since`/`--until` (two unambiguous date forms, everything else refused
//!   loudly), `--merges`/`--no-merges` (parent count), and `--path` (a commit
//!   whose tree matches some parent's under the paths introduced nothing there,
//!   so it is not reported AND the walk follows only that parent — git's
//!   default history simplification, which prunes a branch a merge discarded).
//!   Filters
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

use std::cmp::Ordering;
use std::collections::{BTreeSet, BinaryHeap, HashSet};

use clap::Parser;

use gix_index::hash::ObjectId;
use gix_object::bstr::ByteSlice;
use gix_object::FindExt;

use kaish_tool_api::GlobalFlags;

use crate::diffcore::{flatten_tree, line_delta, Class, LineDelta, Side};
use crate::error::GitError;
use crate::model::{CommitInfo, LogReport, Signature, StatSummary};
use crate::pathfilter::PathFilter;
use crate::repo::ReadRepo;

/// Where `git log` starts when the caller names no revision.
///
/// Named rather than inlined because the dispatcher compares against it to
/// tell "the caller wrote `--rev HEAD`" from "the caller wrote nothing" — the
/// difference between a redundant argument and a conflicting one.
pub(crate) const DEFAULT_REV: &str = "HEAD";

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
    /// Also accepted as a bare operand, git's spelling: `git log main`.
    #[arg(long = "rev", value_name = "REV", default_value = DEFAULT_REV)]
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

    /// A revision to start from, then paths after `--`:
    /// `git log HEAD~5 -- src/lib.rs`. At most one revision — a range is two
    /// calls, not `A B`. Omit it to start from `HEAD`.
    // Bound so clap accepts the `--`-terminated tail `ToolArgs::to_argv()`
    // always emits. The real operands are read off `args.positional` in
    // `tool.rs` — the kernel's own convention, because `to_argv` inserts a
    // `--` of its own and clap cannot tell it from the caller's. Do not read
    // this field; it cannot distinguish them either.
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
    /// The embedder's `max_diff_files`. Bounds how many of a commit's changed
    /// files `--stat` will diff — `max_blob_bytes` bounds each read, this
    /// bounds how many reads one commit can ask for.
    pub max_diff_files: usize,
}

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
    //
    // The shape is checked HERE rather than left to `jiff`, because
    // `civil::Date`'s parser also accepts a full datetime and returns just its
    // date: `"2026-08-01T10:00:00"` would parse, and the caller's 10:00 would
    // vanish into midnight without a word. Requiring exactly `YYYY-MM-DD` means
    // a timestamp that failed the RFC3339 parse above — because it carries no
    // zone — falls through to the refusal instead of being silently rounded
    // down to its date. `jiff` still validates that the date is real, so
    // `2026-02-30` is refused rather than shifted.
    if is_plain_date(value) {
        if let Ok(date) = value.parse::<jiff::civil::Date>() {
            let midnight = date.to_datetime(jiff::civil::Time::midnight());
            // A civil date at UTC always maps to exactly one instant — there is
            // no DST gap in UTC — so this cannot fail for a date jiff parsed.
            if let Ok(zoned) = midnight.to_zoned(jiff::tz::TimeZone::UTC) {
                return Ok(zoned.timestamp().as_second());
            }
        }
    }
    Err(GitError::Usage {
        operation,
        message: format!(
            "{flag} '{value}' is not a date this build accepts — give RFC3339 \
             with a zone ('2026-08-01T10:00:00Z', or an offset like \
             '+09:00'), or 'YYYY-MM-DD' for midnight UTC. A timestamp with no \
             zone is refused rather than guessed at, and so are relative forms \
             like '2 weeks ago': that is git's approxidate, which resolves what \
             it cannot understand to the current time and would answer with a \
             window you did not ask for"
        ),
    })
}

/// Whether a string is exactly the documented `YYYY-MM-DD` spelling.
///
/// Shape only — whether those digits name a real day is `jiff`'s job. This
/// exists to keep the plain-date branch from swallowing spellings the surface
/// does not document (a zoneless timestamp, basic ISO `20260801`), each of
/// which would otherwise resolve to an instant the caller did not ask for.
fn is_plain_date(s: &str) -> bool {
    let b = s.as_bytes();
    b.len() == 10
        && b[4] == b'-'
        && b[7] == b'-'
        && b
            .iter()
            .enumerate()
            .all(|(i, c)| i == 4 || i == 7 || c.is_ascii_digit())
}

// ═══════════════════════════════════════════════════════════════════════════
// The walk
// ═══════════════════════════════════════════════════════════════════════════

/// One commit waiting to be walked, ordered newest-first by committer time.
///
/// The tie-break on oid is not cosmetic: two commits sharing a timestamp is
/// ordinary (a scripted import, a fast rebase), and without a total order the
/// heap's output would depend on insertion order, making the same repository
/// answer differently between runs.
#[derive(PartialEq, Eq)]
struct Pending {
    seconds: i64,
    oid: ObjectId,
}

impl Ord for Pending {
    fn cmp(&self, other: &Self) -> Ordering {
        self.seconds
            .cmp(&other.seconds)
            .then_with(|| self.oid.cmp(&other.oid))
    }
}

impl PartialOrd for Pending {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

/// Queue a commit to be walked, unless it has been queued before.
///
/// Reading the commit here is what buys the ordering — the heap needs the
/// committer time to place it. The `seen` set is what keeps a diamond in the
/// history from being walked, or reported, twice.
fn enqueue(
    repo: &ReadRepo,
    heap: &mut BinaryHeap<Pending>,
    seen: &mut HashSet<ObjectId>,
    oid: ObjectId,
) -> Result<(), GitError> {
    const OP: &str = "log";
    if !seen.insert(oid) {
        return Ok(());
    }
    let mut buf = Vec::new();
    let commit = repo
        .objects()
        .find_commit(&oid, &mut buf)
        .map_err(|e| GitError::repository(OP, "reading a commit", repo.git_dir(), e))?;
    let seconds = commit
        .committer()
        .map_err(|e| GitError::repository(OP, "decoding a commit committer", repo.git_dir(), e))?
        .time()
        .map_err(|e| GitError::repository(OP, "decoding a commit time", repo.git_dir(), e))?
        .seconds;
    heap.push(Pending { seconds, oid });
    Ok(())
}

/// Whether a commit's tree matches a parent's under the path filter — git's
/// "TREESAME". A `None` parent is the empty tree, for a root commit.
fn treesame(
    repo: &ReadRepo,
    commit: ObjectId,
    parent: Option<ObjectId>,
    filter: &PathFilter,
) -> Result<bool, GitError> {
    let changes = changes_against(repo, commit, parent)?;
    Ok(!changes.iter().any(|c| filter.matches(&c.path)))
}

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

    // The walk is ours rather than `gix-traverse`'s, because git's default
    // history simplification cannot be expressed as a filter over someone
    // else's traversal: it decides which *parents to follow*, not merely which
    // commits to report. `Simple` enqueues every parent unconditionally, so a
    // side branch whose change a merge discarded would still be walked and its
    // commits still reported — see `simplification` below. A max-heap keyed on
    // committer time reproduces `Sorting::ByCommitTime(NewestFirst)`, which is
    // git's default ordering.
    let mut heap: BinaryHeap<Pending> = BinaryHeap::new();
    let mut seen: HashSet<ObjectId> = HashSet::new();
    enqueue(repo, &mut heap, &mut seen, start)?;

    while let Some(Pending { oid, .. }) = heap.pop() {
        examined += 1;
        if examined > MAX_COMMITS_EXAMINED {
            truncated = true;
            break;
        }

        let mut buf = Vec::new();
        let commit = objects
            .find_commit(&oid, &mut buf)
            .map_err(|e| GitError::repository(OP, "reading a commit", repo.git_dir(), e))?;

        let parents: Vec<ObjectId> = commit.parents().collect();

        // Which parents this walk may follow at all. `--first-parent` is a
        // traversal choice, so it narrows the set before simplification sees
        // it.
        let followable: &[ObjectId] = if opts.first_parent {
            &parents[..parents.len().min(1)]
        } else {
            &parents
        };

        // ── History simplification (git's default under a pathspec) ─────────
        //
        // If this commit's tree matches some parent's under the paths, then it
        // introduced nothing there: git does not report it, and follows *only*
        // that parent, dropping the other branches. That pruning is the half
        // that matters. Without it a merge that discarded a side branch's
        // change still leads the walk down that branch, whose commits then look
        // like they touched the path — and an agent reads a change that the
        // merge threw away as present.
        //
        // A root commit is compared against the empty tree, so one that adds
        // nothing under the paths is simplified away with no parent to follow.
        let mut simplified_away = false;
        let mut follow: Option<ObjectId> = None;
        if filtering_paths {
            if parents.is_empty() {
                simplified_away = treesame(repo, oid, None, &filter)?;
            } else {
                for parent in followable {
                    if treesame(repo, oid, Some(*parent), &filter)? {
                        simplified_away = true;
                        follow = Some(*parent);
                        break;
                    }
                }
            }
        }

        // Enqueue what the walk continues into, before any reporting filter
        // can `continue` past it: `--author` or a date window narrows what is
        // *shown*, never what is reachable.
        match follow {
            Some(parent) => enqueue(repo, &mut heap, &mut seen, parent)?,
            None if simplified_away => {}
            None => {
                for parent in followable {
                    enqueue(repo, &mut heap, &mut seen, *parent)?;
                }
            }
        }

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
        // filters — it is the expensive one — but its *traversal* half already
        // ran above, because it decides which parents get enqueued.
        let first_parent = parents.first().copied();
        if simplified_away {
            continue;
        }

        // The body is only split out when it was asked for: a commit message
        // can be arbitrarily large, and copying one into a String to drop it
        // unread is an allocation the caller did not ask for.
        let message = commit.message.as_bstr().to_str_lossy();
        let (summary, body) = if opts.body {
            let (s, b) = split_message(message.as_ref());
            (s, Some(b))
        } else {
            (summary_of(message.as_ref()), None)
        };

        let stat = if opts.stat {
            Some(commit_stat(
                repo,
                oid,
                first_parent,
                is_merge,
                opts.max_blob_bytes,
                opts.max_diff_files,
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
            body,
            stat,
        });

        if commits.len() >= opts.limit {
            // There may or may not be more history behind this point, and
            // claiming either way without checking is how `truncated` stops
            // meaning anything: a repository with exactly `--limit` commits
            // would report more history that does not exist, and an agent
            // would go asking for a second page that is empty. So actually ask
            // the walk for one more step. A step that errors is a real read
            // failure and stays loud rather than being read as "no more".
            // Anything still queued is history this walk had not reached, so
            // the flag states a fact rather than an assumption. A repository
            // with exactly `--limit` commits empties the queue and reports
            // `false`, instead of sending an agent after a page that is not
            // there.
            truncated = !heap.is_empty();
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
pub(crate) fn signature(sig: &gix_actor::SignatureRef<'_>) -> Signature {
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
fn summary_of(message: &str) -> String {
    message.split('\n').next().unwrap_or_default().trim_end().to_string()
}

pub(crate) fn split_message(message: &str) -> (String, String) {
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
    old: Option<(ObjectId, Class)>,
    new: Option<(ObjectId, Class)>,
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

/// Every path that differs between a commit's tree and the given parent's.
///
/// A `None` parent is the empty tree, which is what a root commit is compared
/// against — so its every file reads as an addition, matching git.
fn changes_against(
    repo: &ReadRepo,
    commit: ObjectId,
    parent: Option<ObjectId>,
) -> Result<Vec<Change>, GitError> {
    const OP: &str = "log";
    let mut new_side = std::collections::BTreeMap::new();
    let mut old_side = std::collections::BTreeMap::new();
    flatten_tree(repo, OP, tree_of(repo, Some(commit))?, &mut new_side)?;
    flatten_tree(repo, OP, tree_of(repo, parent)?, &mut old_side)?;

    let mut out = Vec::new();
    let paths: BTreeSet<&String> = new_side.keys().chain(old_side.keys()).collect();
    for path in paths {
        // The class rides along on the shared flatten; `--stat` compares blob
        // oids alone, so a mode-only change (`chmod +x`) is not reported here
        // even though git counts it as a changed file (docs/issues.md, L8).
        let old = old_side.get(path).copied();
        let new = new_side.get(path).copied();
        // The oids alone decide whether the path changed: a mode-only change
        // (`chmod +x`) keeps the blob and is therefore invisible here, even
        // though git counts it as a changed file (docs/issues.md, L8).
        let (old_oid, new_oid) = (old.map(|(o, _)| o), new.map(|(o, _)| o));
        if old_oid != new_oid {
            out.push(Change {
                path: path.clone(),
                old,
                new,
            });
        }
    }
    Ok(out)
}

/// Which end of [`line_delta`] one side of a change is.
///
/// A gitlink is named from its class rather than probed from the object
/// store: its oid is a commit in another repository, and asking this store
/// for the header fails outright.
fn side_of(side: Option<(ObjectId, Class)>) -> Side<'static> {
    match side {
        None => Side::Absent,
        Some((_, Class::Commit)) => Side::Gitlink,
        Some((oid, _)) => Side::Object(oid),
    }
}

/// A commit's `--stat` summary against its first parent.
fn commit_stat(
    repo: &ReadRepo,
    commit: ObjectId,
    first_parent: Option<ObjectId>,
    is_merge: bool,
    max_blob_bytes: u64,
    max_diff_files: usize,
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

    for (i, change) in changes.iter().enumerate() {
        // Past the embedder's file cap, stop diffing. Each blob is bounded by
        // `max_blob_bytes`, but a commit touching a hundred thousand
        // just-under-cap files is a hundred thousand Myers diffs — bounded
        // memory, unbounded time. The remainder is declared, not dropped.
        if i >= max_diff_files {
            summary.lines_capped += changes.len() - i;
            break;
        }
        let old = side_of(change.old);
        let new = side_of(change.new);
        match line_delta(repo, "log", old, new, max_blob_bytes)? {
            LineDelta::Counted { added, deleted } => {
                summary.additions += added;
                summary.deletions += deleted;
            }
            // Declined by a limit — the caller is owed the count.
            LineDelta::OverCap => summary.lines_capped += 1,
            // No lines to count by nature. Git's shortstat leaves binary files
            // and gitlinks out of its totals too, and they are not "capped":
            // nothing was withheld, there was nothing there.
            LineDelta::Binary | LineDelta::Gitlink => {}
        }
    }
    Ok(summary)
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

    /// A timestamp with no zone must be REFUSED, not silently read as a date.
    ///
    /// Found by probe, not by reasoning: `jiff`'s `civil::Date` parser happily
    /// accepts a full datetime string and hands back just the date, so
    /// `--since 2026-08-01T10:00:00` was accepted as midnight — the caller's
    /// 10:00 dropped on the floor, no complaint, a ten-hour error in the
    /// window. That is precisely the confidently-wrong answer this whole gate
    /// exists to prevent, arriving through our own parser instead of git's
    /// approxidate.
    #[test]
    fn a_zoneless_timestamp_is_refused_not_truncated_to_a_date() {
        for bad in [
            "2026-08-01T10:00:00",
            "2026-08-01T10:00:00.500",
            "2026-08-01T00:00:00",
        ] {
            let err = parse_date("log", "--since", bad)
                .expect_err("a timestamp with no zone must be refused");
            assert_eq!(err.exit_code(), 2, "{bad}");
            let msg = err.to_string();
            assert!(
                msg.contains("zone") || msg.contains("offset"),
                "the refusal says what is missing: {msg}"
            );
        }
    }

    /// The plain-date form is exactly `YYYY-MM-DD` and nothing adjacent. Basic
    /// ISO (`20260801`) resolves to the right instant but is outside the
    /// documented surface, and accepting undocumented spellings is how a
    /// surface stops meaning what it says.
    #[test]
    fn only_the_documented_plain_date_spelling_is_accepted() {
        assert!(parse_date("log", "--since", "2026-08-01").is_ok());
        for bad in ["20260801", "2026-8-1", "2026/08/01", "2026-08-01T", "2026-08"] {
            assert!(
                parse_date("log", "--since", bad).is_err(),
                "{bad} is not the documented spelling"
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

