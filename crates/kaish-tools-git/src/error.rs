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
    #[error(
        "git {operation}: repository '{repo}' points its {what} outside the \
         mount rooted at '{ceiling}', and kaish-git reads nothing outside that \
         mount. If that is a real directory you meant to expose — a linked \
         worktree whose main repository lives elsewhere is the usual case — \
         mount it too and retry. If it is not, this repository is asking \
         kaish-git to read a path it was never given. Nothing was read"
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

    /// The repository is on disk but malformed, or a file we must read is
    /// unreadable. Exit 1 — a git-level failure about this repository, not a
    /// statement about the environment.
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
            | GitError::NeedsWorktree { .. } => 1,
            GitError::Usage { .. }
            | GitError::NoVerb { .. }
            | GitError::PathspecMagic { .. } => 2,
            GitError::NotRealPath { .. }
            | GitError::UnsupportedRefBackend { .. }
            | GitError::UnsupportedRepositoryFormat { .. }
            | GitError::UnknownExtension { .. }
            | GitError::EscapingInclude { .. }
            | GitError::EscapesMount { .. }
            | GitError::NoContainingMount { .. }
            | GitError::UntrustedRepository { .. } => 4,
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
