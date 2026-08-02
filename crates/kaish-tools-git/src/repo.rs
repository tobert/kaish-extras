//! Repository discovery and [`ReadRepo`] — the only place the gitoxide
//! plumbing handles are assembled (architecture.md A.1, D.1, D.2, E.2).
//!
//! There is no facade `Repository` to hand out. `ReadRepo` builds an object
//! store, a ref store and a parsed repo-local config, keeps all three
//! private, and never lends them. A caller cannot reach a ref transaction or
//! an index writer because it cannot reach the handle that would offer one —
//! the read-only property is the shape of the type, not a check inside it.
//!
//! Isolation is likewise structural (D.2): nothing is loaded that we did not
//! choose to load. No system config, no user config, no `GIT_*` environment
//! reading, no attributes stack, no credential helpers. Repo-local config is
//! parsed from bytes we read ourselves with `from_bytes_no_includes`, so no
//! library code path can follow an `include.path` — we resolve includes, or
//! refuse them.

use std::path::{Component, Path, PathBuf};

use gix_ref::file::ReferenceExt;

use crate::error::GitError;
use crate::model::{Head, RefBackend};

/// The `extensions.*` keys this crate is willing to read a repository under.
///
/// Lowercase, because git compares config key names case-insensitively.
/// Anything absent from this list is refused with exit 4 rather than ignored:
/// an extension exists precisely because it changes what the on-disk data
/// means, so reading past one we do not implement is how a tool returns a
/// confidently wrong answer.
///
/// Deliberately *not* here, and why:
/// - `worktreeconfig` — moves `core.*` into `config.worktree`, which we do
///   not read, so `bare` and the worktree root could both be wrong.
/// - `partialclone` — objects may be absent and fetched on demand; this build
///   has no transport, so a read would fail in a way that looks like repo
///   corruption.
/// - `relativeworktrees` — changes how `gitdir` files resolve. Probably fine,
///   unverified; refusing is the honest state of that knowledge.
const SUPPORTED_EXTENSIONS: &[&str] = &[
    // git's own no-op test extension.
    "noop",
    // Accepted, but only at value `sha1` — see `check_object_format`.
    "objectformat",
    // Accepted, but only at value `files` — see `check_ref_backend`.
    "refstorage",
    // Tells `gc` not to delete objects. Read semantics are unchanged.
    "preciousobjects",
];

/// A repository opened for reading, and nothing else.
///
/// Every field is private and stays private. The accessors return owned
/// values from [`crate::model`]; none of them lends out a gix handle, which
/// is what keeps E.3's rule ("no gix value ever crosses an `.await`")
/// enforceable at the type level rather than by discipline.
pub struct ReadRepo {
    /// The verb this repository was opened for, so errors can name it.
    operation: &'static str,
    /// The git directory. For a linked worktree this is the worktree's
    /// private dir under `<common>/worktrees/<name>`.
    git_dir: PathBuf,
    /// Where `objects/`, `refs/` and `packed-refs` live. Equal to `git_dir`
    /// except inside a linked worktree.
    common_dir: PathBuf,
    /// The working tree root, or `None` for a bare repository.
    work_dir: Option<PathBuf>,
    objects: gix_odb::Handle,
    refs: gix_ref::file::Store,
}

impl ReadRepo {
    /// Discover a repository at or above `real_path`, never above `ceiling`.
    ///
    /// `ceiling` is the real root of the VFS mount that contains the caller's
    /// path (E.2). Upward discovery is an ergonomic v2 gained — `git info`
    /// from a subdirectory works, where v1's exact-root open did not — and
    /// the ceiling is what keeps that new capability from becoming an escape
    /// out of the sandbox.
    ///
    /// The ceiling is enforced twice, deliberately, because the two
    /// mechanisms fail differently:
    ///
    /// 1. `ceiling_dirs` makes gix-discover stop walking, so we do not even
    ///    stat directories outside the mount.
    /// 2. The discovered git directory is then checked to be inside the
    ///    ceiling. This is not belt-and-braces decoration: gix-discover
    ///    computes its ceiling height by *lexical* prefix match, so a ceiling
    ///    it cannot match (notably when the start path **is** the mount root,
    ///    where the computed height is zero and the ceiling is dropped)
    ///    silently becomes an unbounded walk. The post-check is what makes
    ///    "cannot escape the mount" true in every case rather than in the
    ///    common one.
    pub fn discover(
        operation: &'static str,
        real_path: &Path,
        ceiling: &Path,
    ) -> Result<Self, GitError> {
        let options = gix_discover::upwards::Options {
            ceiling_dirs: vec![ceiling.to_path_buf()],
            // `false` on purpose. With `true`, a ceiling that does not
            // strictly contain the start path is an error — and the start
            // path *equal to* the mount root is the ordinary case of
            // `git info` at the top of a mount. Turning that into an error
            // would refuse the most common invocation there is; the
            // post-check below is what covers the case instead.
            match_ceiling_dir_or_error: false,
            // Hermetic: gix falls back to `std::env::current_dir()` when this
            // is `None`, and the kernel never lets a tool read process state.
            // Both paths we pass are absolute, so this only ever serves as
            // the normalization base.
            current_dir: Some(ceiling),
            ..Default::default()
        };

        let (path, _trust) = gix_discover::upwards_opts(real_path, options).map_err(|e| {
            use gix_discover::upwards::Error;
            match e {
                Error::NoGitRepository { .. }
                | Error::NoGitRepositoryWithinCeiling { .. }
                | Error::NoGitRepositoryWithinFs { .. }
                | Error::NoMatchingCeilingDir
                | Error::InaccessibleDirectory { .. }
                | Error::InvalidInput { .. } => GitError::NotARepository {
                    operation,
                    start: real_path.to_path_buf(),
                    ceiling: ceiling.to_path_buf(),
                },
                Error::NoTrustedGitRepository { candidate, .. } => {
                    GitError::UntrustedRepository {
                        operation,
                        repo: candidate,
                    }
                }
                // CurrentDir and CheckTrust are both io failures against the
                // host; they are about this path, not about the environment.
                Error::CurrentDir(source) | Error::CheckTrust { err: source, .. } => {
                    GitError::repository(operation, "discovering the repository", real_path, source)
                }
            }
        })?;

        let (git_dir, work_dir) = path.into_repository_and_work_tree_directories();

        // ── The ceiling invariant ───────────────────────────────────────────
        //
        // Every directory this type will read from must be inside the ceiling,
        // as the operating system resolves it.
        //
        // Two things make that harder than one `starts_with`. First, discovery
        // is only one of three ways a directory gets chosen here, and the
        // other two are named by *repository content* — attacker-controlled
        // the moment you open a repository you did not create:
        //
        //   git_dir     — discovery, but a `.git` *file* can name it
        //                 (`gitdir: /anywhere`), so it is repo-controlled too
        //   common_dir  — `<git_dir>/commondir`, a file whose entire content
        //                 is a path, absolute or `..`-bearing
        //   work_dir    — discovery's physical parent today
        //
        // Second, a repository owns every byte under its own `.git`, symlinks
        // included, and a lexical check walks straight through one: a
        // `commondir` of `evil` where `.git/evil` is a symlink is inside the
        // ceiling to a string comparison and outside it to `openat`. So every
        // path — and the ceiling itself — is resolved with `canonicalize`
        // before it is compared, and the comparison happens before any of them
        // is read.
        let ceiling = real_dir(operation, "mount root", ceiling)?;
        let git_dir = real_dir(operation, "git directory", &git_dir)?;
        let work_dir = work_dir
            .as_deref()
            .map(|w| real_dir(operation, "working tree", w))
            .transpose()?;

        if !git_dir.starts_with(&ceiling) {
            // Two very different situations reach here, and collapsing them
            // would either leak or mislead.
            //
            // If a working tree was found *inside* the mount, then a `.git`
            // file inside the sandbox named a git dir outside it — the same
            // shape as the `commondir` case below, and the ordinary result of
            // mounting only a linked worktree and not its main repository.
            // The caller already knows about the path we name, so saying so
            // costs nothing and tells an embedder how to fix their layout.
            //
            // Otherwise discovery itself walked out of the mount, and from
            // inside the mount there simply is no repository. That is all we
            // say: confirming one exists above would answer a question the
            // caller was never entitled to ask.
            if let Some(work_dir) = work_dir.filter(|w| w.starts_with(&ceiling)) {
                return Err(GitError::EscapesMount {
                    operation,
                    what: "git directory (the `gitdir:` line in its .git file)",
                    repo: work_dir,
                    ceiling,
                });
            }
            return Err(GitError::NotARepository {
                operation,
                start: real_path.to_path_buf(),
                ceiling,
            });
        }

        // `commondir` is read out of `git_dir`, which the check above has
        // already placed inside the ceiling — so this read is safe to make
        // before the result is trusted. `real_dir` is what makes the check
        // that follows meaningful: `resolve_common_dir` only joins and
        // normalizes lexically, and the path it produces may traverse a
        // symlink the repository put there.
        let common_dir = real_dir(
            operation,
            "common directory",
            &resolve_common_dir(operation, &git_dir)?,
        )?;
        if !common_dir.starts_with(&ceiling) {
            // Different from the case above, and a different code: a
            // repository *was* found inside the mount. Either it is a linked
            // worktree whose main repository legitimately lives outside — the
            // embedder mounts that too — or it is a repository trying to point
            // us at a path we were never given. We cannot tell the two apart,
            // and under the sandbox model neither is readable, so both are
            // refused with a message that explains what to do about the honest
            // one.
            return Err(GitError::EscapesMount {
                operation,
                what: "common directory (.git/commondir)",
                repo: git_dir,
                ceiling,
            });
        }
        if let Some(work_dir) = work_dir.as_deref() {
            // Defense in depth. Today `work_dir` is discovery's own physical
            // parent and is bounded by the `git_dir` check above — see
            // `work_dir_is_bounded_by_discovery` in tests/hostile_repo.rs for
            // the reasoning and for what would invalidate it. The check costs
            // one `starts_with` and it is what stands between a future
            // `core.worktree` reader and `submodule_count` reading
            // `<work_dir>/.gitmodules` off an arbitrary host path.
            if !work_dir.starts_with(&ceiling) {
                return Err(GitError::EscapesMount {
                    operation,
                    what: "working tree",
                    repo: git_dir,
                    ceiling,
                });
            }
        }

        // Read repo-local config before opening anything. A repository we are
        // going to refuse must be refused before we touch its objects.
        let config = read_repo_config(operation, &common_dir)?;
        check_include_paths(operation, &common_dir, &config)?;
        let format_version = check_format_version(operation, &common_dir, &config)?;
        if format_version >= 1 {
            check_extensions(operation, &common_dir, &config)?;
            check_object_format(operation, &common_dir, &config)?;
        }
        // The ref backend check does not wait for format version 1: a
        // `reftable/` directory on disk is evidence regardless of what the
        // config claims, and answering from an empty `refs/` would be the
        // silent fallback E.5 forbids.
        check_ref_backend(operation, &common_dir, &config)?;

        let objects = gix_odb::at(common_dir.join("objects")).map_err(|e| {
            GitError::repository(operation, "opening the object store", &common_dir, e)
        })?;

        // `WriteReflog::Disable` is not an optimization. The files ref store
        // appends to a reflog on ref *writes*; we make none, and setting this
        // means a future write-shaped mistake cannot quietly log its way into
        // the fingerprint test's `.git` diff either.
        let ref_options = gix_ref::store::init::Options {
            write_reflog: gix_ref::store::WriteReflog::Disable,
            prohibit_windows_device_names: cfg!(windows),
            ..Default::default()
        };
        let refs = if git_dir == common_dir {
            gix_ref::file::Store::at(git_dir.clone(), ref_options)
        } else {
            // A linked worktree: HEAD is private to the worktree, everything
            // else comes from the common dir.
            gix_ref::file::Store::for_linked_worktree(
                git_dir.clone(),
                common_dir.clone(),
                ref_options,
            )
        };

        Ok(Self {
            operation,
            git_dir,
            common_dir,
            work_dir,
            objects,
            refs,
        })
    }

    /// The git directory on the host filesystem.
    pub fn git_dir(&self) -> &Path {
        &self.git_dir
    }

    /// The repository root: the working tree when there is one, else the git
    /// directory.
    pub fn root(&self) -> &Path {
        self.work_dir.as_deref().unwrap_or(&self.git_dir)
    }

    /// Whether the repository has no working tree.
    ///
    /// Derived from what discovery found, not from `core.bare`. Every other
    /// path in this result is derived the same way, and a `bare: true` sitting
    /// beside a `repo_root_real` that is plainly a working tree would be worse
    /// than either answer alone.
    pub fn is_bare(&self) -> bool {
        self.work_dir.is_none()
    }

    /// Whether history is truncated.
    pub fn is_shallow(&self) -> bool {
        self.common_dir.join("shallow").exists()
    }

    /// The ref backend actually read.
    ///
    /// Always `Files`: [`ReadRepo::discover`] refuses anything else with exit
    /// 4, so this can never report a backend whose data we did not read.
    pub fn ref_backend(&self) -> RefBackend {
        RefBackend::Files
    }

    /// Where HEAD points.
    ///
    /// All three states are distinguished: attached to a branch with a
    /// commit, attached to an unborn branch (`oid` is `None`), and detached.
    pub fn head(&self) -> Result<Head, GitError> {
        let head = self.refs.find("HEAD").map_err(|e| {
            GitError::repository(self.operation, "reading HEAD", &self.git_dir, e)
        })?;

        let branch = match &head.target {
            gix_ref::Target::Symbolic(name) => Some(name.shorten().to_string()),
            gix_ref::Target::Object(_) => None,
        };
        let detached = branch.is_none();

        // An unborn branch — `git init` then nothing — is a ref that does not
        // exist yet. Asking for its object would report "not found", which
        // reads as a broken repository rather than a new one.
        if let gix_ref::Target::Symbolic(name) = &head.target {
            let exists = self
                .refs
                .try_find(name.as_ref())
                .map_err(|e| {
                    GitError::repository(
                        self.operation,
                        format!("reading ref '{}'", name.as_bstr()),
                        &self.common_dir,
                        e,
                    )
                })?
                .is_some();
            if !exists {
                return Ok(Head {
                    branch,
                    oid: None,
                    detached,
                });
            }
        }

        let mut head = head;
        let oid = head
            .peel_to_id(&self.refs, &self.objects)
            .map_err(|e| GitError::repository(self.operation, "resolving HEAD", &self.git_dir, e))?;

        Ok(Head {
            branch,
            oid: Some(oid.to_string()),
            detached,
        })
    }

    /// How many working trees this repository has.
    ///
    /// The main one, when it has one, plus every registered linked worktree.
    /// "Has a main working tree" is read off the common dir being named
    /// `.git` — gitoxide's own `DOT_GIT_DIR` convention. A repository whose
    /// git directory is named something else is reported without its main
    /// working tree; that shape needs the `core.worktree` handling this build
    /// does not have.
    pub fn worktree_count(&self) -> Result<usize, GitError> {
        let mut count = usize::from(self.common_dir.file_name() == Some(std::ffi::OsStr::new(".git")));

        let dir = self.common_dir.join("worktrees");
        match std::fs::read_dir(&dir) {
            Ok(entries) => {
                for entry in entries {
                    let entry = entry.map_err(|e| {
                        GitError::repository(self.operation, "listing linked worktrees", &dir, e)
                    })?;
                    // A registered worktree is a directory carrying a
                    // `gitdir` file; git leaves other things in here.
                    if entry.path().join("gitdir").is_file() {
                        count += 1;
                    }
                }
            }
            // Only "no worktrees directory" is swallowed, and it is not an
            // error: a repository that never had a linked worktree has none.
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => {
                return Err(GitError::repository(
                    self.operation,
                    "listing linked worktrees",
                    &dir,
                    e,
                ))
            }
        }
        Ok(count)
    }

    /// How many submodules the working tree's `.gitmodules` declares.
    ///
    /// `.gitmodules` is the file git itself treats as the submodule registry,
    /// and it lives in the working tree — a bare repository reports 0 because
    /// there is no working tree to read it from. Parsed with
    /// `from_bytes_no_includes` like every other config here; an `include.path`
    /// inside it is not followed (git does not honor one there either).
    pub fn submodule_count(&self) -> Result<usize, GitError> {
        let Some(work_dir) = self.work_dir.as_deref() else {
            return Ok(0);
        };
        let path = work_dir.join(".gitmodules");
        let bytes = match std::fs::read(&path) {
            Ok(b) => b,
            // Only "no .gitmodules" is swallowed: a repository without
            // submodules does not have the file.
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(0),
            Err(e) => {
                return Err(GitError::repository(
                    self.operation,
                    "reading .gitmodules",
                    &path,
                    e,
                ))
            }
        };
        let file = parse_config_bytes(self.operation, &path, &bytes)?;
        Ok(file
            .sections()
            .filter(|s| s.header().name().eq_ignore_ascii_case(b"submodule"))
            .count())
    }
}

// ═══════════════════════════════════════════════════════════════════════════
// Config reading and the refusals
// ═══════════════════════════════════════════════════════════════════════════

/// Parse config bytes we fetched ourselves. Includes are never followed.
fn parse_config_bytes(
    operation: &'static str,
    path: &Path,
    bytes: &[u8],
) -> Result<gix_config::File, GitError> {
    let meta = gix_config::file::Metadata::from(gix_config::Source::Local).at(path);
    gix_config::File::from_bytes_no_includes(
        bytes,
        meta,
        gix_config::file::init::Options {
            // Values only: we never write this file back, and dropping
            // comments and whitespace is less to parse and less to hold.
            lossy: true,
            ..Default::default()
        },
    )
    .map_err(|e| GitError::repository(operation, "parsing config", path, e))
}

/// Read `<common_dir>/config`.
///
/// A repository with no config file is legal (it just has no settings), so
/// only "file is missing" is swallowed.
fn read_repo_config(
    operation: &'static str,
    common_dir: &Path,
) -> Result<gix_config::File, GitError> {
    let path = common_dir.join("config");
    let bytes = match std::fs::read(&path) {
        Ok(b) => b,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Vec::new(),
        Err(e) => {
            return Err(GitError::repository(
                operation,
                "reading the repository config",
                &path,
                e,
            ))
        }
    };
    parse_config_bytes(operation, &path, &bytes)
}

/// Refuse an `include.path` / `includeIf.*.path` that leaves the repository.
///
/// D.3: the escape is retired by construction — nothing follows an include,
/// because we parse with `from_bytes_no_includes`. What remains is to say so
/// out loud rather than answer from a config whose author expected the
/// include to have been applied.
fn check_include_paths(
    operation: &'static str,
    common_dir: &Path,
    config: &gix_config::File,
) -> Result<(), GitError> {
    for section in config.sections() {
        let name = section.header().name();
        let is_include = name.eq_ignore_ascii_case(b"include");
        let is_include_if = name.eq_ignore_ascii_case(b"includeIf");
        if !is_include && !is_include_if {
            continue;
        }
        let key = if is_include_if {
            "includeIf.path"
        } else {
            "include.path"
        };
        for value in section.values("path") {
            let text = value.to_string();
            let path = Path::new(&text);
            let escapes = path.is_absolute()
                || path.components().any(|c| c == Component::ParentDir)
                // `~/…` is git's home-directory form; expanding it would read
                // host state the kernel never exposes to a tool.
                || text.starts_with('~');
            if escapes {
                return Err(GitError::EscapingInclude {
                    operation,
                    repo: common_dir.to_path_buf(),
                    key: key.to_string(),
                    value: text,
                });
            }
        }
    }
    Ok(())
}

/// Validate `core.repositoryformatversion` and return it.
fn check_format_version(
    operation: &'static str,
    common_dir: &Path,
    config: &gix_config::File,
) -> Result<i64, GitError> {
    let raw = config.string("core.repositoryformatversion");
    let Some(raw) = raw else {
        // Absent means 0, exactly as git reads it.
        return Ok(0);
    };
    let text = raw.to_string();
    let version: i64 = text.trim().parse().map_err(|_| {
        GitError::UnsupportedRepositoryFormat {
            operation,
            repo: common_dir.to_path_buf(),
            version: text.clone(),
        }
    })?;
    if version > 1 {
        return Err(GitError::UnsupportedRepositoryFormat {
            operation,
            repo: common_dir.to_path_buf(),
            version: text,
        });
    }
    Ok(version)
}

/// Refuse any `extensions.*` key this build does not implement.
fn check_extensions(
    operation: &'static str,
    common_dir: &Path,
    config: &gix_config::File,
) -> Result<(), GitError> {
    // Every `[extensions]` section, because a config may repeat the header.
    let Some(sections) = config.sections_by_name("extensions") else {
        return Ok(());
    };
    for section in sections {
        for name in section.value_names() {
            let lowered = name.to_ascii_lowercase();
            if !SUPPORTED_EXTENSIONS.contains(&lowered.as_str()) {
                return Err(GitError::UnknownExtension {
                    operation,
                    repo: common_dir.to_path_buf(),
                    extension: name,
                });
            }
        }
    }
    Ok(())
}

/// Refuse a repository whose objects are not SHA-1.
///
/// The pin set is compiled with the `sha1` feature only ([A.4]), so a SHA-256
/// repository cannot be read — and reading its refs as though they were SHA-1
/// would produce oids that are simply wrong.
fn check_object_format(
    operation: &'static str,
    common_dir: &Path,
    config: &gix_config::File,
) -> Result<(), GitError> {
    let Some(format) = config.string("extensions.objectformat") else {
        return Ok(());
    };
    let format = format.to_string();
    if !format.trim().eq_ignore_ascii_case("sha1") {
        return Err(GitError::UnsupportedRepositoryFormat {
            operation,
            repo: common_dir.to_path_buf(),
            version: format!("extensions.objectFormat={format}"),
        });
    }
    Ok(())
}

/// Refuse a ref backend other than `files`, loudly (E.5).
///
/// Two independent signals, because either alone can be present: the config
/// declaration, and a `reftable/` directory on disk. gix-ref has no reftable
/// support at all and will happily "open" such a repository and report zero
/// refs with a HEAD pointing at `refs/heads/.invalid` — the exact silent
/// wrong answer this refusal exists to prevent.
fn check_ref_backend(
    operation: &'static str,
    common_dir: &Path,
    config: &gix_config::File,
) -> Result<(), GitError> {
    if let Some(declared) = config.string("extensions.refstorage") {
        let declared = declared.to_string();
        if !declared.trim().eq_ignore_ascii_case("files") {
            return Err(GitError::UnsupportedRefBackend {
                operation,
                backend: declared.trim().to_string(),
                repo: common_dir.to_path_buf(),
            });
        }
    }
    if common_dir.join("reftable").is_dir() {
        return Err(GitError::UnsupportedRefBackend {
            operation,
            backend: "reftable".to_string(),
            repo: common_dir.to_path_buf(),
        });
    }
    // The files backend's own markers. Their absence means we are looking at
    // storage we have not identified, and guessing is what E.5 forbids.
    if !common_dir.join("refs").is_dir() {
        return Err(GitError::UnsupportedRefBackend {
            operation,
            backend: "unknown (no 'refs' directory)".to_string(),
            repo: common_dir.to_path_buf(),
        });
    }
    Ok(())
}

// ═══════════════════════════════════════════════════════════════════════════
// Path helpers
// ═══════════════════════════════════════════════════════════════════════════

/// Resolve `<git_dir>/commondir`, which a linked worktree carries.
fn resolve_common_dir(operation: &'static str, git_dir: &Path) -> Result<PathBuf, GitError> {
    let path = git_dir.join("commondir");
    let text = match std::fs::read_to_string(&path) {
        Ok(t) => t,
        // Only "no commondir file" is swallowed: that is the ordinary main
        // repository, where the git dir *is* the common dir.
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(git_dir.to_path_buf()),
        Err(e) => {
            return Err(GitError::repository(
                operation,
                "reading the worktree's commondir",
                &path,
                e,
            ))
        }
    };
    let text = text.trim();
    let common = Path::new(text);
    Ok(if common.is_absolute() {
        normalize(common)
    } else {
        normalize(&git_dir.join(common))
    })
}

/// Resolve a path the way the operating system will, for ceiling comparison.
///
/// [`normalize`] is lexical and the kernel is not, and the gap between them is
/// an escape: `<git_dir>/link/..` is `<git_dir>` to a string walk and the
/// parent of `link`'s *target* to `openat`. A repository controls every byte
/// under its own `.git`, symlinks included, so a lexical ceiling check can be
/// walked straight through — confirmed by fixture, not reasoned about.
///
/// kaish's own `LocalFs` canonicalizes before its containment check and
/// returns `PermissionDenied` when the real path leaves the mount root
/// (`kaish-vfs`, `local.rs`). Checking anything weaker here would make this
/// crate a bypass of a guarantee the platform already enforces and tests —
/// a worse failure than never having offered the guarantee.
///
/// Both sides of every comparison go through this, so a mount that is itself
/// reached through a symlink compares equal instead of refusing everything.
fn real_dir(operation: &'static str, what: &'static str, path: &Path) -> Result<PathBuf, GitError> {
    std::fs::canonicalize(path)
        .map_err(|e| GitError::repository(operation, format!("resolving the {what}"), path, e))
}

/// Lexically normalize a path: drop `.`, resolve `..` against the preceding
/// component. Never touches the filesystem — the same discipline the kernel's
/// own `resolve_path` follows.
///
/// Cheap and correct for *building* a path. Never sufficient for deciding one
/// is inside the ceiling: [`real_dir`] is what that requires.
fn normalize(path: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    for component in path.components() {
        match component {
            Component::CurDir => {}
            Component::ParentDir => {
                if !out.pop() {
                    out.push("..");
                }
            }
            other => out.push(other),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalize_resolves_dot_and_dotdot() {
        assert_eq!(normalize(Path::new("/a/b/../c/./d")), PathBuf::from("/a/c/d"));
        assert_eq!(
            normalize(Path::new("/a/.git/worktrees/x/../..")),
            PathBuf::from("/a/.git")
        );
    }

    /// Every supported extension name must already be lowercase, or the
    /// case-insensitive lookup in `check_extensions` would never match it and
    /// the allowlist would silently be empty.
    #[test]
    fn supported_extension_names_are_lowercase() {
        for name in SUPPORTED_EXTENSIONS {
            assert_eq!(
                *name,
                name.to_ascii_lowercase(),
                "{name} must be lowercase to match the case-folded lookup"
            );
        }
    }

    fn config(text: &str) -> gix_config::File {
        parse_config_bytes("test", Path::new("/repo/.git/config"), text.as_bytes())
            .expect("fixture config parses")
    }

    #[test]
    fn absolute_include_path_is_refused() {
        let cfg = config("[include]\n\tpath = /etc/passwd\n");
        let err = check_include_paths("info", Path::new("/repo/.git"), &cfg)
            .expect_err("an absolute include must be refused");
        assert_eq!(err.exit_code(), 4);
        assert!(err.to_string().contains("/etc/passwd"), "{err}");
    }

    #[test]
    fn parent_relative_include_path_is_refused() {
        let cfg = config("[include]\n\tpath = ../../../etc/gitconfig\n");
        let err = check_include_paths("info", Path::new("/repo/.git"), &cfg)
            .expect_err("a `..`-bearing include must be refused");
        assert_eq!(err.exit_code(), 4);
    }

    #[test]
    fn home_relative_include_path_is_refused() {
        let cfg = config("[include]\n\tpath = ~/.gitconfig\n");
        let err = check_include_paths("info", Path::new("/repo/.git"), &cfg)
            .expect_err("a `~`-rooted include must be refused");
        assert_eq!(err.exit_code(), 4);
    }

    #[test]
    fn include_if_path_is_checked_too() {
        let cfg = config("[includeIf \"gitdir:/srv/\"]\n\tpath = /etc/evil\n");
        let err = check_include_paths("info", Path::new("/repo/.git"), &cfg)
            .expect_err("includeIf must be checked like include");
        assert!(err.to_string().contains("includeIf.path"), "{err}");
    }

    #[test]
    fn repo_relative_include_path_is_allowed() {
        let cfg = config("[include]\n\tpath = extra-config\n");
        check_include_paths("info", Path::new("/repo/.git"), &cfg)
            .expect("a repo-relative include is not an escape");
    }

    #[test]
    fn unknown_extension_is_refused() {
        let cfg = config("[extensions]\n\tworktreeConfig = true\n");
        let err = check_extensions("info", Path::new("/repo/.git"), &cfg)
            .expect_err("an unimplemented extension must be refused");
        assert_eq!(err.exit_code(), 4);
        assert!(err.to_string().contains("worktreeConfig"), "{err}");
    }

    /// Case folding, because git compares config keys case-insensitively and
    /// `objectFormat` is how git itself writes the key.
    #[test]
    fn supported_extension_is_accepted_regardless_of_case() {
        let cfg = config("[extensions]\n\tobjectFormat = sha1\n");
        check_extensions("info", Path::new("/repo/.git"), &cfg)
            .expect("objectFormat is supported whatever its spelling");
    }

    #[test]
    fn sha256_object_format_is_refused() {
        let cfg = config("[extensions]\n\tobjectFormat = sha256\n");
        let err = check_object_format("info", Path::new("/repo/.git"), &cfg)
            .expect_err("this build links sha1 only");
        assert_eq!(err.exit_code(), 4);
    }

    /// E.5's headline refusal, verbatim in spirit: it names the backend, the
    /// repository, and the fact that no fallback happened.
    #[test]
    fn reftable_declaration_is_refused_loudly() {
        let cfg = config("[extensions]\n\trefStorage = reftable\n");
        let err = check_ref_backend("info", Path::new("/repo/.git"), &cfg)
            .expect_err("reftable must be refused");
        assert_eq!(err.exit_code(), 4);
        let msg = err.to_string();
        assert!(msg.contains("reftable"), "must name the backend: {msg}");
        assert!(msg.contains("/repo/.git"), "must name the repo: {msg}");
        assert!(msg.contains("files"), "must name what we do read: {msg}");
        assert!(
            msg.contains("not a fallback"),
            "must say no fallback happened: {msg}"
        );
    }

    #[test]
    fn format_version_beyond_one_is_refused() {
        let cfg = config("[core]\n\trepositoryformatversion = 2\n");
        let err = check_format_version("info", Path::new("/repo/.git"), &cfg)
            .expect_err("format 2 is not something we can read");
        assert_eq!(err.exit_code(), 4);
    }

    #[test]
    fn format_version_zero_and_one_are_accepted() {
        assert_eq!(
            check_format_version("info", Path::new("/r"), &config("")).expect("absent means 0"),
            0
        );
        assert_eq!(
            check_format_version(
                "info",
                Path::new("/r"),
                &config("[core]\n\trepositoryformatversion = 1\n")
            )
            .expect("format 1 is readable"),
            1
        );
    }
}
