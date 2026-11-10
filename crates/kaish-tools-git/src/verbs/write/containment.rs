//! Write containment (git-write-profile.md §6.1).
//!
//! The read side's P13 carve-out (a symlinked loose ref can be read from
//! outside the mount, docs/issues.md) is a write primitive here. gix opens
//! loose objects, refs and reflogs by name and follows symlinks, so a
//! repository that plants `objects/ab`, `refs/heads/sub` or `logs/HEAD` as a
//! link receives our write wherever the link points (feasibility surprise 1).
//!
//! The rule: before each write, `lstat` every component from **the root of the
//! store the write lands in** down to the target, and the target itself when
//! the write replaces or appends to it. A symbolic link, or a component that
//! is not what the walk needs, refuses the write (exit 4).
//!
//! What this does not stop: a process that swaps a component for a link
//! between the `lstat` and the write. The threat here is a hostile repository
//! at rest, not a concurrent attacker, and `docs/embedding-git.md` says so.
//! `openat2(RESOLVE_BENEATH)` would close it (kaish #276).
//!
//! Refusals name the offending component relative to its root, and never an
//! absolute path or a link target. The existence-oracle rule: a refusal that
//! echoed where a link points would let a hostile repository ask "does this
//! host path exist?" and read the answer.

use std::path::{Component, Path, PathBuf};

use crate::error::GitError;

/// Which root a path is relative to.
///
/// In a linked worktree these are three different directories: `HEAD`,
/// `index` and `logs/HEAD` live in the per-worktree git directory, objects
/// and refs in the common directory, and the work tree is a third place.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Root {
    /// The (per-worktree) git directory.
    GitDir,
    /// The common directory: `objects/`, `refs/`, `packed-refs`, `config`.
    CommonDir,
    /// The working tree root.
    WorkTree,
}

impl Root {
    /// The root as a refusal names it.
    fn label(self) -> &'static str {
        match self {
            Root::GitDir => "the git directory",
            Root::CommonDir => "the common git directory",
            Root::WorkTree => "the working tree",
        }
    }
}

/// What the final component of the walk must be, if it exists.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Leaf {
    /// A directory a write will create entries in.
    Dir,
    /// A file a write will modify in place or append to (a reflog, a ref
    /// written through its lock is not one). A link, a directory, anything not
    /// a regular file, or a file with more than one hard link refuses: an
    /// in-place write reaches every name the inode has, and the other names
    /// can be outside the mount. gix-ref 0.66 appends a reflog line with
    /// `OpenOptions::append` (`store/file/loose/reflog.rs:115-130`).
    File,
    /// A file a write replaces by renaming a new file over it (a loose
    /// object, a ref through its lock file). The old inode is never written,
    /// so extra hard links to it are harmless, and `git clone --local`
    /// makes them on purpose. A link, a directory or anything not a regular
    /// file still refuses.
    Replaced,
    /// A ref, or the reflog of one, that a write creates or appends to. As
    /// [`Leaf::File`], and a tree of empty directories also passes: git
    /// removes one in the way of a ref or a reflog and writes the file
    /// (checked on git 2.56 for `branch` and `tag`). A directory that holds a
    /// file or a link refuses, as git does.
    Ref,
    /// A name the walk only reads, deletes or renames: a regular file or a
    /// symbolic link, never followed. The link count does not matter, because
    /// unlink and rename change the directory entry and not the inode.
    Entry,
}

/// Which root a path under the git directory lands in.
///
/// Git's own rule (`path.c`, `common_list`), checked against
/// `git rev-parse --git-path` in a linked worktree by
/// `root_choice_matches_git_rev_parse_git_path`: a name is **per-worktree
/// unless it is on the common list**. A new per-worktree file git invents
/// (`MERGE_MSG`, `AUTO_MERGE`, `BISECT_START`, `config.worktree`) is
/// therefore right by default, where a list of per-worktree names was wrong
/// for each one it had not heard of.
///
/// Common: `branches`, `common`, `hooks`, `info`, `logs`, `lost-found`,
/// `objects`, `refs`, `remotes`, `worktrees`, `rr-cache`, `svn`, `config`,
/// `gc.pid`, `packed-refs`, `shallow`. Exceptions inside them that stay
/// per-worktree: `info/sparse-checkout`, `logs/HEAD`, and the `bisect`,
/// `worktree` and `rewritten` namespaces under both `refs/` and
/// `logs/refs/`. A name matches a base when it is the base, the base plus
/// `.lock`, or beneath the base; `refs/worktreex` is not `refs/worktree`.
/// For a plain repository both roots are the same directory.
pub(crate) fn root_for(rel: &Path) -> Root {
    use std::os::unix::ffi::OsStrExt as _;
    let joined = rel
        .components()
        .filter_map(|c| match c {
            Component::Normal(n) => Some(n.as_bytes().to_vec()),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join(&b'/');
    let at = |base: &str| {
        let base = base.as_bytes();
        joined == base
            || joined.strip_suffix(b".lock".as_slice()) == Some(base)
            || joined.strip_prefix(base).is_some_and(|rest| rest.starts_with(b"/"))
    };
    const PER_WORKTREE_INSIDE_COMMON: &[&str] = &[
        "info/sparse-checkout",
        "logs/HEAD",
        "logs/refs/bisect",
        "logs/refs/worktree",
        "logs/refs/rewritten",
        "refs/bisect",
        "refs/worktree",
        "refs/rewritten",
    ];
    const COMMON: &[&str] = &[
        "branches", "common", "hooks", "info", "logs", "lost-found", "objects", "refs",
        "remotes", "worktrees", "rr-cache", "svn", "config", "gc.pid", "packed-refs",
        "shallow",
    ];
    if PER_WORKTREE_INSIDE_COMMON.iter().any(|b| at(b)) || !COMMON.iter().any(|b| at(b)) {
        Root::GitDir
    } else {
        Root::CommonDir
    }
}

/// Assert that `root` is a canonical directory inside `ceiling`.
///
/// `discover` already established this for every root it hands out. The write
/// side asserts it again at the boundary where a mistake costs a write, not a
/// read (§6.1). `Path::starts_with` is lexical and blind to `..` and links,
/// so a lexical match alone proves nothing; the root must also equal its own
/// canonical form.
pub(crate) fn assert_inside_ceiling(
    operation: &'static str,
    root_kind: Root,
    root: &Path,
    ceiling: &Path,
) -> Result<(), GitError> {
    let canonical_and_inside =
        root.starts_with(ceiling) && std::fs::canonicalize(root).is_ok_and(|c| c == root);
    if canonical_and_inside {
        return Ok(());
    }
    // `root` derives from repository content (`gitdir:`, `commondir`), so it
    // is not echoed, and neither is the ceiling's relation to it. Read-side
    // `EscapesMount` may name its `repo` because every value it is built with
    // was first checked to be inside the mount; this root is the one that
    // failed that check.
    Err(GitError::WriteRootOutsideMount { operation, what: root_kind.label() })
}

/// Walk `rel` beneath `root`, refusing any link or wrong-typed component.
///
/// Components that do not exist yet end the walk successfully: the write will
/// create them beneath a directory this walk has already vouched for. Returns
/// the joined absolute path for the caller to write.
pub(crate) fn check_beneath(
    operation: &'static str,
    root_kind: Root,
    root: &Path,
    ceiling: &Path,
    rel: &Path,
    leaf: Leaf,
) -> Result<PathBuf, GitError> {
    assert_inside_ceiling(operation, root_kind, root, ceiling)?;

    let refuse = |path: &Path, found: &'static str, then: &'static str| GitError::WriteThroughLink {
        operation,
        store: root_kind.label(),
        path: shown(path),
        found,
        then,
    };

    // Validate the whole relative path before touching the disk: a `..` or an
    // absolute component is a request to leave the root, wherever it sits.
    let mut names = Vec::new();
    for c in rel.components() {
        match c {
            Component::Normal(n) => names.push(n),
            Component::CurDir => {}
            Component::ParentDir | Component::RootDir | Component::Prefix(_) => {
                return Err(refuse(rel, "a path that leaves the directory it is relative to", THEN_LEAVES));
            }
        }
    }
    if names.is_empty() {
        return Err(refuse(rel, "an empty path", THEN_LEAVES));
    }
    // A `.git` component in the work tree, at any depth, is a repository's own
    // directory and never a target (git's `verify_path` refuses it in the
    // index at any depth for the same reason). Compared without regard to
    // case, because on a case-insensitive filesystem `.GIT` is the same
    // directory, and against the NTFS short name `GIT~1`.
    if root_kind == Root::WorkTree {
        if let Some(bad) = names.iter().find(|n| is_dot_git(n)) {
            return Err(refuse(Path::new(bad), "a git directory", THEN_DOT_GIT));
        }
    }

    let mut abs = root.to_path_buf();
    let mut seen = PathBuf::new();
    for (i, name) in names.iter().enumerate() {
        abs.push(name);
        seen.push(name);
        let last = i + 1 == names.len();
        let meta = match std::fs::symlink_metadata(&abs) {
            Ok(m) => m,
            // Nothing here, so nothing below it either: the write creates it.
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                return Ok(root.join(names.iter().collect::<PathBuf>()));
            }
            Err(_) => return Err(refuse(&seen, "not accessible", THEN_NOT_ACCESSIBLE)),
        };
        let ty = meta.file_type();
        // What the write needs at this component: the last one is the leaf
        // kind, every earlier one is a directory.
        let needs_dir = !last || leaf == Leaf::Dir;
        if ty.is_symlink() && !(last && leaf == Leaf::Entry) {
            return Err(refuse(
                &seen,
                "a symbolic link",
                if needs_dir { THEN_LINK_NEEDS_DIR } else { THEN_LINK_NEEDS_FILE },
            ));
        }
        if needs_dir && !ty.is_dir() {
            return Err(refuse(&seen, "not a directory", THEN_NOT_DIR));
        }
        if !needs_dir {
            if leaf == Leaf::Entry {
                if ty.is_dir() {
                    return Err(refuse(&seen, "a directory", THEN_NOT_FILE));
                }
            } else if leaf == Leaf::Ref && ty.is_dir() {
                if !only_empty_directories(&abs, EMPTY_TREE_DEPTH) {
                    return Err(refuse(&seen, "a directory that holds files", THEN_NOT_FILE));
                }
                continue;
            } else if !ty.is_file() {
                return Err(refuse(&seen, "not a regular file", THEN_NOT_FILE));
            }
            if matches!(leaf, Leaf::File | Leaf::Ref) && has_other_hard_links(&meta) {
                return Err(refuse(&seen, "a file with other hard links", THEN_HARD_LINKED));
            }
        }
    }
    Ok(abs)
}

/// How deep [`only_empty_directories`] looks. A tree deeper than this is
/// refused as if it held a file; git has no such bound, and no real
/// repository leaves a tree this deep behind.
const EMPTY_TREE_DEPTH: usize = 32;

/// Whether `dir` holds nothing but directories, to `depth` levels, so git
/// would remove it. A link, a file, anything unreadable, or a tree deeper
/// than `depth` answers false.
fn only_empty_directories(dir: &Path, depth: usize) -> bool {
    let Some(depth) = depth.checked_sub(1) else { return false };
    let Ok(entries) = std::fs::read_dir(dir) else { return false };
    for entry in entries {
        let Ok(entry) = entry else { return false };
        // `DirEntry::file_type` does not follow a link.
        if !entry.file_type().is_ok_and(|t| t.is_dir()) || !only_empty_directories(&entry.path(), depth) {
            return false;
        }
    }
    true
}

const THEN_LEAVES: &str = "A write must stay beneath one directory of the repository, and this \
    one would leave it. This is a bug in the command; nothing outside the \
    repository was touched";
const THEN_DOT_GIT: &str = "A write never goes into a git directory from the working tree. Real \
    git refuses a .git path in the working tree too. Choose a path outside it";
const THEN_NOT_ACCESSIBLE: &str = "The write could not tell what is there, so it did not go on. Check the \
    permissions of that path using tools outside this repository's control";
const THEN_LINK_NEEDS_DIR: &str = "Writing through it could change files outside the mount. Real git \
    follows such links; this shell does not. Replace it with a real directory \
    using tools outside this repository's control, or use a clean clone";
const THEN_LINK_NEEDS_FILE: &str = "Writing through it could change a file outside the mount. Real git \
    follows such links; this shell does not. Replace it with a regular file \
    using tools outside this repository's control, or use a clean clone";
const THEN_NOT_DIR: &str = "The write needs a directory there. \
    Remove or rename it using tools outside this repository's control, or use \
    a clean clone";
const THEN_NOT_FILE: &str = "The write needs a regular file there. \
    Remove or rename it using tools outside this repository's control, or use \
    a clean clone";
const THEN_HARD_LINKED: &str = "Writing in place would also change every other name for it, and one of \
    them can be outside the mount. Real git appends in place without \
    checking; this shell does check. Replace it with a copy that has one name \
    using tools outside this repository's control, or use a clean clone";

/// Is `name` a `.git` directory name on any filesystem this crate runs on?
///
/// ASCII case-insensitive (`.GIT`, `.Git`), plus the NTFS 8.3 short name
/// `GIT~1`. HFS+ ignorable code points and trailing dots or spaces are not
/// covered: they matter only on filesystems whose mounts git itself guards
/// with `core.protectHFS` and `core.protectNTFS`.
fn is_dot_git(name: &std::ffi::OsStr) -> bool {
    name.eq_ignore_ascii_case(".git") || name.eq_ignore_ascii_case("git~1")
}

/// Does the file have a second name? Unix reports the link count; elsewhere
/// there is no cheap count in std, so the check is skipped and the
/// containment claim is the symlink walk alone.
#[cfg(unix)]
fn has_other_hard_links(meta: &std::fs::Metadata) -> bool {
    use std::os::unix::fs::MetadataExt;
    meta.nlink() > 1
}

#[cfg(not(unix))]
fn has_other_hard_links(_meta: &std::fs::Metadata) -> bool {
    false
}

/// A relative path as a refusal shows it: `/`-separated, lossy.
fn shown(path: &Path) -> String {
    path.components()
        .map(|c| c.as_os_str().to_string_lossy().into_owned()) // display-only
        .collect::<Vec<_>>()
        .join("/")
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use crate::verbs::write::fixtures::{git_stdout, Case, Fixture, CASES, SENTINEL};
    use std::path::Path;

    fn walk_outside(dir: &Path, out: &mut Vec<(String, u64, std::time::SystemTime)>) {
        for e in std::fs::read_dir(dir).expect("read outside") {
            let e = e.expect("entry");
            let m = std::fs::symlink_metadata(e.path()).expect("lstat");
            out.push((
                e.path().display().to_string(),
                m.len(),
                m.modified().expect("mtime"),
            ));
            if m.is_dir() {
                walk_outside(&e.path(), out);
            }
        }
    }

    fn outside_state(fx: &Fixture) -> Vec<(String, u64, std::time::SystemTime)> {
        let mut v = Vec::new();
        walk_outside(&fx.outside, &mut v);
        v.sort();
        v
    }

    fn run_case(fx: &Fixture, case: &Case) -> Result<PathBuf, GitError> {
        let repo = fx.write_repo(case.from_linked);
        repo.check(Path::new(case.rel), case.leaf)
    }

    /// Every planted-symlink fixture is refused (exit 4), names the planted
    /// component relative to its root, leaks no host path, and leaves the
    /// outside directory untouched.
    #[test]
    fn planted_links_are_refused_without_leaking_or_writing() {
        for case in CASES {
            let fx = Fixture::new();
            (case.plant)(&fx);
            let before = outside_state(&fx);
            assert!(!before.is_empty(), "{}: the outside sentinel is empty", case.name);

            let err = run_case(&fx, case).expect_err(case.name);
            assert_eq!(err.exit_code(), 4, "{}: {err}", case.name);
            let text = err.to_string();
            assert!(
                text.contains(&format!("'{}'", case.expect_path)),
                "{}: must name '{}' relative to its root: {text}",
                case.name,
                case.expect_path
            );
            assert!(text.contains(case.expect_store), "{}: {text}", case.name);
            // Say what was found, not just where: a link that is refused
            // only because a later check happened to trip on it would pass
            // the path assertion above.
            assert!(
                text.contains(&format!("'{}' in {} is {}", case.expect_path, case.expect_store, case.expect_found)),
                "{}: {text}",
                case.name
            );
            assert!(text.contains("nothing was written"), "{}: {text}", case.name);
            // The existence-oracle rule: no host path, and not where the link
            // points. `scratch` is a prefix of every path in the fixture.
            let scratch = fx.scratch().display().to_string();
            assert!(!text.contains(&scratch), "{}: leaked a host path: {text}", case.name);
            assert!(!text.contains(SENTINEL), "{}: leaked the link target: {text}", case.name);

            assert_eq!(outside_state(&fx), before, "{}: the outside tree changed", case.name);
            assert_eq!(
                std::fs::read_to_string(fx.outside.join("logs_head")).expect("sentinel"),
                "keep\n",
                "{}: the outside file changed",
                case.name
            );
        }
    }

    /// The negative control. A containment check that refuses everything would
    /// pass the test above; this is the test that fails it. The very same
    /// targets, on a repository with nothing planted, must be allowed.
    #[test]
    fn the_same_targets_pass_on_a_clean_repository() {
        for case in CASES {
            let fx = Fixture::new();
            let path = run_case(&fx, case)
                .unwrap_or_else(|e| panic!("{}: a clean repository was refused: {e}", case.name));
            assert!(path.is_absolute(), "{}", case.name);
            assert!(path.starts_with(fx.scratch()), "{}", case.name);
        }
    }

    /// The linked-worktree variant is only a test of root choice if the plant
    /// is under the root a naive check would not look at. Plant under the
    /// COMMON dir, then look from the git dir's `refs/`: nothing is there.
    #[test]
    fn a_linked_worktree_checks_the_common_dir_for_refs() {
        let fx = Fixture::new();
        let repo = fx.write_repo(true);
        assert_ne!(repo.git_dir(), repo.common_dir(), "the fixture must be a linked worktree");
        (CASES.iter().find(|c| c.name == "linked: refs/heads/sub in the common dir").expect("case").plant)(&fx);
        assert!(
            repo.git_dir().join("refs/heads/sub").symlink_metadata().is_err(),
            "the plant goes under the common dir only; checking the \
             per-worktree git dir's refs/ would find nothing and pass"
        );
        let err = repo
            .check(Path::new("refs/heads/sub/x.lock"), Leaf::File)
            .expect_err("must refuse");
        assert!(err.to_string().contains("the common git directory"), "{err}");
    }

    /// F6: the oracle is real git, not a table written from the same belief
    /// as `root_for`. `git rev-parse --git-path <name>`, run inside a linked
    /// worktree, says which directory git itself resolves each name to; the
    /// per-worktree git directory (`.git/worktrees/wt`) or the common one.
    #[test]
    fn root_choice_matches_git_rev_parse_git_path() {
        const NAMES: &[&str] = &[
            "HEAD", "index", "ORIG_HEAD", "MERGE_HEAD", "MERGE_MSG", "SQUASH_MSG",
            "COMMIT_EDITMSG", "AUTO_MERGE", "FETCH_HEAD", "BISECT_LOG", "BISECT_START",
            "BISECT_TERMS", "BISECT_EXPECTED_REV", "BISECT_ANCESTORS_OK", "BISECT_RUN",
            "BISECT_NAMES", "CHERRY_PICK_HEAD", "REVERT_HEAD", "REBASE_HEAD",
            "NOTES_MERGE_REF", "NOTES_MERGE_WORKTREE", "TAG_EDITMSG",
            "rebase-merge", "rebase-merge/head-name", "rebase-apply", "sequencer",
            "sequencer/todo", "logs/HEAD", "logs/HEAD.lock", "logs/refs/heads/x",
            "logs/refs/heads/x.lock", "logs/refs/remotes/o/h", "refs/heads/x",
            "refs/heads/x.lock", "refs/tags/v1", "refs/remotes/o/h", "refs/bisect/bad",
            "refs/bisect/bad.lock", "refs/worktree/x", "refs/rewritten/x",
            "logs/refs/bisect/x", "logs/refs/worktree/x", "logs/refs/rewritten/x",
            "packed-refs", "packed-refs.lock", "objects/ab", "objects/info/alternates",
            "objects/pack/x.pack", "config", "config.lock", "config.worktree", "shallow",
            "shallow.lock", "info/exclude", "info/sparse-checkout", "hooks/pre-commit",
            "HEAD.lock", "index.lock", "gc.pid", "gc.log", "modules/x/config",
            "rr-cache", "worktrees/wt/HEAD", "description", "branches/x", "remotes/x",
            "svn/x", "lost-found/x", "common/x", "logs", "refs", "logs/refs",
            // Case and prefix near-misses: git compares exactly.
            "refs/bisectx", "refs/Bisect/x", "logs/head", "logs/HEAD/x",
        ];
        let fx = Fixture::new();
        let git_dir = fx.linked_git_dir();
        let common = fx.common();
        for name in NAMES {
            let resolved = git_stdout(&fx.linked, &["rev-parse", "--git-path", name]);
            let resolved = PathBuf::from(resolved).canonicalize().unwrap_or_else(|_| {
                // The path may not exist; canonicalize the parent that does.
                let p = PathBuf::from(git_stdout(&fx.linked, &["rev-parse", "--git-path", name]));
                let mut up = p.clone();
                let mut tail = Vec::new();
                while !up.exists() {
                    tail.push(up.file_name().expect("name").to_owned());
                    up.pop();
                }
                let mut base = up.canonicalize().expect("canonical ancestor");
                for t in tail.iter().rev() {
                    base.push(t);
                }
                base
            });
            // Compare against `<root>/<name>` exactly: `worktrees/wt/HEAD` is
            // beneath the linked git dir's path but is the common dir's name.
            let want = if resolved == git_dir.join(name) {
                Root::GitDir
            } else {
                assert_eq!(resolved, common.join(name), "{name}: git resolved outside both roots");
                Root::CommonDir
            };
            assert_eq!(root_for(Path::new(name)), want, "{name}: git resolved it to {}", resolved.display());
        }
    }

    /// Where gix-ref 0.66 itself writes refs/bisect/* for a linked worktree
    /// (`file::Store::to_base_dir_and_relative_name`: `Bisect | Rewritten |
    /// WorktreePrivate | PseudoRef => self.git_dir`), checked by a real
    /// transaction rather than the source read: the loose ref and its reflog
    /// land where `root_for` says.
    #[test]
    fn gix_writes_per_worktree_refs_where_root_for_says() {
        use gix_ref::transaction::{Change, LogChange, PreviousValue, RefEdit, RefLog};
        let fx = Fixture::new();
        let repo = fx.write_repo(true);
        let head = gix_index::hash::ObjectId::from_hex(repo.read().head().expect("head").oid.expect("born").as_bytes()).expect("hex");
        for name in ["refs/bisect/bad", "refs/worktree/x", "refs/rewritten/x", "refs/heads/viagix"] {
            let edit = RefEdit {
                change: Change::Update {
                    log: LogChange { mode: RefLog::AndReference, force_create_reflog: true, message: "t".into() },
                    expected: PreviousValue::Any,
                    new: gix_ref::Target::Object(head),
                },
                name: name.try_into().expect("name"),
                deref: false,
            };
            repo.refs()
                .transaction()
                .prepare(
                    [edit],
                    gix_lock::acquire::Fail::Immediately,
                    gix_lock::acquire::Fail::Immediately,
                )
                .expect("prepare")
                .commit(Some(gix_actor::SignatureRef {
                    name: "T".into(),
                    email: "t@example.com".into(),
                    time: "0 +0000",
                }))
                .expect("commit");
            let root = match root_for(Path::new(name)) {
                Root::GitDir => fx.linked_git_dir(),
                _ => fx.common(),
            };
            assert!(root.join(name).is_file(), "{name}: the loose ref is not under {}", root.display());
        }
        // Per-worktree refs are not in the common directory at all.
        for name in ["refs/bisect/bad", "refs/worktree/x", "refs/rewritten/x"] {
            assert!(fx.common().join(name).symlink_metadata().is_err(), "{name} leaked into the common dir");
        }
    }

    fn plant_symlink_over(path: &Path, target: &Path) {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).expect("parent");
        }
        if path.is_dir() && !path.is_symlink() {
            std::fs::remove_dir_all(path).expect("clear dir");
        } else {
            let _ = std::fs::remove_file(path);
        }
        std::os::unix::fs::symlink(target, path).expect("plant");
    }

    /// F7: a ref move touches the ref, its lock, its reflog, `packed-refs`
    /// and, when HEAD follows, `logs/HEAD`. Checking the ref path alone
    /// passes a repository whose reflog is a link out of the mount.
    #[test]
    fn a_ref_write_checks_its_whole_write_set() {
        for linked in [false, true] {
            // Clean: every path passes, each under the right root.
            let fx = Fixture::new();
            let repo = fx.write_repo(linked);
            let paths = repo.check_ref_write("refs/heads/main", true).expect("clean repo");
            let common = repo.common_dir();
            let git_dir = repo.git_dir();
            assert_eq!(
                paths,
                [
                    common.join("refs/heads/main"),
                    common.join("refs/heads/main.lock"),
                    common.join("logs/refs/heads/main"),
                    git_dir.join("logs/HEAD"),
                    common.join("packed-refs"),
                    common.join("packed-refs.lock"),
                ],
                "linked={linked}"
            );

            // Each member of the set, planted as a link, refuses the move.
            // Where each lives for a main repository and a linked worktree:
            // the ref, its lock, its reflog and `packed-refs` in the common
            // directory, `logs/HEAD` in the worktree's own git directory.
            type Where = fn(&Fixture, bool) -> PathBuf;
            let head_logs: Where = |fx, linked| {
                if linked { fx.linked_git_dir().join("logs/HEAD") } else { fx.common().join("logs/HEAD") }
            };
            let plants: [(Where, &str); 5] = [
                (|fx, _| fx.common().join("logs/refs/heads/main"), "logs/refs/heads/main"),
                (|fx, _| fx.common().join("refs/heads/main"), "refs/heads/main"),
                (|fx, _| fx.common().join("refs/heads/main.lock"), "refs/heads/main.lock"),
                (|fx, _| fx.common().join("packed-refs"), "packed-refs"),
                (head_logs, "logs/HEAD"),
            ];
            for (at, expect) in plants {
                let fx = Fixture::new();
                // Discovery already refuses a `packed-refs` link that leaves the
                // mount, so that one points at a file inside it: the write
                // side refuses the link itself, wherever it goes.
                let target = if expect == "packed-refs" {
                    let inside = fx.mount.join("inside-the-mount");
                    std::fs::write(&inside, "x\n").expect("inside target");
                    inside
                } else {
                    fx.outside.join("logs_head")
                };
                plant_symlink_over(&at(&fx, linked), &target);
                let before = outside_state(&fx);
                let repo = fx.write_repo(linked);
                let err = repo.check_ref_write("refs/heads/main", true).expect_err(expect);
                assert_eq!(err.exit_code(), 4, "{expect}");
                assert!(err.to_string().contains(&format!("'{expect}' in ")), "{expect}: {err}");
                assert_eq!(outside_state(&fx), before, "{expect}");
            }

            // The control that makes `also_head` mean something: a planted
            // `logs/HEAD` refuses a move that also logs to HEAD, and passes a
            // move that does not.
            let fx = Fixture::new();
            plant_symlink_over(&head_logs(&fx, linked), &fx.outside.join("logs_head"));
            let repo = fx.write_repo(linked);
            repo.check_ref_write("refs/heads/main", false).expect("HEAD is not part of this move");
            repo.check_ref_write("refs/heads/main", true).expect_err("HEAD is part of this move");
            // Moving HEAD itself covers its own reflog without the flag.
            repo.check_ref_write("HEAD", false).expect_err("a HEAD move logs to logs/HEAD");
        }
    }

    /// The specific hole the review named: the ref path is fine, only its
    /// reflog is a link. Checking the ref path alone passes; the write set
    /// refuses.
    #[test]
    fn a_planted_reflog_is_caught_by_the_write_set_and_missed_by_the_ref_path_alone() {
        let fx = Fixture::new();
        plant_symlink_over(&fx.common().join("logs/refs/heads/main"), &fx.outside.join("logs_head"));
        let repo = fx.write_repo(false);
        repo.check(Path::new("refs/heads/main"), Leaf::File).expect("the ref path alone is clean");
        let err = repo.check_ref_write("refs/heads/main", false).expect_err("the reflog is a link");
        assert!(err.to_string().contains("'logs/refs/heads/main' in the common git directory is a symbolic link"), "{err}");
    }

    /// Per-worktree refs are checked under the worktree's git directory.
    #[test]
    fn a_per_worktree_ref_write_is_checked_under_the_worktree_git_dir() {
        let fx = Fixture::new();
        let repo = fx.write_repo(true);
        let paths = repo.check_ref_write("refs/bisect/bad", false).expect("clean");
        assert_eq!(paths[0], repo.git_dir().join("refs/bisect/bad"));
        assert_eq!(paths[2], repo.git_dir().join("logs/refs/bisect/bad"));
        assert_eq!(paths[3], repo.common_dir().join("packed-refs"));
        // A link in the worktree's own refs/bisect refuses.
        plant_symlink_over(&fx.linked_git_dir().join("refs/bisect/bad"), &fx.outside.join("logs_head"));
        let err = repo.check_ref_write("refs/bisect/bad", false).expect_err("link");
        assert!(err.to_string().contains("'refs/bisect/bad' in the git directory"), "{err}");
    }

    #[test]
    fn a_component_that_is_not_a_directory_is_refused() {
        let fx = Fixture::new();
        let repo = fx.write_repo(false);
        let heads = repo.common_dir().join("refs/heads");
        std::fs::remove_dir_all(&heads).expect("remove heads");
        std::fs::write(&heads, "not a directory").expect("plant file");
        let err = repo.check(Path::new("refs/heads/x.lock"), Leaf::File).expect_err("refuse");
        assert!(err.to_string().contains("'refs/heads' in the common git directory is not a directory"), "{err}");
    }

    #[test]
    fn an_existing_directory_where_a_file_is_needed_is_refused() {
        let fx = Fixture::new();
        let repo = fx.write_repo(false);
        let err = repo.check(Path::new("refs/heads"), Leaf::File).expect_err("refuse");
        assert!(err.to_string().contains("not a regular file"), "{err}");
        repo.check(Path::new("refs/heads"), Leaf::Dir).expect("a directory is a directory");
    }

    #[test]
    fn a_path_that_leaves_its_root_is_refused() {
        let fx = Fixture::new();
        let repo = fx.write_repo(false);
        for rel in ["../outside/x", "/etc/passwd", "refs/../../x", ""] {
            let err = repo.check(Path::new(rel), Leaf::File).expect_err(rel);
            assert_eq!(err.exit_code(), 4, "{rel}");
        }
    }

    /// A hard link is invisible to the `lstat` walk: the path is a regular
    /// file in the repository, and the inode is also named outside the mount.
    /// A write that reaches the inode changes both.
    #[test]
    fn a_hard_linked_file_is_refused_for_a_write_and_not_for_an_unlink() {
        let fx = Fixture::new();
        let repo = fx.write_repo(false);
        let outside_file = fx.outside.join("logs_head");
        let planted = repo.common_dir().join("logs/HEAD");
        std::fs::remove_file(&planted).expect("main logs/HEAD exists after a commit");
        std::fs::hard_link(&outside_file, &planted).expect("hard link");
        let before = std::fs::read(&outside_file).expect("outside");

        let err = repo.check(Path::new("logs/HEAD"), Leaf::File).expect_err("must refuse");
        assert_eq!(err.exit_code(), 4);
        let text = err.to_string();
        assert!(text.contains("'logs/HEAD' in the git directory is a file with other hard links"), "{text}");
        assert!(!text.contains(&fx.scratch().display().to_string()), "leaked a host path: {text}");
        assert_eq!(std::fs::read(&outside_file).expect("outside"), before);

        // Unlink and rename change the directory entry, not the inode.
        repo.check(Path::new("logs/HEAD"), Leaf::Entry).expect("an entry walk does not care about the link count");

        // The negative control: the same file with one link passes.
        std::fs::remove_file(&planted).expect("unlink");
        std::fs::write(&planted, "ordinary\n").expect("ordinary file");
        repo.check(Path::new("logs/HEAD"), Leaf::File).expect("a file with one link is fine");
    }

    #[test]
    fn the_worktree_variant_refuses_links_and_the_git_dir() {
        let fx = Fixture::new();
        let repo = fx.write_repo(false);
        let work = repo.read().work_dir().expect("work dir").to_path_buf();

        std::os::unix::fs::symlink(&fx.outside, work.join("planted")).expect("plant");
        let before = outside_state(&fx);

        let err = repo.check_worktree(Path::new("planted/file.txt"), Leaf::File).expect_err("link");
        assert_eq!(err.exit_code(), 4);
        let text = err.to_string();
        assert!(text.contains("'planted' in the working tree is a symbolic link"), "{text}");
        assert!(!text.contains(&fx.scratch().display().to_string()), "{text}");

        let err = repo.check_worktree(Path::new(".git/config"), Leaf::File).expect_err(".git");
        assert_eq!(err.exit_code(), 4);

        // Negative control: ordinary targets in the same tree pass.
        repo.check_worktree(Path::new("README.md"), Leaf::File).expect("existing file");
        repo.check_worktree(Path::new("new/dir/file.txt"), Leaf::File).expect("not yet existing");
        assert_eq!(outside_state(&fx), before);
    }

    /// `assert_inside_ceiling` must be able to fail: a root outside the
    /// ceiling, and a root spelled through a link, are both refused. F9: the
    /// refusal names no host path, because the root came from the
    /// repository's own content (`gitdir:`, `commondir`) and an echo would
    /// answer "does this path exist" for a hostile repository.
    #[test]
    fn roots_outside_the_ceiling_are_refused_without_echoing_them() {
        let fx = Fixture::new();
        let ceiling = fx.mount.canonicalize().expect("canonical");
        let scratch = fx.scratch().display().to_string();
        let link = fx.mount.join("alias");
        std::os::unix::fs::symlink(&fx.outside, &link).expect("link");

        for (what, root, label) in [
            ("outside the mount", fx.outside.clone(), "the git directory"),
            ("lexically inside, spelled through a link", link, "the common git directory"),
        ] {
            let kind = if label == "the git directory" { Root::GitDir } else { Root::CommonDir };
            let err = assert_inside_ceiling("commit", kind, &root, &ceiling).expect_err(what);
            assert_eq!(err.exit_code(), 4, "{what}");
            let text = err.to_string();
            assert!(text.contains(label), "{what}: {text}");
            assert!(text.contains("nothing was written"), "{what}: {text}");
            assert!(!text.contains(&scratch), "{what}: echoed a host path: {text}");
            assert!(!text.contains(SENTINEL), "{what}: echoed the root: {text}");
            assert!(!text.contains("alias"), "{what}: echoed the root: {text}");
        }

        assert_inside_ceiling("commit", Root::GitDir, &fx.repo.join(".git"), &ceiling)
            .expect("the repository's own git dir is inside");
    }

    /// F10: the text says what was found and what the write needed there. It
    /// claims "real git follows such links" only for a link, and names a
    /// directory or a file by what the write needs.
    #[test]
    fn refusal_wording_follows_what_was_found_and_what_the_write_needed() {
        let fx = Fixture::new();
        let repo = fx.write_repo(false);
        let common = repo.common_dir().to_path_buf();
        std::fs::create_dir_all(common.join("refs/heads/sub")).expect("dir");

        // A link where a directory is needed (a middle component).
        plant_symlink_over(&common.join("refs/heads/sub"), &fx.outside.join("refs_sub"));
        let t = repo.check(Path::new("refs/heads/sub/x"), Leaf::File).expect_err("link").to_string();
        assert!(t.contains("is a symbolic link"), "{t}");
        assert!(t.contains("Real git follows such links"), "{t}");
        assert!(t.contains("Replace it with a real directory"), "{t}");

        // A link where the write needs a file (the leaf).
        let t = repo.check(Path::new("refs/heads/sub"), Leaf::File).expect_err("link").to_string();
        assert!(t.contains("Replace it with a regular file"), "{t}");
        // A link where the write needs a directory (the leaf).
        let t = repo.check(Path::new("refs/heads/sub"), Leaf::Dir).expect_err("link").to_string();
        assert!(t.contains("Replace it with a real directory"), "{t}");

        // Not a link: no claim about following links.
        std::fs::write(common.join("refs/heads/file"), "x").expect("file");
        let t = repo.check(Path::new("refs/heads/file/x"), Leaf::File).expect_err("file").to_string();
        assert!(t.contains("is not a directory"), "{t}");
        assert!(!t.contains("follows such links"), "{t}");
        assert!(t.contains("needs a directory"), "{t}");
        let t = repo.check(Path::new("refs/heads"), Leaf::File).expect_err("dir").to_string();
        assert!(t.contains("is not a regular file"), "{t}");
        assert!(!t.contains("follows such links"), "{t}");
        assert!(t.contains("needs a regular file"), "{t}");
        let t = repo.check(Path::new("refs/heads/file"), Leaf::Dir).expect_err("file").to_string();
        assert!(t.contains("is not a directory"), "{t}");

        // A hard link: says other names, and does not call it a symbolic link.
        std::fs::hard_link(common.join("refs/heads/file"), common.join("refs/heads/second")).expect("hard link");
        let t = repo.check(Path::new("refs/heads/file"), Leaf::File).expect_err("hard link").to_string();
        assert!(t.contains("is a file with other hard links"), "{t}");
        assert!(t.contains("every other name"), "{t}");
        assert!(!t.contains("symbolic"), "{t}");
        // `Replaced` writes a new file over the old, so the extra name is harmless.
        repo.check(Path::new("refs/heads/file"), Leaf::Replaced).expect("replace-by-rename tolerates hard links");
    }

    /// F5's negative control and the cargo-clone case: a loose object that is
    /// a hard link (`git clone --local` makes them) is a fine rename target.
    #[test]
    fn a_hard_linked_file_is_refused_for_in_place_writes_only() {
        let fx = Fixture::new();
        let repo = fx.write_repo(false);
        let logs = repo.common_dir().join("logs/refs/heads/main");
        assert!(std::fs::metadata(&logs).expect("reflog").len() > 0);
        repo.check(Path::new("logs/refs/heads/main"), Leaf::File).expect("one name: fine");
        std::fs::hard_link(&logs, fx.scratch().join("second-name")).expect("hard link");
        repo.check(Path::new("logs/refs/heads/main"), Leaf::File).expect_err("two names");
        repo.check(Path::new("logs/refs/heads/main"), Leaf::Replaced).expect("replace by rename");
    }

    /// F11: a `.git` component in the work tree is refused at any depth and
    /// in any case; names that merely start with it are not.
    #[test]
    fn dot_git_in_the_work_tree_is_refused_in_any_case_and_depth() {
        let fx = Fixture::new();
        let repo = fx.write_repo(false);
        for rel in [".git/config", ".GIT/config", ".Git/hooks/pre-commit", "sub/.git/config", "GIT~1/config", ".git"] {
            let err = repo.check_worktree(Path::new(rel), Leaf::File).expect_err(rel);
            assert_eq!(err.exit_code(), 4, "{rel}");
            assert!(err.to_string().contains("a git directory"), "{rel}: {err}");
        }
        // Negative control: lookalikes are ordinary paths.
        for rel in [".gitignore", ".github/workflows/ci.yml", "a.git/x", "git/x", ".gitattributes"] {
            repo.check_worktree(Path::new(rel), Leaf::File).unwrap_or_else(|e| panic!("{rel}: {e}"));
        }
        // And `.git` is only special in the work tree: the git directory's
        // own tree has no such rule.
        repo.check(Path::new("refs/heads/.git"), Leaf::File).expect("not a work tree path");
    }
}
