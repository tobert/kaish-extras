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
    /// A file a write will replace or append to. A link, a directory or
    /// anything not a regular file refuses.
    File,
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
    let joined = rel
        .components()
        .filter_map(|c| match c {
            Component::Normal(n) => Some(n.to_string_lossy().into_owned()),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("/");
    let at = |base: &str| {
        joined == base
            || joined.strip_suffix(".lock") == Some(base)
            || joined.strip_prefix(base).is_some_and(|rest| rest.starts_with('/'))
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
    Err(GitError::EscapesMount {
        operation,
        what: root_kind.label(),
        repo: root.to_path_buf(),
        ceiling: ceiling.to_path_buf(),
    })
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

    let refuse = |path: &Path, found: &'static str| GitError::WriteThroughLink {
        operation,
        store: root_kind.label(),
        path: shown(path),
        found,
    };

    // Validate the whole relative path before touching the disk: a `..` or an
    // absolute component is a request to leave the root, wherever it sits.
    let mut names = Vec::new();
    for c in rel.components() {
        match c {
            Component::Normal(n) => names.push(n),
            Component::CurDir => {}
            Component::ParentDir | Component::RootDir | Component::Prefix(_) => {
                return Err(refuse(rel, "a path that leaves the directory it is relative to"));
            }
        }
    }
    if names.is_empty() {
        return Err(refuse(rel, "an empty path"));
    }
    // `.git` inside the work tree is the repository itself, never a target.
    if root_kind == Root::WorkTree && names[0] == ".git" {
        return Err(refuse(Path::new(names[0]), "the repository's own git directory"));
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
            Err(_) => return Err(refuse(&seen, "not accessible")),
        };
        let ty = meta.file_type();
        if ty.is_symlink() {
            return Err(refuse(&seen, "a symbolic link"));
        }
        if !last && !ty.is_dir() {
            return Err(refuse(&seen, "not a directory"));
        }
        if last {
            match leaf {
                Leaf::Dir if !ty.is_dir() => return Err(refuse(&seen, "not a directory")),
                Leaf::File if !ty.is_file() => return Err(refuse(&seen, "not a regular file")),
                _ => {}
            }
        }
    }
    Ok(abs)
}

/// A relative path as a refusal shows it: `/`-separated, lossy.
fn shown(path: &Path) -> String {
    path.components()
        .map(|c| c.as_os_str().to_string_lossy().into_owned())
        .collect::<Vec<_>>()
        .join("/")
}

#[cfg(test)]
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
    fn planted_symlinks_are_refused_without_leaking_or_writing() {
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
                text.contains(&format!("'{}' in {} is a symbolic link", case.expect_path, case.expect_store)),
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
    /// ceiling, and a root spelled through a link, are both refused.
    #[test]
    fn roots_outside_the_ceiling_are_refused() {
        let fx = Fixture::new();
        let ceiling = fx.mount.canonicalize().expect("canonical");
        let err = assert_inside_ceiling("commit", Root::GitDir, &fx.outside, &ceiling)
            .expect_err("outside the mount");
        assert_eq!(err.exit_code(), 4);

        let link = fx.mount.join("alias");
        std::os::unix::fs::symlink(&fx.outside, &link).expect("link");
        let err = assert_inside_ceiling("commit", Root::CommonDir, &link, &ceiling)
            .expect_err("a lexically inside root that is a link");
        assert_eq!(err.exit_code(), 4);

        assert_inside_ceiling("commit", Root::GitDir, &fx.repo.join(".git"), &ceiling)
            .expect("the repository's own git dir is inside");
    }
}
