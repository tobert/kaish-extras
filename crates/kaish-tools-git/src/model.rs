//! The typed results verbs produce (architecture.md A.1, B).
//!
//! Every value here is owned and free of both gix types and kaish types. That
//! is not tidiness: it is what makes E.3's rule — *no gix value ever crosses
//! an `.await`* — enforceable. A verb opens the repository, works, and
//! produces one of these inside a single blocking closure; nothing `!Send`
//! survives the closure's end.
//!
//! Every implemented verb's model lives here, beside the others — `info`,
//! `status`, `log`, `ls`, `show`, `diff`, `branch`, `tag`, `worktree list`.

use std::collections::BTreeMap;

use serde::Serialize;

/// The ref storage backend a repository uses.
///
/// Only `Files` is reachable: a reftable repository is refused at open time
/// with exit 4 (E.5), so this never carries a backend we did not read.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
#[non_exhaustive]
pub enum RefBackend {
    /// Loose refs under `refs/` plus `packed-refs` — the only backend
    /// gitoxide can read today.
    Files,
}

/// Where HEAD points.
///
/// The three states are all representable, including the one git tools
/// routinely get wrong: an unborn branch, where `branch` names a ref that
/// does not exist yet and `oid` is therefore `None`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Head {
    /// The short branch name HEAD points at, or `None` when detached.
    pub branch: Option<String>,
    /// The commit HEAD resolves to, or `None` on an unborn branch.
    pub oid: Option<String>,
    /// Whether HEAD names an object directly rather than a branch.
    pub detached: bool,
}

/// What this build will let the caller ask for.
///
/// Discoverability, not authority (B.1): the agent learns what it may ask for
/// without gaining any way to change it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Capabilities {
    /// Enabled profile names, e.g. `["read"]`.
    pub profiles: Vec<String>,
    /// Enabled verb names, in schema order.
    pub verbs: Vec<String>,
    /// Cargo features this build was compiled with.
    pub features: Vec<String>,
    /// The embedder's output caps.
    pub limits: LimitsReport,
}

/// The [`crate::Limits`] an embedder configured, as reported to the caller.
///
/// A separate type from `Limits` on purpose: `model` depends on nothing of
/// ours (A.1), and the wire shape should be free to diverge from the
/// embedder-facing struct without either dragging the other.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub struct LimitsReport {
    /// Maximum rows for any row-producing verb.
    pub max_rows: usize,
    /// Maximum files reported by a single diff.
    pub max_diff_files: usize,
    /// Maximum bytes of blob content returned by `git show`.
    pub max_blob_bytes: u64,
    /// Maximum bytes of hunk text per file.
    pub max_hunk_bytes_per_file: u64,
    /// How deep to descend into submodules.
    pub submodule_depth: u8,
}

/// `git info`'s result (architecture.md B.1) — what am I looking at, and what
/// am I allowed to do.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct RepoInfo {
    /// The repository root as a VFS path, or `None` when it cannot be mapped
    /// back into the mount the caller reached it through. An agent should be
    /// told when it cannot name a path it can see.
    pub repo_root_vfs: Option<String>,
    /// The repository root on the host filesystem.
    pub repo_root_real: String,
    /// The git directory on the host filesystem. For a linked worktree this
    /// is the worktree's private git dir, not the common dir.
    pub git_dir: String,
    /// Whether the repository has no working tree.
    pub bare: bool,
    /// Whether history is truncated (`shallow` exists in the common dir).
    pub shallow: bool,
    /// The ref storage backend actually read.
    pub ref_backend: RefBackend,
    /// Where HEAD points.
    pub head: Head,
    /// How many working trees this repository has: the main one, when it has
    /// one, plus every registered linked worktree. A bare repository with no
    /// linked worktrees reports 0.
    pub worktrees: usize,
    /// How many submodules `.gitmodules` declares in the working tree. A bare
    /// repository reports 0 — there is no working tree to read it from.
    pub submodules: usize,
    /// The gitoxide plumbing crates this build links, and their versions.
    /// There is no single facade version to report (A.2), so the pin set is
    /// what an embedder can act on.
    pub gix_pins: BTreeMap<String, String>,
    /// What this build will let the caller ask for.
    pub capabilities: Capabilities,
}

/// One column of a status entry, in JSON's self-describing words (B.2).
///
/// The text surface speaks git's porcelain `XY` letters; this is the other
/// half of decision 9 — words in JSON, so a script never has to know that a
/// leading space means "unmodified" or that `?` means "untracked". Each of the
/// two columns (`index`, `worktree`) takes exactly one of these.
///
/// There is deliberately no `unmerged`/`conflicted` word: a conflict is the
/// `conflicted` boolean on the entry (B.2), and this column then carries the
/// side's own change (`modified`, `added`, `deleted`) so the two halves of the
/// model never disagree about *what* changed, only about whether it conflicts.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
#[non_exhaustive]
pub enum EntryStatus {
    /// No change on this side (`git`'s space in the `XY` pair).
    None,
    /// Added: present on this side, absent from the comparison base.
    Added,
    /// Modified: content changed.
    Modified,
    /// Deleted: removed on this side.
    Deleted,
    /// Renamed from [`StatusEntry::orig_path`] — exact-match only (B.2), never
    /// a similarity score.
    Renamed,
    /// Copied. Never produced by this build — copy detection needs the
    /// `gix-diff` `blob` feature, which is exactly what pulls `gix-command`
    /// (A.2). The variant exists so the word set is git-shaped.
    Copied,
    /// The item's type changed (file ↔ symlink ↔ submodule).
    Typechange,
    /// Untracked: in the worktree, in neither the index nor `.gitignore`.
    Untracked,
    /// Ignored: matched by a `.gitignore` / `info/exclude` rule.
    Ignored,
}

/// What kind of thing a status entry or tree row describes.
///
/// Shared by `git status`'s entries and `git ls`/`git show`'s tree rows
/// (B.2, B.6) rather than each verb naming its own words for the same four
/// ideas — one vocabulary, read once (AGENTS.md, "one term, one meaning").
/// §B.6 spells a tree row's kind in git's own object vocabulary,
/// `blob`/`tree`/`commit`/`symlink`; this reuses `file`/`dir` instead of
/// introducing `blob`/`tree` as second names for them (see the PR 4 entry in
/// architecture.md's Changelog/provenance). `commit` is how a submodule
/// gitlink appears in either verb.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
#[non_exhaustive]
pub enum EntryKind {
    /// A regular file (executable or not).
    File,
    /// A symbolic link.
    Symlink,
    /// A submodule gitlink.
    Commit,
    /// A directory. `git status` produces this only for a collapsed
    /// untracked directory (`--untracked normal`, where git reports a
    /// wholly-untracked directory as a single `path/` row rather than
    /// listing its contents); `git ls` and `git show`'s tree form produce it
    /// for an ordinary subtree row in a non-recursive listing.
    Dir,
}

/// One changed path in a [`StatusReport`] (architecture.md B.2).
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct StatusEntry {
    /// The repository-relative path, slash-separated.
    pub path: String,
    /// The rename source, set only when `index` or `worktree` is
    /// [`EntryStatus::Renamed`]. Exact-match renames only.
    pub orig_path: Option<String>,
    /// What kind of item this is.
    pub kind: EntryKind,
    /// The staged column (index vs `HEAD`).
    pub index: EntryStatus,
    /// The unstaged column (worktree vs index).
    pub worktree: EntryStatus,
    /// Whether this path is unmerged (a nonzero index stage).
    pub conflicted: bool,
    /// The two porcelain letters this entry renders as, `XY` (B.2). Carried on
    /// the model so the text renderer and the JSON words are computed from one
    /// source; skipped from `--json`, which speaks words, not letters.
    #[serde(skip)]
    pub porcelain: [char; 2],
}

/// The five running counts a status reports (architecture.md B.2).
///
/// `staged` and `unstaged` count *columns*, not entries: a path modified and
/// re-modified without staging (git's `MM`) counts in both, exactly as
/// `git status` totals it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Default)]
pub struct StatusTotals {
    /// Entries with a staged change (a non-empty index column).
    pub staged: usize,
    /// Entries with an unstaged change (a non-empty worktree column).
    pub unstaged: usize,
    /// Untracked entries.
    pub untracked: usize,
    /// Ignored entries (only ever nonzero with `--ignored`).
    pub ignored: usize,
    /// Unmerged (conflicted) entries.
    pub conflicted: usize,
}

/// `git status`'s result (architecture.md B.2).
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct StatusReport {
    /// Where HEAD points.
    pub head: Head,
    /// The changed paths, capped at the effective `--limit`.
    pub entries: Vec<StatusEntry>,
    /// The running counts, taken over the *untruncated* set — a total is a
    /// fact about the repository, not about how many rows fit under `--limit`.
    pub totals: StatusTotals,
    /// Whether the working tree is clean: no staged, unstaged, untracked or
    /// conflicted changes. Ignored entries do not make a tree dirty.
    pub clean: bool,
    /// Whether the entry list was truncated by `--limit`. Always reported,
    /// never silent (E.5); a stderr note fires alongside it.
    pub truncated: bool,
}

// ═══════════════════════════════════════════════════════════════════════════
// git log (architecture.md B.3)
// ═══════════════════════════════════════════════════════════════════════════

/// A commit's author or committer: who acted, and when (B.3).
///
/// `time` is RFC3339 with the actor's own timezone offset preserved
/// (`2026-08-01T10:00:00+00:00`) — the instant is always correct, and the
/// offset is the commit's, not the reader's. Native-only: `git log` does not
/// build for wasm, so this reaches for the commit's fixed offset rather than a
/// named-zone table (kaish #225).
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Signature {
    /// The actor's name, as recorded on the commit.
    pub name: String,
    /// The actor's email, as recorded on the commit.
    pub email: String,
    /// When they acted, RFC3339 with the commit's own offset.
    pub time: String,
}

/// A commit's `--stat` summary: how much it changed against its first parent
/// (B.3).
///
/// This is an aggregate over the commit, not a per-file listing — the file
/// count plus total added and deleted lines, exactly what `git log --shortstat`
/// reduces a commit to. A root commit is diffed against the empty tree, so its
/// lines are all additions. A merge (two or more parents) reports zeros: git
/// shows no diffstat for a merge by default, and this matches that.
///
/// Line deltas come from diffing blob contents, so the embedder bounds them
/// twice: `max_blob_bytes` bounds each side that is read, and `max_diff_files`
/// bounds how many of one commit's files are diffed at all. A changed file
/// declined by either cap still counts in `files`, contributes nothing to
/// `additions`/`deletions`, and is counted in `lines_capped` — so the counts
/// are an honest lower bound with the shortfall stated, rather than a lie or an
/// unbounded read.
///
/// `lines_capped` means **we declined to read it**, and nothing else. Binary
/// files and submodule gitlinks count in `files` with no line delta and are
/// *not* counted there: nothing was withheld, there were no lines to withhold.
/// Git's shortstat leaves them out of its totals the same way. Conflating the
/// two would have an agent read `lines_capped: 1` and conclude a file was too
/// large when the repository merely contains a PNG.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Default)]
pub struct StatSummary {
    /// How many files changed against the first parent. The true count, even
    /// when a cap stopped us from diffing all of them.
    pub files: usize,
    /// Total lines added across the files that were diffed.
    pub additions: u64,
    /// Total lines deleted across the files that were diffed.
    pub deletions: u64,
    /// How many changed files a cap kept us from diffing — a side over
    /// `max_blob_bytes`, or a file past `max_diff_files`. Their line deltas are
    /// absent from `additions`/`deletions`. Zero in the common case.
    pub lines_capped: usize,
}

/// One commit in a [`LogReport`] (architecture.md B.3).
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct CommitInfo {
    /// The full commit oid.
    pub oid: String,
    /// The abbreviated oid — the first seven hex characters, git's default.
    pub short_oid: String,
    /// Every parent oid, in order. The first is the mainline; a merge has two
    /// or more. Always the full parent set, even under `--first-parent`, which
    /// changes what is *walked*, not what a commit *is*.
    pub parents: Vec<String>,
    /// Who wrote the change.
    pub author: Signature,
    /// Who committed it (often the author; differs after a rebase or amend).
    pub committer: Signature,
    /// The first line of the commit message.
    pub summary: String,
    /// The full message body below the summary, or `None` unless `--body` was
    /// given. `None` and an empty body are distinct: a commit with only a
    /// summary has no body to show.
    pub body: Option<String>,
    /// The per-commit change summary, or `None` unless `--stat` was given.
    pub stat: Option<StatSummary>,
}

/// `git log`'s result (architecture.md B.3).
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct LogReport {
    /// The starting revision, verbatim as the caller spelled it (`HEAD`, a
    /// branch, a tag, an oid) — not the oid it resolved to.
    pub rev: String,
    /// The commits, newest first, capped at the effective `--limit`.
    pub commits: Vec<CommitInfo>,
    /// Whether the walk stopped at `--limit` with more history behind it.
    /// Always reported, never silent (E.5); a stderr note fires alongside it.
    /// With a filter (`--path`, `--author`, a date window) in effect this means
    /// the walk had more commits to examine, not that more would have matched.
    pub truncated: bool,
}

// ═══════════════════════════════════════════════════════════════════════════
// git ls and git show's tree/blob forms (architecture.md B.5, B.6)
// ═══════════════════════════════════════════════════════════════════════════

/// One row of a tree listing — `git ls` and `git show`'s tree form share this
/// exact shape (architecture.md B.6; "same row shape as `ls`" in B.5), so an
/// agent that has read one has read both.
///
/// §B.6 spells the row `{path, kind: blob|tree|commit(submodule)|symlink, ...}`
/// in git's own object vocabulary. This build reuses [`EntryKind`] instead —
/// `file`/`dir`/`symlink`/`commit` — the vocabulary `git status` already
/// established, rather than introduce `blob`/`tree` as second names for
/// "file" and "dir" (AGENTS.md, "one term, one meaning"; see the Changelog /
/// provenance entry in architecture.md for this PR).
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct TreeRow {
    /// Repo-relative path, slash-separated.
    pub path: String,
    /// What kind of thing this is. [`EntryKind::Commit`] is a submodule
    /// gitlink; [`EntryKind::Dir`] is a subtree, reported as a row only in a
    /// non-recursive listing (a recursive one reports the leaves under it
    /// instead, exactly as `git ls-tree -r` omits the directories it walks
    /// through).
    pub kind: EntryKind,
    /// The git tree mode, six-digit octal (`100644`, `100755`, `120000`,
    /// `160000`, `040000`) — the same string `git ls-tree` prints. A tree's
    /// raw on-disk mode is five digits (`40000`); this pads it to six so the
    /// two agree byte for byte.
    pub mode: String,
    /// The object id this entry names.
    pub oid: String,
    /// The blob's size in bytes, or `null` for a tree or a submodule gitlink
    /// — a made-up zero would claim a size nobody measured.
    pub size: Option<u64>,
}

/// `git ls`'s result (architecture.md B.6).
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct LsReport {
    /// The revision, verbatim as the caller spelled it.
    pub rev: String,
    /// The repo-relative path listed, `""` for the repository root.
    pub path: String,
    /// Whether subtrees were expanded (`--recursive`).
    pub recursive: bool,
    /// The rows, capped at the effective `--limit`.
    pub entries: Vec<TreeRow>,
    /// Whether the listing was truncated by `--limit`. Always reported, never
    /// silent (E.5); a stderr note fires alongside it.
    pub truncated: bool,
}

/// An annotated tag's own metadata, plus the object it points at
/// (architecture.md B.5) — "tag metadata, then the tagged object" as a real
/// field rather than a sentence in prose. `target` is tagged with its own
/// `kind` exactly as [`crate::ShowOutcome`] is at the top level, so a caller
/// reading a nested tag never has to guess what it is looking at either.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ShowTag {
    /// The tag object's own oid — distinct from `target_oid`, which is what
    /// it points at. An agent that reads a tag and wants to name it again
    /// (`show <oid>`) needs this; every other model in this crate reports
    /// its own oid, and a tag should not be the exception.
    pub oid: String,
    /// The tag's own name (`v0.1.0`), independent of the ref that may point
    /// at it.
    pub name: String,
    /// Who created the tag. Absent for a tag object with no tagger line — a
    /// state the git format allows and this reports rather than fabricates.
    pub tagger: Option<Signature>,
    /// The tag's own message, in full.
    pub message: String,
    /// The oid of the object this tag points at, before it is resolved.
    pub target_oid: String,
    /// The tagged object, described the same way `show` would describe it
    /// directly — recursing through a tag-of-a-tag, bounded the same way
    /// [`ReadRepo::tree_of_object`](crate::ReadRepo) bounds its own tag chain.
    pub target: Box<ShowTarget>,
}

/// What a [`ShowTag`] points at, or what a `<rev>:<path>` navigation found
/// under a tree — every case `git show` can resolve to, except the top-level
/// blob case, which carries its bytes outside this model (see
/// [`crate::ShowOutcome`]).
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(tag = "kind", rename_all = "lowercase")]
pub enum ShowTarget {
    /// A commit — the same shape `git log` reports per commit ([`CommitInfo`]
    /// with `body` always populated, `stat` always `None`: this build has no
    /// `--patch`/`--stat` for `show` yet — those are a later phase).
    Commit(CommitInfo),
    /// An annotated tag pointing at another tag.
    Tag(ShowTag),
    /// A tree — the same row shape [`LsReport`] uses for `git ls`.
    Tree(LsReport),
    /// A blob, described but not read: embedding its bytes here would either
    /// blow past `max_blob_bytes` unexamined or require non-UTF-8 content
    /// inside a JSON string. `show <oid>` (or `show <rev>:<path>` naming the
    /// same blob) is the honest way to read it, checked against the cap like
    /// any other blob read.
    Blob {
        /// The blob's oid.
        oid: String,
        /// The blob's real size in bytes.
        size: u64,
    },
}

// ═══════════════════════════════════════════════════════════════════════════
// git diff (architecture.md B.4)
// ═══════════════════════════════════════════════════════════════════════════

/// One end of a comparison (architecture.md B.4).
///
/// Every `git diff` result states both ends, in the text surface and in
/// `--json`, because the endpoint selection is a decision the caller made out
/// of five and an agent reading only rows cannot tell which one it got.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(tag = "kind", rename_all = "lowercase")]
#[non_exhaustive]
pub enum DiffEndpoint {
    /// The index (the staging area), at stage 0.
    Index,
    /// The working tree's files on disk.
    Worktree,
    /// A revision's tree.
    Rev {
        /// The revision, verbatim as the caller spelled it (`HEAD`, a branch,
        /// a tag, `HEAD~2`) — not the oid it resolved to.
        rev: String,
        /// The commit or tree oid it resolved to.
        oid: String,
    },
}

/// What one line of a [`DiffHunk`] does (architecture.md B.4).
///
/// A word, never a sigil. Patch text spells these ` `, `-` and `+`, and a
/// JSON consumer reading that would have to tell a leading space from an
/// empty line — a distinction whitespace-trimming middleware quietly
/// destroys. The word survives.
#[cfg(feature = "textdiff")]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
#[non_exhaustive]
pub enum DiffOp {
    /// Present unchanged on both sides.
    Context,
    /// Present on the `from` side only.
    Delete,
    /// Present on the `to` side only.
    Insert,
}

/// One line inside a [`DiffHunk`] (architecture.md B.4).
#[cfg(feature = "textdiff")]
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct DiffLine {
    /// What this line does.
    pub op: DiffOp,
    /// The line's text, without its trailing newline and without a leading
    /// sigil. Content that is not valid UTF-8 carries U+FFFD in place of the
    /// bytes that were not — see [`DiffFile::hunks`].
    pub text: String,
    /// Whether the file ends here with no trailing newline — the
    /// `\ No newline at end of file` marker in patch text. Absent from
    /// `--json` when false, and only ever true on the last line of a side.
    #[serde(skip_serializing_if = "is_false")]
    pub no_newline: bool,
}

/// Whether to leave a false flag out of the JSON. One line per hunk line is
/// the largest thing this model emits; a field that is false everywhere but
/// two places in a repository is worth spending nothing on.
#[cfg(feature = "textdiff")]
fn is_false(v: &bool) -> bool {
    !*v
}

/// One hunk of a changed file (architecture.md B.4).
///
/// The four numbers are the ones the `@@ -old_start,old_lines
/// +new_start,new_lines @@` header carries, in git's own convention: a side
/// with zero lines reports the line it *follows*, so a new file's hunk is
/// `old_start: 0, old_lines: 0`.
#[cfg(feature = "textdiff")]
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct DiffHunk {
    /// First line of this hunk on the `from` side, 1-based.
    pub old_start: u32,
    /// How many `from`-side lines this hunk covers.
    pub old_lines: u32,
    /// First line of this hunk on the `to` side, 1-based.
    pub new_start: u32,
    /// How many `to`-side lines this hunk covers.
    pub new_lines: u32,
    /// The enclosing declaration, as git's default heading rule finds it: the
    /// nearest preceding `from`-side line whose first character is an ASCII
    /// letter, `_` or `$`, truncated to 80 bytes with trailing whitespace
    /// removed. `null` when there is none above the hunk.
    ///
    /// git's per-language `diff.<driver>.xfuncname` patterns are **not**
    /// consulted: they live in `.gitattributes`, which nothing in this build
    /// reads (D.3). A repository that configures one gets the default rule
    /// here and git's pattern from `git diff`.
    pub section: Option<String>,
    /// The hunk's lines, in patch order.
    pub lines: Vec<DiffLine>,
}

/// One changed path in a [`DiffReport`] (architecture.md B.4).
///
/// `additions`, `deletions` and `binary` are `null` rather than zero whenever
/// nothing was counted — under `--name-only`, or when a cap declined the read.
/// Zero would claim the file changed no lines, which is a different fact from
/// "we did not look".
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct DiffFile {
    /// The repository-relative path on the `to` side, slash-separated. For a
    /// deletion, the path it had on the `from` side.
    pub path: String,
    /// The rename source, set only when `status` is
    /// [`EntryStatus::Renamed`]. Exact-match renames only.
    pub old_path: Option<String>,
    /// What happened to this path: `added`, `deleted`, `modified`,
    /// `renamed`, or `typechange` — the same words `git status` uses for the
    /// same ideas (B.2), so an agent that has read one has read both.
    pub status: EntryStatus,
    /// Whether either side holds binary content (a NUL byte, git's own
    /// heuristic). `null` when nothing was read — under `--name-only`, or
    /// when a cap declined the file.
    pub binary: Option<bool>,
    /// The six-digit octal mode on the `from` side (`100644`, `100755`,
    /// `120000`, `160000`), or `null` when the path is absent there.
    pub old_mode: Option<String>,
    /// The six-digit octal mode on the `to` side, or `null` when the path is
    /// absent there.
    pub new_mode: Option<String>,
    /// The blob oid on the `from` side, or `null` when the path is absent
    /// there or that side is the working tree — working-tree content is not
    /// in the object store, and naming an oid nothing can read would send an
    /// agent to `git show` for an object that is not there. `git diff --raw`
    /// prints all-zeros in the same place.
    pub old_oid: Option<String>,
    /// The blob oid on the `to` side, under the same rule as `old_oid`.
    pub new_oid: Option<String>,
    /// `100` for a rename, `null` for everything else — and never anything
    /// between. Rename detection here is exact-match only: a rename is a blob
    /// oid reappearing at a new path, so the two sides are byte-identical and
    /// 100 is measured, not assumed. A file that was edited *and* moved is
    /// reported as a delete plus an add, where git would have scored it (say
    /// `R087`) and folded the pair. This field will never carry a computed
    /// score — `gix-diff`'s rename tracker is `blob`-gated and `blob` pulls
    /// `gix-command` (A.2). Copy detection does not exist here at all.
    pub similarity: Option<u8>,
    /// Lines added on the `to` side, or `null` when nothing was counted.
    pub additions: Option<u64>,
    /// Lines removed from the `from` side, or `null` when nothing was
    /// counted.
    pub deletions: Option<u64>,
    /// This file's hunks, or `null` when there are none to give: `--patch`
    /// was not asked for, the content is binary, a submodule pointer moved,
    /// only the mode or the path changed, or a cap declined the read.
    /// `binary`, `lines_capped` and `status` say which.
    ///
    /// Hunk text is UTF-8. A side that is not valid UTF-8 but holds no NUL
    /// byte is text to git and to `binary` here, and its hunks carry U+FFFD
    /// where the invalid bytes were — so those hunks read correctly and the
    /// patch rendered from them does not apply. Content with a NUL byte is
    /// `binary: true` and has no hunks at all.
    #[cfg(feature = "textdiff")]
    pub hunks: Option<Vec<DiffHunk>>,
    /// Whether a cap withheld line-level detail for this file. Two caps can
    /// set it, and `additions` says which: a side over the embedder's
    /// `max_blob_bytes` declines the counts too (`additions` is `null`), and
    /// The file counts in `totals.files` either way.
    ///
    /// This is **not** the flag for trimmed hunks — see `hunks_capped`. The
    /// two were one field briefly, disambiguated by whether `additions` was
    /// `null`. That made an agent cross-reference two fields to learn which
    /// of two different things happened, and it contradicted this type's own
    /// rule that `lines_capped` means "we declined to read it, and nothing
    /// else".
    pub lines_capped: bool,
    /// Whether `max_hunk_bytes_per_file` stopped this file's hunks short.
    ///
    /// Distinct from `lines_capped` in the way that matters to a reader: the
    /// counts are **exact** here (`additions` and `deletions` are numbers,
    /// because they are a property of the diff rather than of what was
    /// emitted), and `hunks` holds as many whole hunks as fit — never a
    /// partial hunk, which is not a patch. `false` without the `textdiff`
    /// feature, where there are no hunks to cap.
    pub hunks_capped: bool,
}

/// The running totals a diff reports (architecture.md B.4).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub struct DiffTotals {
    /// How many files are reported. This is the count after `--limit`, and
    /// `truncated` says whether more were found.
    pub files: usize,
    /// Total lines added across the files that were counted, or `null` under
    /// `--name-only`, where none were.
    pub additions: Option<u64>,
    /// Total lines removed, under the same rule as `additions`.
    pub deletions: Option<u64>,
    /// How many reported files carry `lines_capped` — a cap withheld their
    /// counts entirely. Zero in the common case.
    pub lines_capped: usize,
    /// How many reported files carry `hunks_capped` — their counts are exact
    /// but their hunks were cut short. Zero in the common case, and always
    /// zero without the `textdiff` feature.
    pub hunks_capped: usize,
}

/// `git diff`'s result (architecture.md B.4).
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct DiffReport {
    /// The side compared *from*.
    pub from: DiffEndpoint,
    /// The side compared *to*.
    pub to: DiffEndpoint,
    /// The changed paths, sorted by path and capped at the effective
    /// `--limit`.
    pub files: Vec<DiffFile>,
    /// The running totals.
    pub totals: DiffTotals,
    /// How many unmerged (conflicted) index entries were left out of the
    /// comparison. An unmerged path has no stage 0 to compare, so it is
    /// skipped rather than reported as an add or a delete; `git status` is
    /// where its state is reported. Zero in the common case, and a stderr
    /// note fires when it is not.
    pub unmerged: usize,
    /// Whether the file list was truncated by `--limit`. Always reported,
    /// never silent (E.5); a stderr note fires alongside it.
    pub truncated: bool,
}

// ═══════════════════════════════════════════════════════════════════════════
// `git branch` / `git tag` (architecture.md B.7)
// ═══════════════════════════════════════════════════════════════════════════

/// Which namespace a branch row came from.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
#[non_exhaustive]
pub enum BranchKind {
    /// Under `refs/heads/` — a branch in this repository.
    Local,
    /// Under `refs/remotes/` — a remote-tracking branch, last updated by a
    /// fetch this build cannot perform.
    Remote,
}

/// One row of `git branch` (architecture.md B.7).
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct BranchRow {
    /// The branch name with its namespace prefix removed: `main`,
    /// `origin/main`.
    pub name: String,
    /// Which namespace it came from.
    pub kind: BranchKind,
    /// The commit the branch points at.
    pub oid: String,
    /// Whether HEAD is on this branch, in the working tree the caller asked
    /// from. A branch checked out in a *different* worktree is not marked —
    /// `git worktree list` is where that shows up.
    pub is_head: bool,
    /// The configured upstream's short name (`origin/main`), or `null` when
    /// `branch.<name>.remote`/`.merge` name none. Reported whether or not the
    /// upstream ref exists; [`BranchRow::upstream_gone`] says which.
    pub upstream: Option<String>,
    /// Whether [`BranchRow::upstream`] is configured but names a ref this
    /// repository does not have — git renders it `[gone]`. Counts cannot be
    /// computed against a ref that is not there.
    pub upstream_gone: bool,
    /// Commits on this branch that the upstream does not have, or `null`.
    ///
    /// `null` in exactly three cases: `--ahead-behind` was not passed, the
    /// branch has no upstream, or the upstream is gone. Each is visible in
    /// this row or in [`BranchReport::ahead_behind`], so a `null` is never
    /// ambiguous.
    pub ahead: Option<u64>,
    /// Commits the upstream has that this branch does not, or `null` under
    /// the same three conditions as [`BranchRow::ahead`].
    pub behind: Option<u64>,
}

/// `git branch`'s result (architecture.md B.7).
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct BranchReport {
    /// The branches, in full-refname order — `refs/heads/` before
    /// `refs/remotes/`, and alphabetical within each.
    pub branches: Vec<BranchRow>,
    /// Whether `--ahead-behind` was passed, and therefore whether a `null`
    /// count on a row means "not asked for" or "could not be computed".
    pub ahead_behind: bool,
    /// How many commits this invocation's ancestry walks read.
    ///
    /// Zero for a plain listing, which walks nothing. `--contains`,
    /// `--merged` and `--ahead-behind` each cost commits rather than rows, so
    /// this is the number `--limit` does *not* bound — the one an embedder
    /// budgeting a long-lived server needs to see.
    pub commits_examined: u64,
    /// Whether `--limit` cut the listing short.
    pub truncated: bool,
}

/// Whether a tag is a ref pointing straight at its target, or a tag object.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
#[non_exhaustive]
pub enum TagKind {
    /// The ref names the target directly. There is no tag object, so no
    /// tagger and no message.
    Lightweight,
    /// The ref names a tag object carrying a tagger and a message.
    Annotated,
}

/// One row of `git tag` (architecture.md B.7).
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct TagRow {
    /// The tag name with `refs/tags/` removed.
    pub name: String,
    /// The object the ref names: the tag object for an annotated tag, the
    /// target itself for a lightweight one.
    pub oid: String,
    /// Which of the two shapes this is.
    pub kind: TagKind,
    /// What the tag ultimately points at, with every tag object in the chain
    /// peeled away. Equal to [`TagRow::oid`] for a lightweight tag.
    pub target_oid: String,
    /// The kind of object [`TagRow::target_oid`] names — usually `commit`,
    /// but git permits tagging a tree or a blob.
    pub target_kind: String,
    /// Who wrote the tag object, or `null` for a lightweight tag.
    pub tagger: Option<Signature>,
    /// The first line of the tag object's message, or `null` for a
    /// lightweight tag. A lightweight tag has no message of its own; git's
    /// `%(contents:subject)` falls back to the *target commit's* subject,
    /// which would report a line nobody wrote about the tag.
    pub message_summary: Option<String>,
}

/// `git tag`'s result (architecture.md B.7).
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct TagReport {
    /// The tags, in name order.
    pub tags: Vec<TagRow>,
    /// How many commits `--contains`'s ancestry walk read. Zero for a plain
    /// listing — see [`BranchReport::commits_examined`].
    pub commits_examined: u64,
    /// Whether `--limit` cut the listing short.
    pub truncated: bool,
}

// ═══════════════════════════════════════════════════════════════════════════
// `git worktree list` (architecture.md B.9)
// ═══════════════════════════════════════════════════════════════════════════

/// One working tree of a repository (architecture.md B.9).
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct WorktreeRow {
    /// The registration name under `<common>/worktrees/`, or `null` for the
    /// main working tree, which has no registration.
    pub name: Option<String>,
    /// The working tree's path on the host filesystem, exactly as the
    /// repository recorded it.
    pub path_real: String,
    /// The same path in the VFS, or `null` when it falls outside every mount.
    /// An agent should be told when it cannot reach a path it can see.
    pub path_vfs: Option<String>,
    /// The commit this working tree's HEAD resolves to, or `null` on an
    /// unborn branch.
    pub head_oid: Option<String>,
    /// The branch HEAD is on, or `null` when it is detached.
    pub branch: Option<String>,
    /// Whether a `locked` file marks this working tree as not to be removed.
    pub locked: bool,
    /// The text of the `locked` file, or `null` when it is absent or empty.
    pub lock_reason: Option<String>,
    /// Whether the registration outlived what it points at, or `null` when
    /// the working tree is outside the mount and was therefore never
    /// examined. Answering it would mean stat-ing a host path the repository
    /// chose, which is the existence oracle `repo.rs`'s containment refuses
    /// to hand out.
    pub prunable: Option<bool>,
    /// Why it is prunable, when it is.
    pub prunable_reason: Option<String>,
}

/// `git worktree list`'s result (architecture.md B.9).
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct WorktreeReport {
    /// The main working tree first, then every registered linked worktree in
    /// name order — git's own order.
    pub worktrees: Vec<WorktreeRow>,
    /// Whether `--limit` cut the listing short.
    pub truncated: bool,
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The B.1 key names are a wire contract with agents that were shown the
    /// design's example object. A rename here is a silent breaking change for
    /// every prompt that learned the old shape.
    #[test]
    fn repo_info_serializes_with_the_documented_keys() {
        let info = RepoInfo {
            repo_root_vfs: Some("/mnt/repos/kaish".into()),
            repo_root_real: "/srv/kaish".into(),
            git_dir: "/srv/kaish/.git".into(),
            bare: false,
            shallow: false,
            ref_backend: RefBackend::Files,
            head: Head {
                branch: Some("main".into()),
                oid: Some("0".repeat(40)),
                detached: false,
            },
            worktrees: 2,
            submodules: 0,
            gix_pins: BTreeMap::from([("gix-object".to_string(), "0.63.0".to_string())]),
            capabilities: Capabilities {
                profiles: vec!["read".into()],
                verbs: vec!["info".into()],
                features: vec!["read".into()],
                limits: LimitsReport {
                    max_rows: 1000,
                    max_diff_files: 500,
                    max_blob_bytes: 8 * 1024 * 1024,
                    max_hunk_bytes_per_file: 256 * 1024,
                    submodule_depth: 1,
                },
            },
        };
        let json = serde_json::to_value(&info).expect("RepoInfo must serialize");
        for key in [
            "repo_root_vfs",
            "repo_root_real",
            "git_dir",
            "bare",
            "shallow",
            "ref_backend",
            "head",
            "worktrees",
            "submodules",
            "gix_pins",
            "capabilities",
        ] {
            assert!(json.get(key).is_some(), "B.1 key {key} is missing: {json}");
        }
        assert_eq!(json["ref_backend"], "files");
        assert_eq!(json["head"]["branch"], "main");
        assert_eq!(json["head"]["detached"], false);
        for key in ["profiles", "verbs", "features", "limits"] {
            assert!(
                json["capabilities"].get(key).is_some(),
                "capabilities.{key} is missing: {json}"
            );
        }
    }

    /// B.2 in JSON speaks words, and the porcelain letters must not leak into
    /// it — the letters are the *text* surface, and a script reading `--json`
    /// should never have to know that `?` means untracked.
    #[test]
    fn status_entry_serializes_words_not_letters() {
        let entry = StatusEntry {
            path: "src/lib.rs".into(),
            orig_path: None,
            kind: EntryKind::File,
            index: EntryStatus::Modified,
            worktree: EntryStatus::None,
            conflicted: false,
            porcelain: ['M', ' '],
        };
        let json = serde_json::to_value(&entry).expect("StatusEntry serializes");
        assert_eq!(json["index"], "modified");
        assert_eq!(json["worktree"], "none");
        assert_eq!(json["kind"], "file");
        assert!(json["orig_path"].is_null());
        assert!(
            json.get("porcelain").is_none(),
            "the porcelain letters must not appear in JSON: {json}"
        );
    }

    /// The report's key names are the B.2 wire contract, same as B.1's.
    #[test]
    fn status_report_serializes_with_the_documented_keys() {
        let report = StatusReport {
            head: Head {
                branch: Some("main".into()),
                oid: Some("0".repeat(40)),
                detached: false,
            },
            entries: vec![],
            totals: StatusTotals::default(),
            clean: true,
            truncated: false,
        };
        let json = serde_json::to_value(&report).expect("StatusReport serializes");
        for key in ["head", "entries", "totals", "clean", "truncated"] {
            assert!(json.get(key).is_some(), "B.2 key {key} missing: {json}");
        }
        for key in ["staged", "unstaged", "untracked", "ignored", "conflicted"] {
            assert!(
                json["totals"].get(key).is_some(),
                "totals.{key} missing: {json}"
            );
        }
    }

    /// The B.3 key names are the `git log` wire contract, same standing as
    /// B.1's and B.2's. A rename here silently breaks every prompt taught the
    /// documented shape.
    #[test]
    fn log_report_serializes_with_the_documented_keys() {
        let report = LogReport {
            rev: "HEAD".into(),
            commits: vec![CommitInfo {
                oid: "a".repeat(40),
                short_oid: "aaaaaaa".into(),
                parents: vec!["b".repeat(40)],
                author: Signature {
                    name: "Amy".into(),
                    email: "amy@example.invalid".into(),
                    time: "2026-08-01T10:00:00+00:00".into(),
                },
                committer: Signature {
                    name: "Amy".into(),
                    email: "amy@example.invalid".into(),
                    time: "2026-08-01T10:00:00+00:00".into(),
                },
                summary: "fix the thing".into(),
                body: None,
                stat: Some(StatSummary {
                    files: 3,
                    additions: 40,
                    deletions: 7,
                    lines_capped: 0,
                }),
            }],
            truncated: true,
        };
        let json = serde_json::to_value(&report).expect("LogReport serializes");
        for key in ["rev", "commits", "truncated"] {
            assert!(json.get(key).is_some(), "B.3 key {key} missing: {json}");
        }
        let commit = &json["commits"][0];
        for key in [
            "oid",
            "short_oid",
            "parents",
            "author",
            "committer",
            "summary",
            "body",
            "stat",
        ] {
            assert!(commit.get(key).is_some(), "B.3 commit key {key} missing: {commit}");
        }
        for key in ["name", "email", "time"] {
            assert!(commit["author"].get(key).is_some(), "author.{key} missing");
        }
        for key in ["files", "additions", "deletions"] {
            assert!(commit["stat"].get(key).is_some(), "stat.{key} missing");
        }
        assert_eq!(json["truncated"], true);
        assert!(commit["body"].is_null(), "body is null unless --body");
    }

    /// `body` and `stat` are null unless their flag was given — the default
    /// `git log` is a summary, not the whole message and not a diffstat.
    #[test]
    fn body_and_stat_default_to_null() {
        let commit = CommitInfo {
            oid: "a".repeat(40),
            short_oid: "aaaaaaa".into(),
            parents: vec![],
            author: Signature {
                name: "A".into(),
                email: "a@b.invalid".into(),
                time: "2026-08-01T10:00:00+00:00".into(),
            },
            committer: Signature {
                name: "A".into(),
                email: "a@b.invalid".into(),
                time: "2026-08-01T10:00:00+00:00".into(),
            },
            summary: "only a summary".into(),
            body: None,
            stat: None,
        };
        let json = serde_json::to_value(&commit).expect("CommitInfo serializes");
        assert!(json["body"].is_null());
        assert!(json["stat"].is_null());
        assert_eq!(json["parents"].as_array().expect("parents is an array").len(), 0);
    }

    /// An unborn branch is a real state (`git init` then `git info`), and the
    /// honest encoding is a named branch with no oid — not a fabricated zero
    /// oid and not a missing branch.
    #[test]
    fn unborn_head_is_representable() {
        let head = Head {
            branch: Some("main".into()),
            oid: None,
            detached: false,
        };
        let json = serde_json::to_value(&head).expect("Head must serialize");
        assert_eq!(json["branch"], "main");
        assert!(json["oid"].is_null(), "unborn HEAD must report a null oid");
    }
}
