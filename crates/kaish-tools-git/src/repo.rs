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

use gix_index::hash as gix_hash;
use gix_object::FindExt;
use gix_odb::HeaderExt;
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
    /// The canonical mount root that discovery was ceilinged at. Kept so the
    /// read accessors can hold the same "everything I open is inside the
    /// ceiling" invariant `discover` establishes — a `shallow` marker or a
    /// `.gitmodules` that is a symlink out of the mount is refused there too,
    /// not only during open.
    ceiling: PathBuf,
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

        // Screen a `.git` *file*'s `gitdir:` line BEFORE discovery reads it.
        //
        // This has to happen first, and that ordering is the whole fix.
        // gix-discover validates the target itself: a `gitdir:` naming a real
        // git directory outside the mount succeeds (and our ceiling check then
        // refuses it, exit 4), while one naming a path that does not exist
        // fails discovery outright (exit 1). The difference between those two
        // exits is a one-bit read of any path on the host — point `gitdir:` at
        // it and look at the code. No check placed *after* `upwards_opts` can
        // close that, because the leak already happened inside it.
        //
        // Screening here refuses an escaping `gitdir:` on the strength of
        // where it points, never on whether it is there.
        screen_gitdir_file(operation, real_path, &real_dir(operation, "mount root", ceiling)?)?;

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
        // ceiling to a string comparison and outside it to `openat`. And it is
        // not only the directories below that are named by repository content
        // — every file this type opens is reached by joining a fixed name onto
        // a checked directory, and that leaf can be a symlink too. So the
        // ceiling and every directory are resolved with `canonicalize` before
        // they are compared (here), and every file and probe underneath goes
        // through `open_leaf`, which refuses a symlinked leaf that escapes.
        let ceiling = real_dir(operation, "mount root", ceiling)?;

        // The working tree first, because whether it is inside the mount is
        // what decides how a bad `git_dir` may be reported. It is discovery's
        // own output — a directory the caller named or walked up from — so
        // naming it leaks nothing.
        let work_dir = match work_dir {
            Some(w) => Some(std::fs::canonicalize(&w).map_err(|_| GitError::NotARepository {
                operation,
                start: real_path.to_path_buf(),
                ceiling: ceiling.clone(),
            })?),
            None => None,
        };
        let contained_work_dir = work_dir
            .as_ref()
            .filter(|w| w.starts_with(&ceiling))
            .cloned();

        // `git_dir` is different: a `.git` *file* inside the sandbox can name
        // any path on the host through its `gitdir:` line, which makes this
        // value **repository-controlled**. So the two ways it can fail us —
        // it does not resolve at all, or it resolves outside the mount — must
        // be *indistinguishable*, exactly as `contain` and `open_leaf` already
        // make them for `commondir`, alternates and symlinked leaves.
        //
        // They were not. `canonicalize` failing produced `NotARepository`
        // (exit 1) while resolving-outside produced `EscapesMount` (exit 4),
        // so a hostile repository could read one bit about any host path per
        // probe: point `gitdir:` at it and read the exit code. That is the
        // same oracle shape the earlier round retired everywhere else, and it
        // survived here because this branch was written before that rule
        // existed.
        //
        // Now the refusal depends only on what the attacker already knows —
        // whether the working tree it planted is inside the mount — and never
        // on the host.
        let refuse_git_dir = |ceiling: PathBuf| -> GitError {
            match contained_work_dir.clone() {
                // A working tree inside the mount named a git dir we will not
                // read. The caller already knows this path, and an embedder
                // who mounted only a linked worktree needs to be told how to
                // fix their layout, so naming it costs nothing.
                Some(work_dir) => GitError::EscapesMount {
                    operation,
                    what: "git directory (the `gitdir:` line in its .git file)",
                    repo: work_dir,
                    ceiling,
                },
                // Discovery itself walked out of the mount. From inside the
                // mount there simply is no repository, and that is all we say:
                // confirming one exists above would answer a question the
                // caller was never entitled to ask.
                None => GitError::NotARepository {
                    operation,
                    start: real_path.to_path_buf(),
                    ceiling,
                },
            }
        };

        let git_dir = match std::fs::canonicalize(&git_dir) {
            Ok(dir) if dir.starts_with(&ceiling) => dir,
            // Resolves outside the mount, or does not resolve at all. One
            // answer for both — that indistinguishability IS the fix.
            Ok(_) | Err(_) => return Err(refuse_git_dir(ceiling)),
        };

        if let Some(work_dir) = work_dir.as_deref() {
            // Defense in depth. Today `work_dir` is discovery's own physical
            // parent and is bounded by the `git_dir` check above — see
            // `work_dir_is_bounded_by_discovery` in tests/hostile_repo.rs for
            // the reasoning and for what would invalidate it. The check costs
            // one `starts_with` and it is what stands between a future
            // `core.worktree` reader and `submodule_count` reading
            // `<work_dir>/.gitmodules` off an arbitrary host path.
            if !work_dir.starts_with(&ceiling) {
                return Err(escapes(operation, "working tree", &git_dir, &ceiling));
            }
        }

        // `commondir` in two steps, because both the file and its content are
        // repository-controlled. First the *file*: `open_leaf` refuses it if it
        // is a symlink pointing out of the mount, so a `commondir` symlinked to
        // `/etc/shadow` is never read. Then its *content*, which is itself a
        // path: `contain` canonicalizes and ceiling-checks it, refusing an
        // absolute, `..`-bearing, or symlink-traversing target — the linked
        // worktree whose main repository lives outside the mount, or a
        // repository pointing us somewhere it was never given.
        let common_dir = match open_leaf(
            operation,
            "common directory (.git/commondir)",
            &git_dir,
            "commondir",
            &ceiling,
        )? {
            Leaf::Absent => git_dir.clone(),
            Leaf::At(commondir_file) => {
                let text = std::fs::read_to_string(&commondir_file)
                    .map_err(|e| {
                        GitError::repository(operation, "reading the commondir file", &git_dir, e)
                    })?
                    .trim()
                    .to_string();
                let candidate = if Path::new(&text).is_absolute() {
                    normalize(Path::new(&text))
                } else {
                    normalize(&git_dir.join(&text))
                };
                contain(
                    operation,
                    "common directory (.git/commondir)",
                    &git_dir,
                    &candidate,
                    &ceiling,
                )?
            }
        };

        // Read repo-local config before opening anything. A repository we are
        // going to refuse must be refused before we touch its objects. The
        // config file itself goes through `open_leaf`, since it too is a leaf
        // a repository could symlink out of the mount.
        let config_leaf = open_leaf(operation, "repository config", &common_dir, "config", &ceiling)?;
        let config = read_repo_config(operation, config_leaf.path())?;
        check_include_paths(operation, &common_dir, &config)?;
        let format_version = check_format_version(operation, &common_dir, &config)?;
        if format_version >= 1 {
            check_extensions(operation, &common_dir, &config)?;
            check_object_format(operation, &common_dir, &config)?;
        }
        // The ref backend check does not wait for format version 1: a
        // `reftable/` directory on disk is evidence regardless of what the
        // config claims, and answering from an empty `refs/` would be the
        // silent fallback E.5 forbids. Both probes go through `open_leaf`.
        let has_reftable = open_leaf(operation, "reftable directory", &common_dir, "reftable", &ceiling)?
            .path()
            .is_some_and(Path::is_dir);
        let has_refs = open_leaf(operation, "refs directory", &common_dir, "refs", &ceiling)?
            .path()
            .is_some_and(Path::is_dir);
        check_ref_backend(operation, &common_dir, &config, has_reftable, has_refs)?;

        // The object store, and its alternates. `open_leaf` catches an
        // `objects` directory symlinked out of the mount (a legitimate
        // shared-store symlink that stays inside is followed and its resolved
        // path used); `guard_alternates` catches `objects/info/alternates`
        // naming an outside store, which needs no symlink at all.
        let objects_dir = match open_leaf(operation, "object store", &common_dir, "objects", &ceiling)? {
            Leaf::At(dir) => dir,
            // A repository with no object directory is malformed; let gix say
            // so against a path the caller owns.
            Leaf::Absent => common_dir.join("objects"),
        };
        guard_alternates(operation, &objects_dir, &ceiling)?;
        let objects = gix_odb::at(&objects_dir).map_err(|e| {
            GitError::repository(operation, "opening the object store", &common_dir, e)
        })?;

        // The containment boundary, stated where it actually ends. Everything
        // above ceiling-checks a path *this crate* opens. From here, gix opens
        // objects, packs, `HEAD` and individual ref files itself, by name, and
        // a symlink among those leaves (`objects/ab/cd…` linking out of the
        // mount) is not interceptable without wrapping every gix open. The
        // honest close is platform-level — `openat2(RESOLVE_BENEATH)` or a
        // kaish VFS seam — and it is a threat-model decision, not a code change
        // this PR makes. For the read verbs the residual is a read that lands
        // outside and almost always fails to parse as a git object, not a
        // general file-exfiltration primitive. Tracked as design input.
        //
        // `git status` widens this residual by exactly one shape, named so it
        // is not silent: `gix-worktree` reads per-directory `.gitignore` files
        // itself as it descends the working tree (`walk_untracked_and_ignored`
        // in verbs/status.rs). Those reads are bounded by the working tree,
        // which is ceiling-checked above; a `.gitignore` symlinked out of the
        // mount would be followed, the same interceptability gap as gix opening
        // an object by name — and worth naming loudly because it is the one
        // carve-out path that reaches into the working tree rather than `.git`,
        // consulted on every descent whether or not `--untracked` reports what
        // it matched. What status opens *itself* is not in the carve-out: the
        // index and `info/exclude` go through `open_leaf`, and the working-tree
        // paths its index entries name are contained parent-chain-first by
        // `WorktreePaths` in verbs/status.rs.

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
            ceiling,
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
    ///
    /// Fallible because the `shallow` marker is a leaf under the common dir
    /// like any other, and a repository that symlinks it out of the mount is
    /// refused rather than followed — the same invariant `discover` holds for
    /// every path it opens.
    pub fn is_shallow(&self) -> Result<bool, GitError> {
        Ok(open_leaf(
            self.operation,
            "shallow marker",
            &self.common_dir,
            "shallow",
            &self.ceiling,
        )?
        .path()
        .is_some())
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

        // `open_leaf` refuses a `worktrees` symlinked out of the mount and
        // hands back its real path otherwise. Absent means the repository
        // never had a linked worktree.
        let Some(dir) = open_leaf(
            self.operation,
            "worktrees directory",
            &self.common_dir,
            "worktrees",
            &self.ceiling,
        )?
        .path()
        .map(Path::to_path_buf) else {
            return Ok(count);
        };

        for entry in std::fs::read_dir(&dir)
            .map_err(|e| GitError::repository(self.operation, "listing linked worktrees", &dir, e))?
        {
            let entry = entry.map_err(|e| {
                GitError::repository(self.operation, "listing linked worktrees", &dir, e)
            })?;
            // A registered worktree is a real directory carrying a `gitdir`
            // file. A symlinked entry is not something git writes here, and
            // following one would step outside the mount to count it, so it is
            // skipped rather than followed — `symlink_metadata` does not
            // follow it.
            let entry_path = entry.path();
            let is_symlink = std::fs::symlink_metadata(&entry_path)
                .map(|m| m.file_type().is_symlink())
                .unwrap_or(true);
            if !is_symlink && entry_path.join("gitdir").is_file() {
                count += 1;
            }
        }
        Ok(count)
    }

    // ── Accessors the status composition reads through (B.2) ────────────────
    //
    // These lend `git status` the raw materials it hand-composes from — the
    // index, the HEAD tree id, `info/exclude`, and the object store — with the
    // ceiling invariant already enforced on every fixed-name leaf they open.
    // `objects()` lends the odb handle to a sibling module, not to a caller
    // outside the crate; it is read-only in practice, and the
    // `write_shaped_identifiers` scan (D.1) is what keeps it that way.

    /// The object store, for reading trees and blobs.
    pub(crate) fn objects(&self) -> &gix_odb::Handle {
        &self.objects
    }

    /// The common directory (`objects/`, `refs/`, `info/` live here).
    pub(crate) fn common_dir(&self) -> &Path {
        &self.common_dir
    }

    /// The working tree root, or `None` for a bare repository.
    ///
    /// Canonical and inside [`ReadRepo::ceiling`], established by `discover`.
    /// A verb that joins repository-controlled names onto it may treat it as
    /// its own ceiling — see `verbs::status`'s index-path containment.
    pub(crate) fn work_dir(&self) -> Option<&Path> {
        self.work_dir.as_deref()
    }

    /// The canonical mount root every path this repository reaches must stay
    /// inside. Named in refusals; never a path the caller did not already
    /// know.
    pub(crate) fn ceiling(&self) -> &Path {
        &self.ceiling
    }

    /// The tree HEAD's commit points at, or `None` on an unborn branch.
    ///
    /// The staged half of `git status` diffs this tree against the index; an
    /// unborn HEAD (`git init` then nothing) has no commit and therefore no
    /// tree, which is a first-class state, not an error — every staged file is
    /// then an addition against the empty tree.
    pub(crate) fn head_tree_id(&self) -> Result<Option<gix_index::hash::ObjectId>, GitError> {
        let head = self.refs.find("HEAD").map_err(|e| {
            GitError::repository(self.operation, "reading HEAD", &self.git_dir, e)
        })?;

        if let gix_ref::Target::Symbolic(name) = &head.target {
            let exists = self
                .refs
                .try_find(name.as_ref())
                .map_err(|e| {
                    GitError::repository(self.operation, "reading HEAD's branch", &self.common_dir, e)
                })?
                .is_some();
            if !exists {
                return Ok(None);
            }
        }

        let mut head = head;
        let commit_id = head
            .peel_to_id(&self.refs, &self.objects)
            .map_err(|e| GitError::repository(self.operation, "resolving HEAD", &self.git_dir, e))?;

        let mut buf = Vec::new();
        let data = self.objects.find(&commit_id, &mut buf).map_err(|e| {
            GitError::repository(self.operation, "reading the HEAD commit", &self.git_dir, e)
        })?;
        let tree_id = match data.decode().map_err(|e| {
            GitError::repository(self.operation, "decoding the HEAD commit", &self.git_dir, e)
        })? {
            gix_object::ObjectRef::Commit(commit) => commit.tree(),
            // HEAD peeled to something that is not a commit — a tag of a tree,
            // say. Nothing this build reads produces that, and treating it as a
            // commit would be a wrong answer.
            other => {
                return Err(GitError::repository(
                    self.operation,
                    "resolving HEAD to a commit",
                    &self.git_dir,
                    std::io::Error::other(format!("HEAD is a {}, not a commit", other.kind())),
                ))
            }
        };
        Ok(Some(tree_id))
    }

    /// Resolve a `--rev` argument to a commit oid, over the small revision
    /// grammar (architecture.md B): `HEAD`, a branch, a tag, `refs/...`, a full
    /// or ≥4-char unambiguous oid, and the `~N` / `^` / `^N` suffixes on any of
    /// those. Everything else — `@{...}`, `A..B`, `:/text`, `^{...}`,
    /// `<rev>:<path>` — is a loud usage error naming the form, never resolved
    /// to a surprising commit.
    ///
    /// A ref is tried before an oid, matching git's preference, so a branch
    /// named like a hex string still resolves as the branch. The result is
    /// always peeled to a commit: a tag resolves to what it ultimately points
    /// at, and a revision that names a tree or blob is refused.
    pub(crate) fn resolve_commit(&self, spec: &str) -> Result<gix_hash::ObjectId, GitError> {
        let (base, nav) = split_base_and_nav(self.operation, spec)?;
        let oid = self.resolve_base(base, spec)?;
        let oid = self.peel_to_commit(oid, spec)?;
        self.apply_nav_steps(oid, spec, nav)
    }

    /// Resolve a revision the way `show`/`ls` need it: over the same small
    /// grammar as [`ReadRepo::resolve_commit`], but the object handed back is
    /// whatever the revision plainly names, not forced down to a commit.
    ///
    /// A `~N`/`^N` suffix still walks commit ancestry, so when one is present
    /// the base is peeled to a commit first, exactly as `resolve_commit` does,
    /// and the result is a commit. A bare revision with no suffix — `v0.1.0`,
    /// an oid — is returned exactly as resolved, tag included: collapsing an
    /// annotated tag here would make `show <tag>` silently describe the tag's
    /// target instead of the tag itself (architecture.md B.5, D5).
    ///
    /// The caller splits off any `<rev>:<path>` suffix first
    /// ([`split_revision_and_path`]) — `spec` here is the revision half alone
    /// and, by construction of that split, never contains a colon.
    pub(crate) fn resolve_object(&self, spec: &str) -> Result<gix_hash::ObjectId, GitError> {
        let (base, nav) = split_base_and_nav(self.operation, spec)?;
        let oid = self.resolve_base_shallow(base, spec)?;
        if nav.is_empty() {
            return Ok(oid);
        }
        let oid = self.peel_to_commit(oid, spec)?;
        self.apply_nav_steps(oid, spec, nav)
    }

    /// Apply a parsed `~N`/`^N` navigation suffix to an already-resolved
    /// commit. Shared by [`ReadRepo::resolve_commit`] and
    /// [`ReadRepo::resolve_object`] so the two grammars cannot drift apart.
    fn apply_nav_steps(
        &self,
        mut oid: gix_hash::ObjectId,
        spec: &str,
        nav: &str,
    ) -> Result<gix_hash::ObjectId, GitError> {
        for step in parse_nav(self.operation, spec, nav)? {
            oid = match step {
                Nav::FirstParentAncestor(n) => {
                    let mut cur = oid;
                    for _ in 0..n {
                        cur = self.nth_parent(cur, 1, spec)?;
                    }
                    cur
                }
                // `^0` dereferences to the commit itself; `^N` (N≥1) takes the
                // Nth parent.
                Nav::NthParent(0) => oid,
                Nav::NthParent(n) => self.nth_parent(oid, n, spec)?,
            };
        }
        Ok(oid)
    }

    /// Resolve the base of a revision — a ref if one matches, otherwise an
    /// oid — fully peeling a ref through any annotated tags to its first
    /// non-tag object, matching git's own `peel_to_id` semantics. Used by
    /// `resolve_commit` (`log`), which only ever wants a commit.
    fn resolve_base(&self, base: &str, spec: &str) -> Result<gix_hash::ObjectId, GitError> {
        let op = self.operation;
        // D3 (kaish-extras issue backlog, L4): `@` is git's alias for `HEAD`
        // wherever a revision is expected. Safe as a hard-coded substitution —
        // `git check-ref-format` refuses to create a ref literally named `@`,
        // so there is no real ref this could ever shadow.
        let base = if base == "@" { "HEAD" } else { base };
        // A ref first, matching git, so a branch named like a hex string still
        // resolves as the branch. The name is converted here rather than inside
        // `try_find` to keep two failure modes apart: a string that is not a
        // valid ref name simply is not a ref, and falls through to the oid
        // attempt; a refs backend that actually failed to read is loud. Letting
        // one `if let Ok(..)` swallow both would turn an unreadable `.git/refs`
        // into a confident "no such revision" (house rule: no silent
        // fallbacks).
        if let Ok(partial) = TryInto::<&gix_ref::PartialNameRef>::try_into(base) {
            match self.refs.try_find(partial) {
                Ok(Some(mut reference)) => {
                    let id = reference.peel_to_id(&self.refs, &self.objects).map_err(|e| {
                        GitError::repository(op, format!("resolving ref '{base}'"), &self.git_dir, e)
                    })?;
                    return Ok(id);
                }
                // A valid ref name that names nothing — fall through and try
                // the same string as an oid.
                Ok(None) => {}
                Err(e) => {
                    return Err(GitError::repository(
                        op,
                        format!("looking up ref '{base}'"),
                        &self.git_dir,
                        e,
                    ))
                }
            }
        }
        self.resolve_oid_prefix(base, spec)
    }

    /// Resolve the base of a revision the way [`ReadRepo::resolve_object`]
    /// needs it: a ref is followed through *symbolic* indirection only (`HEAD`
    /// to the branch it names), never peeled past an annotated tag object, so
    /// `show <tag>` sees the tag itself rather than what it points at. An oid
    /// path is identical to [`ReadRepo::resolve_base`]'s — a hash names one
    /// object directly, so there is nothing to peel either way.
    fn resolve_base_shallow(&self, base: &str, spec: &str) -> Result<gix_hash::ObjectId, GitError> {
        let op = self.operation;
        // D3: same `@` → `HEAD` alias as `resolve_base`; see its comment.
        let base = if base == "@" { "HEAD" } else { base };
        if let Ok(partial) = TryInto::<&gix_ref::PartialNameRef>::try_into(base) {
            match self.refs.try_find(partial) {
                Ok(Some(reference)) => return self.follow_symbolic_only(reference, spec),
                Ok(None) => {}
                Err(e) => {
                    return Err(GitError::repository(
                        op,
                        format!("looking up ref '{base}'"),
                        &self.git_dir,
                        e,
                    ))
                }
            }
        }
        self.resolve_oid_prefix(base, spec)
    }

    /// Follow a reference's symbolic indirection (`HEAD` → `refs/heads/main`)
    /// to the object it directly names, without peeling an annotated tag
    /// object down to its target.
    ///
    /// Bounded at 5 hops, the same depth gitoxide's own symbolic chase caps
    /// itself at (`gix_ref::store::file::raw_ext`'s `MAX_REF_DEPTH`) — a cycle
    /// here is a malformed repository, not something to spin on.
    fn follow_symbolic_only(
        &self,
        mut reference: gix_ref::Reference,
        spec: &str,
    ) -> Result<gix_hash::ObjectId, GitError> {
        let op = self.operation;
        for _ in 0..5 {
            match reference.target {
                gix_ref::Target::Object(oid) => return Ok(oid),
                gix_ref::Target::Symbolic(name) => {
                    reference = self
                        .refs
                        .try_find(name.as_ref())
                        .map_err(|e| {
                            GitError::repository(
                                op,
                                format!("looking up ref '{}'", name.as_bstr()),
                                &self.git_dir,
                                e,
                            )
                        })?
                        .ok_or_else(|| GitError::NoSuchRevision {
                            operation: op,
                            rev: spec.to_string(),
                            repo: self.git_dir.clone(),
                        })?;
                }
            }
        }
        Err(GitError::repository(
            op,
            "following a symbolic ref chain",
            &self.git_dir,
            std::io::Error::other("too many levels of symbolic indirection (over 5)"),
        ))
    }

    /// Resolve a full or ≥4-char oid prefix. `lookup_prefix` handles both: a
    /// 40-char prefix is an exact match, a shorter one may be ambiguous.
    fn resolve_oid_prefix(&self, base: &str, spec: &str) -> Result<gix_hash::ObjectId, GitError> {
        let op = self.operation;
        let looks_hex = (4..=40).contains(&base.len()) && base.bytes().all(|b| b.is_ascii_hexdigit());
        if looks_hex {
            let prefix = gix_hash::Prefix::from_hex(base).map_err(|_| GitError::NoSuchRevision {
                operation: op,
                rev: spec.to_string(),
                repo: self.git_dir.clone(),
            })?;
            return match self.objects.lookup_prefix(prefix, None).map_err(|e| {
                GitError::repository(op, "looking up an oid prefix", &self.git_dir, e)
            })? {
                Some(Ok(oid)) => Ok(oid),
                Some(Err(())) => Err(GitError::AmbiguousRevision {
                    operation: op,
                    rev: spec.to_string(),
                    repo: self.git_dir.clone(),
                }),
                None => Err(GitError::NoSuchRevision {
                    operation: op,
                    rev: spec.to_string(),
                    repo: self.git_dir.clone(),
                }),
            };
        }

        Err(GitError::NoSuchRevision {
            operation: op,
            rev: spec.to_string(),
            repo: self.git_dir.clone(),
        })
    }

    /// Resolve `oid` to the tree it names, for `<rev>:<path>` navigation
    /// (`show`, `ls`): a commit's own tree, an annotated tag peeled
    /// (recursively) to whatever it ultimately points at, or the oid itself
    /// when it already names a tree. A blob has no tree, and is refused
    /// rather than treated as an empty one.
    pub(crate) fn tree_of_object(
        &self,
        mut oid: gix_hash::ObjectId,
        spec: &str,
    ) -> Result<gix_hash::ObjectId, GitError> {
        let op = self.operation;
        let mut buf = Vec::new();
        // A tag chain is finite in any real repository; the bound stops a
        // hand-built cycle from spinning rather than trusting it cannot
        // happen (mirrors `peel_to_commit`'s own bound).
        for _ in 0..32 {
            let kind = self
                .objects
                .header(oid)
                .map_err(|e| GitError::repository(op, "reading an object header", &self.git_dir, e))?
                .kind();
            match kind {
                gix_object::Kind::Tree => return Ok(oid),
                gix_object::Kind::Commit => {
                    return Ok(self
                        .objects
                        .find_commit(&oid, &mut buf)
                        .map_err(|e| GitError::repository(op, "reading a commit", &self.git_dir, e))?
                        .tree());
                }
                gix_object::Kind::Tag => {
                    oid = self
                        .objects
                        .find_tag(&oid, &mut buf)
                        .map_err(|e| GitError::repository(op, "reading a tag", &self.git_dir, e))?
                        .target();
                }
                gix_object::Kind::Blob => {
                    return Err(GitError::NotATree {
                        operation: op,
                        spec: spec.to_string(),
                        repo: self.git_dir.clone(),
                    })
                }
            }
        }
        Err(GitError::NoSuchRevision {
            operation: op,
            rev: spec.to_string(),
            repo: self.git_dir.clone(),
        })
    }

    /// Peel an object to the commit it names, following annotated tags. A
    /// revision that resolves to a tree or blob is refused, not silently
    /// treated as a commit.
    fn peel_to_commit(&self, start: gix_hash::ObjectId, spec: &str) -> Result<gix_hash::ObjectId, GitError> {
        let op = self.operation;
        let mut oid = start;
        let mut buf = Vec::new();
        // A tag chain is finite in any real repository; the bound stops a
        // hand-built cycle from spinning rather than trusting it cannot happen.
        for _ in 0..32 {
            // The decoded object borrows `buf`, so the step is reduced to an
            // owned decision inside this scope — the borrow ends before `oid`
            // is reassigned for the next round.
            let next: Option<gix_hash::ObjectId> = {
                let data = self.objects.find(&oid, &mut buf).map_err(|e| {
                    GitError::repository(op, "reading an object", &self.git_dir, e)
                })?;
                match data.decode().map_err(|e| {
                    GitError::repository(op, "decoding an object", &self.git_dir, e)
                })? {
                    gix_object::ObjectRef::Commit(_) => None,
                    gix_object::ObjectRef::Tag(tag) => Some(tag.target()),
                    _ => {
                        return Err(GitError::NoSuchRevision {
                            operation: op,
                            rev: spec.to_string(),
                            repo: self.git_dir.clone(),
                        })
                    }
                }
            };
            match next {
                None => return Ok(oid),
                Some(target) => oid = target,
            }
        }
        Err(GitError::NoSuchRevision {
            operation: op,
            rev: spec.to_string(),
            repo: self.git_dir.clone(),
        })
    }

    /// The Nth parent (1-based) of a commit, for `^N` and `~N` navigation.
    fn nth_parent(&self, oid: gix_hash::ObjectId, n: usize, spec: &str) -> Result<gix_hash::ObjectId, GitError> {
        let op = self.operation;
        let mut buf = Vec::new();
        let parents: Vec<gix_hash::ObjectId> = self
            .objects
            .find_commit_iter(&oid, &mut buf)
            .map_err(|e| GitError::repository(op, "reading a commit", &self.git_dir, e))?
            .parent_ids()
            .collect();
        parents.get(n - 1).copied().ok_or_else(|| GitError::NoSuchRevision {
            operation: op,
            rev: spec.to_string(),
            repo: self.git_dir.clone(),
        })
    }

    /// The parsed index, or `None` when the repository has never had one.
    ///
    /// Read as bytes through [`open_leaf`], so an `index` symlinked out of the
    /// mount is refused, not followed — the containment invariant `discover`
    /// holds for every leaf, extended to the index. The index is per-worktree,
    /// so it lives at `<git_dir>/index` for a linked worktree exactly as for
    /// the main one. Decoding never writes: a refreshed stat-cache is not
    /// persisted, which is what keeps the fingerprint test (D.4) green over
    /// status.
    pub(crate) fn open_index(&self) -> Result<Option<gix_index::State>, GitError> {
        let Some(path) = open_leaf(self.operation, "index (.git/index)", &self.git_dir, "index", &self.ceiling)?
            .path()
            .map(Path::to_path_buf)
        else {
            return Ok(None);
        };
        let bytes = std::fs::read(&path)
            .map_err(|e| GitError::repository(self.operation, "reading the index", &path, e))?;
        let (state, _checksum) = gix_index::State::from_bytes(
            &bytes,
            // Zero, deliberately: we never write the index back and detect
            // modification by hashing, not by the racy-clean stat cache, so the
            // pass-through timestamp is unused.
            filetime::FileTime::zero(),
            gix_index::hash::Kind::Sha1,
            gix_index::decode::Options::default(),
        )
        .map_err(|e| GitError::repository(self.operation, "decoding the index", &path, e))?;
        Ok(Some(state))
    }

    /// The bytes of `info/exclude`, or empty when it is absent.
    ///
    /// Both `info/` and the `exclude` leaf under it go through [`open_leaf`], so
    /// an `exclude` symlinked out of the mount is refused rather than read. The
    /// per-directory `.gitignore` files are read by `gix-worktree` internally
    /// as it descends — that is the documented residual carve-out (see the
    /// containment note in `discover`), the same shape as gix opening objects
    /// by name, and it stays inside the ceiling because the working tree does.
    pub(crate) fn read_info_exclude(&self) -> Result<Vec<u8>, GitError> {
        let Some(info) = open_leaf(self.operation, "info directory", &self.common_dir, "info", &self.ceiling)?
            .path()
            .map(Path::to_path_buf)
        else {
            return Ok(Vec::new());
        };
        let Some(exclude) = open_leaf(self.operation, "info/exclude", &info, "exclude", &self.ceiling)?
            .path()
            .map(Path::to_path_buf)
        else {
            return Ok(Vec::new());
        };
        std::fs::read(&exclude)
            .map_err(|e| GitError::repository(self.operation, "reading info/exclude", &exclude, e))
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
        // `.gitmodules` is a leaf under the working tree, which a repository
        // could symlink out of the mount — `open_leaf` refuses that and
        // returns the safe path otherwise.
        let Some(path) = open_leaf(
            self.operation,
            ".gitmodules file",
            work_dir,
            ".gitmodules",
            &self.ceiling,
        )?
        .path()
        .map(Path::to_path_buf) else {
            return Ok(0);
        };
        let bytes = std::fs::read(&path)
            .map_err(|e| GitError::repository(self.operation, "reading .gitmodules", &path, e))?;
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

/// Parse the repository config at an already-ceiling-checked path.
///
/// `config_path` is `None` when the file is absent, which is legal — a
/// repository with no config just has no settings. The path itself was
/// resolved by [`open_leaf`], so a `config` symlinked out of the mount was
/// already refused before this runs.
fn read_repo_config(
    operation: &'static str,
    config_path: Option<&Path>,
) -> Result<gix_config::File, GitError> {
    let Some(path) = config_path else {
        return parse_config_bytes(operation, Path::new("config"), &[]);
    };
    let bytes = std::fs::read(path).map_err(|e| {
        GitError::repository(operation, "reading the repository config", path, e)
    })?;
    parse_config_bytes(operation, path, &bytes)
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
    has_reftable: bool,
    has_refs: bool,
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
    // The `reftable`/`refs` probes are made by the caller through `open_leaf`,
    // so a `reftable` or `refs` symlinked out of the mount was refused before
    // we got here rather than followed to decide the backend.
    if has_reftable {
        return Err(GitError::UnsupportedRefBackend {
            operation,
            backend: "reftable".to_string(),
            repo: common_dir.to_path_buf(),
        });
    }
    // The files backend's own marker. Its absence means we are looking at
    // storage we have not identified, and guessing is what E.5 forbids.
    if !has_refs {
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

/// What resolving a fixed-name leaf under a checked parent found.
enum Leaf {
    /// A real path, inside the ceiling, safe to read.
    At(PathBuf),
    /// The leaf does not exist.
    Absent,
}

impl Leaf {
    /// The resolved path, or `None` when the leaf was absent.
    fn path(&self) -> Option<&Path> {
        match self {
            Leaf::At(p) => Some(p),
            Leaf::Absent => None,
        }
    }
}

/// How much of a `.git` file we will read before calling it malformed. A real
/// one is a single short line; this only stops a hostile "file" from being a
/// terabyte.
const MAX_DOT_GIT_FILE_BYTES: u64 = 4096;

/// Refuse a `.git` *file* whose `gitdir:` line leaves the mount, before
/// discovery ever resolves it.
///
/// Walks from `start` up to `ceiling` looking for the `.git` entry discovery
/// would find. A directory is an ordinary repository and needs no screening. A
/// file is the one discovery input a repository fully controls — its
/// `gitdir:` line can name any absolute path on the host — so its target goes
/// through [`contain`], which answers "outside the mount" and "does not
/// resolve" with the *same* refusal.
///
/// A `.git` that is itself a symlink is handled by [`open_leaf`], on the same
/// terms.
fn screen_gitdir_file(
    operation: &'static str,
    start: &Path,
    ceiling: &Path,
) -> Result<(), GitError> {
    const WHAT: &str = "git directory (the `gitdir:` line in its .git file)";

    // Start from a canonical directory so `open_leaf`'s "real leaf under a
    // canonical parent is contained by construction" reasoning holds.
    let Ok(mut dir) = std::fs::canonicalize(start) else {
        // Nothing we can screen; discovery will report the missing path.
        return Ok(());
    };
    if !dir.starts_with(ceiling) {
        return Ok(());
    }

    loop {
        match open_leaf(operation, ".git entry", &dir, ".git", ceiling)? {
            Leaf::At(dot_git) => {
                let meta = match std::fs::symlink_metadata(&dot_git) {
                    Ok(m) => m,
                    Err(_) => return Ok(()),
                };
                if meta.is_dir() {
                    // An ordinary repository: nothing repository-controlled
                    // about where its git dir lives.
                    return Ok(());
                }
                if meta.len() > MAX_DOT_GIT_FILE_BYTES {
                    return Err(GitError::repository(
                        operation,
                        "reading the .git file",
                        &dir,
                        std::io::Error::new(
                            std::io::ErrorKind::InvalidData,
                            format!(
                                "the .git file is {} bytes; a real one names a \
                                 single directory",
                                meta.len()
                            ),
                        ),
                    ));
                }
                let Ok(text) = std::fs::read_to_string(&dot_git) else {
                    // Unreadable or not UTF-8 — discovery will say so, and it
                    // is not a containment question.
                    return Ok(());
                };
                let Some(target) = text
                    .lines()
                    .find_map(|l| l.trim().strip_prefix("gitdir:"))
                    .map(str::trim)
                    .filter(|t| !t.is_empty())
                else {
                    return Ok(());
                };
                // A relative `gitdir:` is relative to the directory holding
                // the `.git` file, exactly as git resolves it.
                let candidate = {
                    let raw = Path::new(target);
                    if raw.is_absolute() {
                        raw.to_path_buf()
                    } else {
                        dir.join(raw)
                    }
                };
                contain(operation, WHAT, &dir, &candidate, ceiling)?;
                return Ok(());
            }
            // No `.git` here — keep walking up, but never past the mount.
            Leaf::Absent => {
                if dir == ceiling {
                    return Ok(());
                }
                match dir.parent() {
                    Some(parent) if parent.starts_with(ceiling) => dir = parent.to_path_buf(),
                    _ => return Ok(()),
                }
            }
        }
    }
}

/// Build a non-echoing exit-4 refusal for a repository-controlled path that
/// left the mount.
///
/// The escaping path is never named — see [`GitError::EscapesMount`]. `repo`
/// is a directory the caller already knows about (the git dir, the common
/// dir), so naming it leaks nothing.
fn escapes(operation: &'static str, what: &'static str, repo: &Path, ceiling: &Path) -> GitError {
    GitError::EscapesMount {
        operation,
        what,
        repo: repo.to_path_buf(),
        ceiling: ceiling.to_path_buf(),
    }
}

/// Resolve `parent/name` for reading, refusing a symlinked leaf whose target
/// leaves the ceiling.
///
/// The escape the whole `commondir` family shares: a lexical ceiling check
/// covers a *directory*, but every file and probe under it is reached by
/// joining a fixed name onto that directory, and the leaf can be a symlink the
/// repository planted. `<common_dir>/config` as a symlink to `/etc/shadow` is
/// inside the ceiling to a string comparison and `/etc/shadow` to `open`.
///
/// `parent` MUST already be canonical and inside `ceiling`, so only the final
/// component can introduce a symlink. `symlink_metadata` (lstat, no follow)
/// separates the cases without paying `canonicalize` on the common path where
/// the leaf is an ordinary file. A symlink is resolved and ceiling-checked;
/// one that escapes — or dangles — is refused without echoing where it aimed,
/// so the refusal cannot be read as an oracle for what exists on the host.
fn open_leaf(
    operation: &'static str,
    what: &'static str,
    parent: &Path,
    name: &str,
    ceiling: &Path,
) -> Result<Leaf, GitError> {
    let path = parent.join(name);
    let meta = match std::fs::symlink_metadata(&path) {
        Ok(m) => m,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Leaf::Absent),
        // `path` is `parent/<fixed name>`: `parent` is ours and `name` is a
        // constant, so this names nothing the caller does not already own.
        Err(e) => {
            return Err(GitError::repository(
                operation,
                format!("checking the {what}"),
                &path,
                e,
            ))
        }
    };
    if !meta.file_type().is_symlink() {
        // A real file or directory directly under a canonical parent is inside
        // the ceiling by construction.
        return Ok(Leaf::At(path));
    }
    match std::fs::canonicalize(&path) {
        Ok(real) if real.starts_with(ceiling) => Ok(Leaf::At(real)),
        _ => Err(escapes(operation, what, parent, ceiling)),
    }
}

/// Canonicalize a path whose value came from repository *content* and confirm
/// it is inside the ceiling.
///
/// The sibling of [`open_leaf`] for the two paths a repository writes out in
/// full rather than as a symlinked leaf: the `commondir` file's contents, and
/// each entry of `objects/info/alternates`. Non-echoing on both "resolves
/// outside" and "does not resolve", and the two are made indistinguishable on
/// purpose — a repository that learns whether `/some/candidate` exists by
/// reading which error it gets back has an oracle, so both give the same one.
fn contain(
    operation: &'static str,
    what: &'static str,
    repo: &Path,
    candidate: &Path,
    ceiling: &Path,
) -> Result<PathBuf, GitError> {
    match std::fs::canonicalize(candidate) {
        Ok(real) if real.starts_with(ceiling) => Ok(real),
        _ => Err(escapes(operation, what, repo, ceiling)),
    }
}

/// Refuse an object store whose `objects/info/alternates` reaches outside the
/// ceiling.
///
/// `gix-odb` honors `objects/info/alternates` — a file naming arbitrary
/// absolute object-store paths, no symlink required — and offers no option to
/// turn that off (`gix_odb::store::init::Options` carries only `slots` and
/// `use_multi_pack_index`). Left alone, a repository inside the mount could
/// name `/anywhere/objects` and have gix search it. So the same resolver gix
/// uses (`gix_odb::alternate::resolve`) runs first, matching its parsing and
/// its chain following, and every dir it yields is ceiling-checked before the
/// store is opened. Any escape, or any error resolving the chain, refuses the
/// store — non-echoing, since the paths came from repository content.
///
/// Two things this must get right that a first pass missed:
///
/// - `resolve`'s second argument is the *base* it joins a **relative**
///   alternate entry against (confirmed against the vendored gix-odb 0.83
///   source: it does `objects_directory.join(parsed_entry)`, then calls
///   `gix_path::realpath_opts(&path, current_dir, ..)`), not a ceiling —
///   `gix_odb::alternate::resolve` has no ceiling concept at all. Passing
///   `objects_dir` is what matches gix's own resolution.
/// - `resolve` hands back the *lexically* joined path, `..` components and
///   all — it never realpaths its own return value, only the copy it keeps
///   for cycle detection. A bare `dir.starts_with(ceiling)` on that raw path
///   is not a containment check: `objects_dir` itself already starts with
///   `ceiling`, so `objects_dir.join("../../../../outside/objects")` still
///   *lexically* starts with `ceiling` no matter how far the `..`s actually
///   walk — every relative alternate, escaping or not, passed unexamined.
///   `contain` is what actually resolves each entry and ceiling-checks the
///   result.
fn guard_alternates(
    operation: &'static str,
    objects_dir: &Path,
    ceiling: &Path,
) -> Result<(), GitError> {
    let what = "object store alternate (objects/info/alternates)";
    let alternates = gix_odb::alternate::resolve(objects_dir.to_path_buf(), objects_dir)
        .map_err(|_| escapes(operation, what, objects_dir, ceiling))?;
    for dir in alternates {
        contain(operation, what, objects_dir, &dir, ceiling)?;
    }
    Ok(())
}

/// One navigation step in a revision suffix (`~N`, `^`, `^N`).
enum Nav {
    /// `~N`: walk the first parent `N` times.
    FirstParentAncestor(usize),
    /// `^N`: the Nth parent. `^0` is the commit itself.
    NthParent(usize),
}

/// Recognize revision syntax outside the small grammar, returning the offending
/// form. The order matters: `...` is checked before `..`, and `^{` before a
/// bare `^` is treated as navigation.
fn unsupported_revspec(spec: &str) -> Option<String> {
    if spec.contains("@{") {
        return Some("@{...}".to_string());
    }
    if spec.contains("...") {
        return Some("...".to_string());
    }
    if spec.contains("..") {
        return Some("..".to_string());
    }
    if spec.contains("^{") {
        return Some("^{...}".to_string());
    }
    if spec.contains(":/") {
        return Some(":/...".to_string());
    }
    if spec.contains(':') {
        return Some("<rev>:<path> (that is `git show`, not `git log`)".to_string());
    }
    None
}

/// Validate a revision against the small grammar and split it into a base (a
/// ref or an oid) and its `~`/`^` navigation suffix. Shared by
/// [`ReadRepo::resolve_commit`] and [`ReadRepo::resolve_object`], so `log`,
/// `show` and `ls` all refuse the same unsupported forms the same way.
fn split_base_and_nav<'a>(
    operation: &'static str,
    spec: &'a str,
) -> Result<(&'a str, &'a str), GitError> {
    if let Some(bad) = unsupported_revspec(spec) {
        return Err(GitError::UnsupportedRevspec {
            operation,
            spec: spec.to_string(),
            syntax: bad,
        });
    }
    // The base ends at the first `~`/`^`; refs and oids contain neither.
    let split = spec.find(['~', '^']).unwrap_or(spec.len());
    let (base, nav) = spec.split_at(split);
    if base.is_empty() {
        return Err(GitError::UnsupportedRevspec {
            operation,
            spec: spec.to_string(),
            syntax: "a leading ~ or ^ with no revision".to_string(),
        });
    }
    Ok((base, nav))
}

/// Split `<rev>:<path>` at the first colon, matching git: `HEAD:a:b` is
/// revision `HEAD`, path `a:b` — a revision cannot contain a colon, so the
/// first one is unambiguous. `show` and `ls` call this themselves and hand
/// [`ReadRepo::resolve_object`] the revision half alone; `log` never splits
/// and refuses any colon through [`unsupported_revspec`] instead (D2 in the
/// PR4 design notes) — that is why this lives beside it as a free function
/// rather than on `ReadRepo`.
///
/// `:/text` is git's find-commit-by-message syntax — a different grammar
/// entirely, outside what this crate parses — and it is checked BEFORE the
/// generic split, because the split alone would read it as an empty revision
/// plus a `/text` path and report the wrong thing. An empty revision half
/// either way (`:path`, `::`) is its own loud usage error: a `--rev` this
/// short is never what the caller meant.
pub(crate) fn split_revision_and_path(
    operation: &'static str,
    spec: &str,
) -> Result<(String, Option<String>), GitError> {
    if spec.contains(":/") {
        return Err(GitError::UnsupportedRevspec {
            operation,
            spec: spec.to_string(),
            syntax: ":/...".to_string(),
        });
    }
    let Some(idx) = spec.find(':') else {
        return Ok((spec.to_string(), None));
    };
    let (rev, path) = (&spec[..idx], &spec[idx + 1..]);
    if rev.is_empty() {
        return Err(GitError::Usage {
            operation,
            message: format!(
                "'{spec}' names an empty revision before ':' — give one, e.g. \
                 'HEAD:{path}'"
            ),
        });
    }
    Ok((rev.to_string(), Some(path.to_string())))
}

/// Parse a `~`/`^` navigation suffix into a sequence of steps.
fn parse_nav(operation: &'static str, spec: &str, nav: &str) -> Result<Vec<Nav>, GitError> {
    let mut out = Vec::new();
    let mut chars = nav.chars().peekable();
    while let Some(c) = chars.next() {
        let is_tilde = match c {
            '~' => true,
            '^' => false,
            other => {
                return Err(GitError::UnsupportedRevspec {
                    operation,
                    spec: spec.to_string(),
                    syntax: other.to_string(),
                })
            }
        };
        let mut digits = String::new();
        while let Some(d) = chars.peek() {
            if d.is_ascii_digit() {
                digits.push(*d);
                chars.next();
            } else {
                break;
            }
        }
        // `~`/`^` with no number both mean 1; `^0` (the commit itself) is the
        // one place a zero is meaningful, so it is preserved.
        let n: usize = if digits.is_empty() {
            1
        } else {
            digits.parse().map_err(|_| GitError::UnsupportedRevspec {
                operation,
                spec: spec.to_string(),
                syntax: format!("{c}{digits} (number too large)"),
            })?
        };
        out.push(if is_tilde {
            Nav::FirstParentAncestor(n)
        } else {
            Nav::NthParent(n)
        });
    }
    Ok(out)
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
        // The config declaration path: no reftable dir on disk, refs present,
        // and the declaration alone must still refuse.
        let err = check_ref_backend("info", Path::new("/repo/.git"), &cfg, false, true)
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

    /// The on-disk `reftable/` directory signal, independent of the config
    /// declaration: `has_reftable = true` alone must refuse.
    #[test]
    fn reftable_directory_on_disk_is_refused() {
        let cfg = config("");
        let err = check_ref_backend("info", Path::new("/repo/.git"), &cfg, true, true)
            .expect_err("a reftable directory must be refused");
        assert_eq!(err.exit_code(), 4);
        assert!(err.to_string().contains("reftable"), "{err}");
    }

    /// A files repository with `refs/` present and no reftable is accepted.
    #[test]
    fn files_backend_with_refs_is_accepted() {
        let cfg = config("");
        check_ref_backend("info", Path::new("/repo/.git"), &cfg, false, true)
            .expect("the files backend is what we read");
    }

    /// No `refs/` directory at all is refused: it is storage we did not
    /// identify, and E.5 forbids guessing.
    #[test]
    fn missing_refs_directory_is_refused() {
        let cfg = config("");
        let err = check_ref_backend("info", Path::new("/repo/.git"), &cfg, false, false)
            .expect_err("no refs directory must be refused");
        assert_eq!(err.exit_code(), 4);
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
