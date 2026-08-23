//! `GitError` and the exit-code taxonomy.
//!
//! The codes are architecture.md E.5, and they sit *inside* kaish's existing
//! contract rather than beside it. Three codes in that contract are the
//! kernel's and are never manufactured here: 3 (output spill), 124 (timeout),
//! 130 (cancel). A `GitError` can only ever produce 1, 2, 4 or 5.
//!
//! Every message names the repository and the operation, because an agent
//! that reads only stderr still has to be able to act on what it read.

use std::path::{Path, PathBuf};

/// Everything this crate can fail with, carrying its own exit code.
///
/// Deliberately not `#[non_exhaustive]`: this enum is matched exhaustively by
/// [`GitError::exit_code`], and a new variant that forgets to declare its code
/// should be a compile error here rather than a silent default somewhere else.
#[derive(Debug, thiserror::Error)]
pub enum GitError {
    /// Discovery walked from the starting path up to the ceiling without
    /// finding a repository. Exit 1 — a git-level "no".
    #[error(
        "git {operation}: no repository at '{start}' or in any parent up to \
         the mount root '{ceiling}' — kaish-git never searches past the mount \
         that contains the path you named"
    )]
    NotARepository {
        /// The verb that was asked for.
        operation: &'static str,
        /// The real path discovery started from.
        start: PathBuf,
        /// The real path discovery was ceilinged at.
        ceiling: PathBuf,
    },

    /// The caller asked for something the verb grammar does not accept.
    /// Exit 2 — POSIX usage, the same code the kernel uses for a clap reject.
    #[error("git {operation}: {message}")]
    Usage {
        /// The verb that was asked for.
        operation: &'static str,
        /// What was wrong, in the caller's own terms.
        message: String,
    },

    /// No verb was selected. Exit 2.
    ///
    /// Its own variant rather than a [`GitError::Usage`] with an empty
    /// operation: at this point there is no verb to name, and the message has
    /// a different job — to list what this build actually offers, since the
    /// enabled set is the embedder's choice and the caller cannot know it.
    #[error("{tool}: a verb is required — {got}. This build offers: {available}")]
    NoVerb {
        /// The name this tool is registered under.
        tool: String,
        /// What was found in the verb position instead.
        got: String,
        /// The verbs this build offers, comma-separated.
        available: String,
    },

    /// A repository inside the mount points at a directory outside it, and
    /// that directory is one we would have had to read. Exit 4.
    ///
    /// Distinct from [`GitError::NotARepository`], which is what an escaping
    /// *discovery* produces, and deliberately so: there, from inside the
    /// mount, no repository exists, and saying more would confirm that one
    /// exists outside. Here a repository *was* legitimately found inside the
    /// mount, and it is the environment that makes it unreadable — a fact its
    /// embedder can act on.
    ///
    /// The escaping path is **not** echoed. It is attacker-controlled content
    /// out of a repository we have not decided to trust, and repeating it back
    /// would turn this refusal into an oracle for probing the host filesystem
    /// (`.git/commondir` says `/etc/shadow`; the error tells you whether that
    /// resolved). The repository and the ceiling are both already known to the
    /// caller, so nothing an honest embedder needs is withheld.
    ///
    /// What the message names instead is a command, not a path: `git
    /// rev-parse --git-common-dir --path-format=absolute`, run inside `repo`
    /// (named explicitly, so the caller knows *where* to run it — the
    /// command answers relative to cwd), gets the escaping directory from a
    /// source the caller trusts — real git on their own machine — rather
    /// than from this refusal. It answers both routes to this variant: a
    /// linked worktree's `gitdir:` target always nests under
    /// `<common-dir>/worktrees/<name>`, so mounting what the command reports
    /// satisfies both the `gitdir:`-line gate ([`crate::repo`]'s
    /// `screen_gitdir_file`) and the `commondir`-content gate below it. That
    /// closes the round trip for the common, legitimate case (a linked
    /// worktree mounted without its main repository — the sibling-worktree
    /// layout kaibo's own PR flow uses) without this crate ever repeating
    /// repository-controlled bytes back to the caller.
    #[error(
        "git {operation}: repository '{repo}' points its {what} outside the \
         mount rooted at '{ceiling}', and kaish-git reads nothing outside that \
         mount. If this is a linked worktree whose main repository lives \
         elsewhere — the usual case — run `git rev-parse --git-common-dir \
         --path-format=absolute` inside '{repo}' to find that repository, \
         then mount it too and retry. If it is not, this repository named a \
         path it was never given. Nothing was read"
    )]
    EscapesMount {
        /// The verb that was asked for.
        operation: &'static str,
        /// Which directory escaped, named as a caller would think of it.
        what: &'static str,
        /// The repository that was found inside the mount.
        repo: PathBuf,
        /// The mount root discovery was ceilinged at.
        ceiling: PathBuf,
    },

    /// The VFS path does not map to a real filesystem path, so there is
    /// nothing for the native git implementation to open. Exit 4.
    #[error(
        "git {operation}: '{vfs_path}' is not on a real filesystem — kaish-git \
         reads through the host filesystem, so it cannot see memory mounts or \
         a backend that has no real path (architecture.md D.5)"
    )]
    NotRealPath {
        /// The verb that was asked for.
        operation: &'static str,
        /// The VFS path that could not be resolved.
        vfs_path: PathBuf,
    },

    /// The repository stores its refs in a backend this crate does not read.
    /// Exit 4, loudly — never a fallback to a stale or empty ref list.
    #[error(
        "git {operation}: unsupported ref backend '{backend}' at '{repo}' — \
         kaish-git reads the 'files' backend only (gitoxide gix-reftable is \
         unimplemented). Nothing was read; this is not a fallback to \
         '.git/refs'"
    )]
    UnsupportedRefBackend {
        /// The verb that was asked for.
        operation: &'static str,
        /// The backend name, as the repository declares it.
        backend: String,
        /// The repository the refusal is about.
        repo: PathBuf,
    },

    /// `core.repositoryformatversion` names a format this crate has not been
    /// taught to read. Exit 4.
    #[error(
        "git {operation}: repository '{repo}' declares \
         core.repositoryformatversion={version} — kaish-git reads format 0 \
         and format 1. Nothing was read"
    )]
    UnsupportedRepositoryFormat {
        /// The verb that was asked for.
        operation: &'static str,
        /// The repository the refusal is about.
        repo: PathBuf,
        /// The declared format version, verbatim.
        version: String,
    },

    /// A format-1 repository declares an extension whose semantics would
    /// change what a read means. Exit 4 — refusing beats answering wrongly.
    #[error(
        "git {operation}: repository '{repo}' declares the unknown extension \
         'extensions.{extension}' — kaish-git does not implement it, and \
         reading the repository as though it were absent could return a wrong \
         answer. Nothing was read"
    )]
    UnknownExtension {
        /// The verb that was asked for.
        operation: &'static str,
        /// The repository the refusal is about.
        repo: PathBuf,
        /// The extension key, without its `extensions.` prefix.
        extension: String,
    },

    /// Repo-local config asks us to include a file outside the repository.
    /// Exit 4. Nothing follows the include — this is the refusal, not a
    /// report of one that happened (architecture.md D.3).
    #[error(
        "git {operation}: config in repository '{repo}' declares \
         '{key} = {value}', which escapes the repository — kaish-git resolves \
         includes itself and follows only repo-relative ones. The include was \
         not read"
    )]
    EscapingInclude {
        /// The verb that was asked for.
        operation: &'static str,
        /// The repository the refusal is about.
        repo: PathBuf,
        /// The full config key, e.g. `include.path`.
        key: String,
        /// The offending value, verbatim.
        value: String,
    },

    /// The path resolves to the host, but no mount claims it — so there is no
    /// mount root to ceiling discovery at, and an unceilinged upward search
    /// is precisely the escape E.2 exists to prevent. Exit 4.
    #[error(
        "git {operation}: no mount contains '{vfs_path}', so kaish-git cannot \
         establish the discovery ceiling it needs — repository discovery is \
         never allowed to search outside the mount that contains the path you \
         named"
    )]
    NoContainingMount {
        /// The verb that was asked for.
        operation: &'static str,
        /// The VFS path with no containing mount.
        vfs_path: PathBuf,
    },

    /// A repository was found, but its ownership does not meet gitoxide's
    /// trust requirement — the same check git spells `safe.directory`.
    /// Exit 4: the repository is fine, the environment is not.
    #[error(
        "git {operation}: repository '{repo}' is not owned by the user running \
         kaish — kaish-git refuses to read a repository it does not trust, and \
         has no equivalent of git's safe.directory to override it. Nothing was \
         read"
    )]
    UntrustedRepository {
        /// The verb that was asked for.
        operation: &'static str,
        /// The repository that was found and refused.
        repo: PathBuf,
    },

    /// A verb that needs a working tree was run against a bare repository.
    /// Exit 1 — a git-level "no", the same class git returns for
    /// `git status` in a bare repo.
    #[error(
        "git {operation}: repository '{repo}' is bare — {operation} needs a \
         working tree, and a bare repository has none"
    )]
    NeedsWorktree {
        /// The verb that was asked for.
        operation: &'static str,
        /// The bare repository.
        repo: PathBuf,
    },

    /// A working-tree file a verb had to read whole is larger than the
    /// embedder's `max_blob_bytes`. Exit 1 — a git-level "no" about this
    /// working tree.
    ///
    /// Loud on purpose, and named on purpose. Skipping the file would make the
    /// answer silently wrong about a tracked path, and reading it would let a
    /// repository choose our allocation size — `git status` hashes every
    /// tracked file, so a multi-GB blob is an OOM in a tool whose whole job is
    /// to be safe to point at a repository you did not write. The path is
    /// inside the mount and the caller can already see it, so naming it leaks
    /// nothing and is the only way an embedder can act on this.
    #[error(
        "git {operation}: working-tree file '{path}' is {size} bytes, over \
         this build's {cap}-byte cap (GitConfig limits, max_blob_bytes) — \
         {operation} hashes every tracked file to compare it against the \
         index, so it will not read this one. Raise the cap to include it"
    )]
    BlobTooLarge {
        /// The verb that was asked for.
        operation: &'static str,
        /// The repo-relative path of the file.
        path: String,
        /// Its size on disk, in bytes.
        size: u64,
        /// The cap it exceeded, in bytes.
        cap: u64,
    },

    /// A tree nests deeper than this build will walk. Exit 1 — a git-level
    /// "no" about this repository.
    ///
    /// Loud rather than truncated. A walk that stopped quietly at the limit
    /// would report every path below it as absent from HEAD — a wrong answer
    /// dressed as a real one. Walking on is not the alternative either: the
    /// walk recurses once per level and the repository picks the number of
    /// levels, so a few hundred nested single-entry trees, cheap to write, are
    /// a stack overflow in a tool whose whole job is to be safe to point at a
    /// repository you did not write.
    ///
    /// Only the limit is named. The depth is repository content, and so is any
    /// path found at it.
    #[error(
        "git {operation}: this repository nests trees more than {limit} levels \
         deep — {operation} walks the tree recursively, and going deeper would \
         exhaust the stack. Nothing below that depth was read"
    )]
    TreeTooDeep {
        /// The verb that was asked for.
        operation: &'static str,
        /// The depth limit this build enforces.
        limit: usize,
    },

    /// The index's cache-tree (the `TREE` extension of `.git/index`) nests
    /// deeper than this build will read. Exit 1 — a git-level "no" about this
    /// repository, the same class as [`GitError::TreeTooDeep`].
    ///
    /// Refused before the index is decoded at all, not after: gitoxide's own
    /// cache-tree decode (`gix_index::extension::tree::decode::one_recursive`)
    /// recurses once per level with no depth bound of its own, so by the time
    /// it would return, a deep enough index has already exhausted the stack.
    /// Nothing this crate does after that call can close it — the check has
    /// to happen first, reading the bytes ourselves (docs/issues.md, R4).
    ///
    /// Only the limit is named, matching `TreeTooDeep`: the depth is
    /// repository content.
    #[error(
        "git {operation}: this repository's index records cached directory \
         info nested more than {limit} levels deep — {operation} would have \
         to decode that structure to read the index, and going deeper would \
         exhaust the stack. Nothing was read"
    )]
    IndexTreeTooDeep {
        /// The verb that was asked for.
        operation: &'static str,
        /// The depth limit this build enforces.
        limit: usize,
    },

    /// The index's cache-tree could not be walked to completion by this
    /// crate's own (non-recursive) depth check — not "too deep", just not
    /// fully accounted for. Exit 1, same class as [`GitError::IndexTreeTooDeep`].
    ///
    /// This crate's depth check and gitoxide's real cache-tree decode are two
    /// independently written readings of the same bytes; where they might
    /// disagree is exactly where refusing matters. A bail here does not mean
    /// gitoxide's decode would also stop — it might read on and recurse
    /// arbitrarily deep on the very same bytes this crate could not finish
    /// walking. So a bail is treated as "cannot certify this is safe," not
    /// "probably fine": refused rather than handed to gitoxide unchecked. A
    /// real index written by real git always parses to completion, so this
    /// costs nothing legitimate.
    #[error(
        "git {operation}: this repository's index records cached directory \
         info that {operation} could not read to the end — an index this \
         build cannot fully account for is refused, not assumed safe. \
         Nothing was read"
    )]
    IndexTreeUnreadable {
        /// The verb that was asked for.
        operation: &'static str,
    },

    /// An index entry's mode names neither a regular file, a symlink, nor a
    /// submodule, so there is no class to compare it on. Exit 4 — the same
    /// class as the other "this build does not model this repository shape"
    /// refusals.
    ///
    /// Refused rather than skipped. A skipped index entry is absent from the
    /// stage-0 map, and a path in HEAD but absent from that map is reported
    /// `deleted` — a confidently wrong answer about a file that is still
    /// there. Mode `040000` is the shape git itself writes here (a
    /// sparse-directory entry in a sparse index); every other mode that
    /// reaches this variant is one git's own `read-cache.c` aborts on
    /// (`BUG: unsupported ce_mode`), so there is no git answer to match.
    ///
    /// The mode is named because it is six octal digits from a fixed
    /// vocabulary, the same thing `git ls-files -s` prints; the path is not,
    /// for the reason the index-path screen gives.
    #[error(
        "git {operation}: repository '{repo}' has an index entry with mode \
         {mode}, which names neither a regular file, a symlink, nor a \
         submodule — kaish-git refuses rather than skip it, because a skipped \
         index entry is reported as a deleted file. Mode 040000 is a \
         sparse-directory entry: run `git sparse-checkout disable` in that \
         repository to expand its index, then retry"
    )]
    UnsupportedIndexMode {
        /// The verb that was asked for.
        operation: &'static str,
        /// The repository the refusal is about.
        repo: PathBuf,
        /// The entry's mode, six octal digits as `git ls-files -s` prints it.
        mode: String,
    },

    /// A `--path` argument used git pathspec magic this crate does not
    /// implement. Exit 2 — usage, and it names the unsupported syntax rather
    /// than silently matching nothing (B, "no git pathspec magic").
    #[error(
        "git {operation}: path '{spec}' uses git pathspec magic \
         ('{magic}'), which kaish-git does not implement — use a literal path \
         or a simple glob (*, **, ?) instead"
    )]
    PathspecMagic {
        /// The verb that was asked for.
        operation: &'static str,
        /// The offending `--path` value, verbatim.
        spec: String,
        /// The magic token that was recognized.
        magic: String,
    },

    /// A `--rev` (or a revision derived from one) names nothing this repository
    /// contains. Exit 1 — a git-level "no such revision", the same class git
    /// returns for an unknown ref or oid.
    ///
    /// The rev is the caller's own argument, echoed so an agent reading only
    /// stderr can see what it typed. It is not repository content — it is what
    /// the caller asked for — so there is no oracle here to withhold.
    #[error(
        "git {operation}: '{rev}' does not name a commit in repository '{repo}' \
         — no such ref, and no object with that id. kaish-git resolves HEAD, a \
         branch, a tag, a full or >=4-char unambiguous oid, and the ~/^ \
         suffixes on any of those"
    )]
    NoSuchRevision {
        /// The verb that was asked for.
        operation: &'static str,
        /// The revision, as the caller spelled it.
        rev: String,
        /// The repository the lookup ran against.
        repo: PathBuf,
    },

    /// A short oid in `--rev` matches more than one object. Exit 1 — a
    /// git-level "no", the same as git's "ambiguous argument".
    #[error(
        "git {operation}: oid prefix '{rev}' is ambiguous in repository \
         '{repo}' — it matches more than one object. Give more characters"
    )]
    AmbiguousRevision {
        /// The verb that was asked for.
        operation: &'static str,
        /// The ambiguous prefix, as the caller spelled it.
        rev: String,
        /// The repository the lookup ran against.
        repo: PathBuf,
    },

    /// A `--rev` used revision syntax outside this crate's small grammar.
    /// Exit 2 — usage, and it names the unsupported form rather than resolving
    /// it to a wrong or surprising commit (B, "Revisions accept a deliberately
    /// small grammar").
    #[error(
        "git {operation}: revision '{spec}' uses '{syntax}', which kaish-git \
         does not parse — the accepted forms are HEAD, a branch, a tag, \
         refs/..., a full or >=4-char oid, and the ~N / ^ / ^N suffixes"
    )]
    UnsupportedRevspec {
        /// The verb that was asked for.
        operation: &'static str,
        /// The offending `--rev` value, verbatim.
        spec: String,
        /// The unsupported token that was recognized.
        syntax: String,
    },

    /// A `<rev>:<path>` navigation (`show`, `ls`) resolved `<rev>` to a blob,
    /// which has no tree to descend into. Exit 1 — a git-level "no", the same
    /// class as `NoSuchRevision`.
    #[error(
        "git {operation}: '{spec}' names a file (a blob), not a commit, tag or \
         tree — there is no tree to read a path from. Repository '{repo}'"
    )]
    NotATree {
        /// The verb that was asked for.
        operation: &'static str,
        /// The revision half of the caller's spec, as spelled.
        spec: String,
        /// The repository the lookup ran against.
        repo: PathBuf,
    },

    /// A `<rev>:<path>` navigation (`show`, `ls`) named a path this tree does
    /// not contain. Exit 1 — a git-level "no", the same class git returns for
    /// a path outside a tree ("path not tracked" in E.5's table).
    #[error(
        "git {operation}: '{path}' is not in the tree at '{rev}' in repository \
         '{repo}' — no entry by that name, and no directory component of it \
         either"
    )]
    NoSuchPath {
        /// The verb that was asked for.
        operation: &'static str,
        /// The revision half of the caller's spec, as spelled.
        rev: String,
        /// The path that was looked up, repo-relative.
        path: String,
        /// The repository the lookup ran against.
        repo: PathBuf,
    },

    /// A flag that only shapes unified-diff text was asked of a build that
    /// assembles none. Exit 4 — an environment/capability gap, the class and
    /// code E.5 gives a `--patch` on a build without `textdiff`.
    ///
    /// Loud and specific on purpose: an agent that passes the flag deserves
    /// "this build cannot", naming the feature and what it will do, not a
    /// generic "unknown flag".
    #[error(
        "git {operation}: {flag} needs unified-diff text, which this build \
         does not assemble — that is the 'textdiff' feature, added in a later \
         phase, where it will render hunks and patch text from the same model \
         this verb already returns. {instead}"
    )]
    PatchNeedsTextdiff {
        /// The verb that was asked for.
        operation: &'static str,
        /// The flag that was refused, as the caller spelled it.
        flag: &'static str,
        /// What to reach for instead, in this verb.
        instead: &'static str,
    },

    /// A verb was asked for patch text it does not assemble, on a build that
    /// does assemble patch text elsewhere. Exit 4 — the same capability-gap
    /// class as [`GitError::PatchNeedsTextdiff`], and distinct from it on
    /// purpose: naming the `textdiff` feature as the fix would be a lie when
    /// the feature is already on.
    #[error("git {operation}: {flag} is not available here — {operation} assembles no patch text. {instead}")]
    PatchNotInThisVerb {
        /// The verb that was asked for.
        operation: &'static str,
        /// The flag that was refused, as the caller spelled it.
        flag: &'static str,
        /// What to reach for instead.
        instead: &'static str,
    },

    /// The repository is on disk but malformed, or a file we must read is
    /// unreadable. Exit 1 — a git-level failure about this repository, not a
    /// statement about the environment.
    /// An ancestry question read more commits than this build will spend on
    /// one invocation. Exit 1 — a git-level "not in this repository", the same
    /// class as [`GitError::TreeTooDeep`].
    ///
    /// A refusal rather than a partial answer, because `--contains`,
    /// `--merged` and `--ahead-behind` have no partial form: a branch dropped
    /// because the walk gave up looking would read as a branch that does not
    /// match.
    #[error(
        "git {operation}: answering this needed more than {limit} commits of \
         history, so nothing was reported rather than part of it. Ask without \
         --contains / --merged / --ahead-behind for the listing itself, which \
         reads no history"
    )]
    AncestryBudgetExhausted {
        /// The verb that was asked for.
        operation: &'static str,
        /// The commit-read limit this build enforces.
        limit: u64,
    },

    #[error("git {operation}: {what} at '{path}': {source}")]
    Repository {
        /// The verb that was asked for.
        operation: &'static str,
        /// What we were doing, e.g. "reading HEAD".
        what: String,
        /// The path involved.
        path: PathBuf,
        /// The underlying failure.
        source: Box<dyn std::error::Error + Send + Sync>,
    },

    /// The verb exists in the binary but is not enabled by this embedder's
    /// profile. Exit 5. Belt-and-braces: a disabled verb is absent from the
    /// schema, so the kernel has nothing to route to and this should be
    /// unreachable through normal dispatch.
    #[error(
        "git {operation}: the verb is not enabled by this build's profile — \
         the embedder's GitConfig does not include it, and no argument can \
         turn it on"
    )]
    VerbNotEnabled {
        /// The verb that was asked for.
        operation: &'static str,
    },
}

impl GitError {
    /// The process exit code for this failure (architecture.md E.5).
    ///
    /// Codes 3, 124 and 130 belong to the kernel (output spill, timeout,
    /// cancel) and are deliberately unreachable from here.
    pub fn exit_code(&self) -> i64 {
        match self {
            GitError::NotARepository { .. }
            | GitError::Repository { .. }
            | GitError::NeedsWorktree { .. }
            | GitError::BlobTooLarge { .. }
            | GitError::TreeTooDeep { .. }
            | GitError::IndexTreeTooDeep { .. }
            | GitError::IndexTreeUnreadable { .. }
            | GitError::NoSuchRevision { .. }
            | GitError::AmbiguousRevision { .. }
            | GitError::NotATree { .. }
            | GitError::AncestryBudgetExhausted { .. }
            | GitError::NoSuchPath { .. } => 1,
            GitError::Usage { .. }
            | GitError::NoVerb { .. }
            | GitError::PathspecMagic { .. }
            | GitError::UnsupportedRevspec { .. } => 2,
            GitError::NotRealPath { .. }
            | GitError::UnsupportedRefBackend { .. }
            | GitError::UnsupportedRepositoryFormat { .. }
            | GitError::UnknownExtension { .. }
            | GitError::EscapingInclude { .. }
            | GitError::EscapesMount { .. }
            | GitError::NoContainingMount { .. }
            | GitError::UntrustedRepository { .. }
            | GitError::PatchNeedsTextdiff { .. }
            | GitError::PatchNotInThisVerb { .. }
            | GitError::UnsupportedIndexMode { .. } => 4,
            GitError::VerbNotEnabled { .. } => 5,
        }
    }

    /// Build a [`GitError::Repository`] from an underlying error.
    pub fn repository(
        operation: &'static str,
        what: impl Into<String>,
        path: impl AsRef<Path>,
        source: impl std::error::Error + Send + Sync + 'static,
    ) -> Self {
        GitError::Repository {
            operation,
            what: what.into(),
            path: path.as_ref().to_path_buf(),
            source: Box::new(source),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The three kernel-owned codes must be unreachable from `GitError`. This
    /// is the taxonomy's whole point: a tool that manufactured 130 would tell
    /// an embedder its execution was cancelled when it was not.
    #[test]
    fn no_variant_claims_a_kernel_owned_code() {
        let repo = PathBuf::from("/repo");
        let variants = [
            GitError::NotARepository {
                operation: "info",
                start: repo.clone(),
                ceiling: repo.clone(),
            },
            GitError::Usage {
                operation: "info",
                message: "bad".into(),
            },
            GitError::NoVerb {
                tool: "git".into(),
                got: "none was given".into(),
                available: "info".into(),
            },
            GitError::NotRealPath {
                operation: "info",
                vfs_path: repo.clone(),
            },
            GitError::UnsupportedRefBackend {
                operation: "info",
                backend: "reftable".into(),
                repo: repo.clone(),
            },
            GitError::UnsupportedRepositoryFormat {
                operation: "info",
                repo: repo.clone(),
                version: "2".into(),
            },
            GitError::UnknownExtension {
                operation: "info",
                repo: repo.clone(),
                extension: "worktreeconfig".into(),
            },
            GitError::EscapingInclude {
                operation: "info",
                repo: repo.clone(),
                key: "include.path".into(),
                value: "/etc/passwd".into(),
            },
            GitError::EscapesMount {
                operation: "info",
                what: "common directory",
                repo: repo.clone(),
                ceiling: PathBuf::from("/mnt"),
            },
            GitError::NoContainingMount {
                operation: "info",
                vfs_path: repo.clone(),
            },
            GitError::UntrustedRepository {
                operation: "info",
                repo: repo.clone(),
            },
            GitError::Repository {
                operation: "info",
                what: "reading HEAD".into(),
                path: repo.clone(),
                source: Box::new(std::io::Error::other("boom")),
            },
            GitError::VerbNotEnabled { operation: "info" },
            GitError::NeedsWorktree {
                operation: "status",
                repo: repo.clone(),
            },
            GitError::PathspecMagic {
                operation: "status",
                spec: ":(exclude)src".into(),
                magic: ":(".into(),
            },
            GitError::BlobTooLarge {
                operation: "status",
                path: "big.bin".into(),
                size: 4096,
                cap: 64,
            },
            GitError::IndexTreeTooDeep {
                operation: "status",
                limit: 256,
            },
            GitError::IndexTreeUnreadable { operation: "status" },
            GitError::NoSuchRevision {
                operation: "log",
                rev: "nonesuch".into(),
                repo: repo.clone(),
            },
            GitError::AmbiguousRevision {
                operation: "log",
                rev: "abcd".into(),
                repo: repo.clone(),
            },
            GitError::UnsupportedRevspec {
                operation: "log",
                spec: "A..B".into(),
                syntax: "..".into(),
            },
            GitError::NotATree {
                operation: "show",
                spec: "HEAD:README.md".into(),
                repo: repo.clone(),
            },
            GitError::NoSuchPath {
                operation: "show",
                rev: "HEAD".into(),
                path: "nonesuch".into(),
                repo: repo.clone(),
            },
            GitError::PatchNeedsTextdiff {
                operation: "log",
                flag: "--patch",
                instead: "Use --stat.",
            },
            GitError::PatchNotInThisVerb {
                operation: "log",
                flag: "--patch",
                instead: "Use git diff --patch.",
            },
            GitError::UnsupportedIndexMode {
                operation: "status",
                repo: repo.clone(),
                mode: "040000".into(),
            },
        ];
        for e in &variants {
            let code = e.exit_code();
            assert!(
                matches!(code, 1 | 2 | 4 | 5),
                "{e} claimed exit {code}; only 1/2/4/5 belong to this crate"
            );
        }
    }

    /// The escape refusal must never echo the path it refused. Repeating
    /// attacker-supplied content back would turn the error into an oracle:
    /// point `.git/commondir` at a candidate path and read the reply to learn
    /// whether it exists.
    #[test]
    fn the_escape_refusal_does_not_echo_the_escaping_path() {
        let err = GitError::EscapesMount {
            operation: "info",
            what: "common directory (.git/commondir)",
            repo: PathBuf::from("/mnt/repo/.git"),
            ceiling: PathBuf::from("/mnt"),
        };
        let msg = err.to_string();
        assert_eq!(err.exit_code(), 4);
        assert!(msg.contains("/mnt/repo/.git"), "must name the repo: {msg}");
        assert!(msg.contains("outside the mount"), "must say what happened: {msg}");
        assert!(
            msg.contains("mount it too"),
            "must tell a legitimate embedder the fix: {msg}"
        );
        assert!(
            msg.contains("linked worktree"),
            "must name the legitimate shape it might be: {msg}"
        );
        assert!(
            msg.contains("Nothing was read"),
            "must state that no read happened: {msg}"
        );
        // The recovery command, not the escaping path itself: an embedder
        // finds the directory to mount from a source they trust (real git on
        // their own machine) instead of this refusal echoing
        // repository-controlled bytes. Pinned together with "inside
        // '{repo}'" rather than as two separate substring checks, so a future
        // edit cannot keep the command while quietly dropping the clause
        // that says where to run it — `git rev-parse` answers relative to
        // cwd, so the location is not optional detail.
        assert!(
            msg.contains(
                "run `git rev-parse --git-common-dir --path-format=absolute` \
                 inside '/mnt/repo/.git' to find that repository"
            ),
            "must name the command AND where to run it, from a trusted \
             source, without echoing the escaping path ourselves: {msg}"
        );
        // The other reading, kept deliberately. Without it the refusal reads
        // as pure friction: an operator who did NOT mean to expose anything
        // needs to know this repository asked for a path it was never given,
        // because their next step is to investigate it, not to mount it.
        assert!(
            msg.contains("this repository named a path it was never given"),
            "must name the hostile reading too, so the fix is not the only \
             next step on offer: {msg}"
        );
    }

    /// E.5: "errors name the repository and the operation". A message that
    /// names neither leaves an agent reading stderr with nothing to act on.
    #[test]
    fn every_message_names_the_operation_and_a_path() {
        let err = GitError::UnsupportedRefBackend {
            operation: "info",
            backend: "reftable".into(),
            repo: PathBuf::from("/srv/repos/kaish"),
        };
        let msg = err.to_string();
        assert!(msg.starts_with("git info:"), "must name the verb: {msg}");
        assert!(msg.contains("/srv/repos/kaish"), "must name the repo: {msg}");
        assert!(msg.contains("reftable"), "must name the backend: {msg}");
        assert!(
            msg.contains("not a fallback"),
            "the reftable refusal must say it did not fall back: {msg}"
        );
    }
}
