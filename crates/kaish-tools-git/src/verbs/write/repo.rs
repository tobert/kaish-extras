//! [`WriteRepo`]: a [`ReadRepo`] plus the one handle that can write refs
//! (feasibility §7).
//!
//! `ReadRepo` keeps `WriteReflog::Disable` and lends only [`OdbRead`] and
//! [`RefsRead`] (feasibility F1), types with no write method, so read code
//! cannot write whatever the scan misses. `WriteRepo` holds the real handles:
//! its own ref store with `WriteReflog::Normal`, so a ref move through it
//! appends a reflog line, and its own object store handle. Everything else
//! (discovery, revision resolution, the object reader) is `ReadRepo`'s,
//! reused through [`WriteRepo::read`].
//!
//! [`OdbRead`]: crate::repo::OdbRead
//! [`RefsRead`]: crate::repo::RefsRead
//!
//! A `WriteRepo` needs a [`WriteToken`], and a token comes only from the
//! commit profile's settings. A code path with no `WritableGitConfig` behind
//! it cannot build one.

use std::path::{Path, PathBuf};

use crate::config::WriteSettings;
use crate::error::GitError;
use crate::repo::ReadRepo;
use crate::verbs::write::containment::{self, Leaf, Root};

/// Proof that the embedder installed the commit profile.
///
/// Carries no authorization (git-write-profile.md §2): per-command approval
/// is the embedder's, through kaish `plan`.
#[derive(Debug, Clone, Copy)]
pub(crate) struct WriteToken(());

impl WriteToken {
    /// The only constructor: settings exist only on a `WritableGitConfig`.
    pub(crate) fn from_settings(_settings: &WriteSettings) -> Self {
        WriteToken(())
    }

    /// For unit tests, which have no config to take settings from.
    #[cfg(test)]
    pub(crate) fn test() -> Self {
        WriteToken(())
    }
}

/// A repository opened for writing.
pub(crate) struct WriteRepo {
    read: ReadRepo,
    /// The ref store that writes reflogs. A second store beside `ReadRepo`'s;
    /// `write_shaped_identifiers` counts constructions outside this directory.
    refs: gix_ref::file::Store,
    /// The same store with reflogs off, for a ref that gets none (a tag).
    /// gix-ref opens an existing log path even then, and fails on a directory
    /// there where git carries on.
    refs_unlogged: gix_ref::file::Store,
    /// The real object store handle, the one that can write objects.
    objects: gix_odb::Handle,
    _token: WriteToken,
}

impl WriteRepo {
    /// Wrap `read`, after asserting both of its roots are canonical and inside
    /// the mount ceiling (§6.1). `discover` proved it already; this is the
    /// boundary where a mistake costs a write.
    pub(crate) fn open(read: ReadRepo, token: WriteToken) -> Result<Self, GitError> {
        let op = read.operation();
        containment::assert_inside_ceiling(op, Root::GitDir, read.git_dir(), read.ceiling())?;
        containment::assert_inside_ceiling(op, Root::CommonDir, read.common_dir(), read.ceiling())?;

        // Same two layouts as `ReadRepo`'s store (repo.rs). Built once for
        // each reflog mode, from this one body, so the two cannot disagree.
        let build = |write_reflog| {
            let options = gix_ref::store::init::Options {
                write_reflog,
                prohibit_windows_device_names: cfg!(windows),
                ..Default::default()
            };
            if read.git_dir() == read.common_dir() {
                gix_ref::file::Store::at(read.git_dir().to_path_buf(), options)
            } else {
                gix_ref::file::Store::for_linked_worktree(
                    read.git_dir().to_path_buf(),
                    read.common_dir().to_path_buf(),
                    options,
                )
            }
        };
        let refs = build(gix_ref::store::WriteReflog::Normal);
        let refs_unlogged = build(gix_ref::store::WriteReflog::Disable);
        let objects = crate::repo::open_object_store(op, read.common_dir(), read.ceiling())?;
        Ok(Self { read, refs, refs_unlogged, objects, _token: token })
    }

    /// The read side: discovery, revisions, objects.
    pub(crate) fn read(&self) -> &ReadRepo {
        &self.read
    }

    /// The ref store that writes reflogs. Verbs reach it only after a
    /// containment check for the path the transaction will touch.
    pub(crate) fn refs(&self) -> &gix_ref::file::Store {
        &self.refs
    }

    /// [`Self::refs`] without reflogs. Same containment rule.
    pub(crate) fn refs_unlogged(&self) -> &gix_ref::file::Store {
        &self.refs_unlogged
    }

    /// The object store handle that can write objects.
    pub(crate) fn objects(&self) -> &gix_odb::Handle {
        &self.objects
    }

    pub(crate) fn git_dir(&self) -> &Path {
        self.read.git_dir()
    }

    pub(crate) fn common_dir(&self) -> &Path {
        self.read.common_dir()
    }

    /// Check a path under the git or common directory (the root is chosen by
    /// [`containment::root_for`]) and return its absolute form.
    pub(crate) fn check(&self, rel: &Path, leaf: Leaf) -> Result<PathBuf, GitError> {
        let root_kind = containment::root_for(rel);
        let root = match root_kind {
            Root::GitDir => self.git_dir(),
            Root::CommonDir => self.common_dir(),
            // `root_for` answers only for paths under a git directory. A
            // work tree root coming back means that function changed; crash
            // rather than check a path against the wrong root.
            Root::WorkTree => unreachable!("root_for never returns the work tree"),
        };
        containment::check_beneath(
            self.read.operation(),
            root_kind,
            root,
            self.read.ceiling(),
            rel,
            leaf,
        )
    }

    /// Check every file a ref move for `refname` touches, each against its
    /// own root, and return the absolute paths checked (F7).
    ///
    /// A ref move is not one path. gix-ref's files backend, for a transaction
    /// on `refs/heads/main`, takes `refs/heads/main.lock`, replaces
    /// `refs/heads/main`, appends to `logs/refs/heads/main`, and reads (and on
    /// a packed ref, rewrites through `packed-refs.lock`) `packed-refs`. A
    /// symlink planted at any of them is a write primitive, and the reflog
    /// is the one a check of the ref path alone misses. Set `also_head` when
    /// the move also appends to `HEAD`'s reflog: a commit or branch switch
    /// on the checked-out branch, where HEAD is symbolic. Moving `HEAD`
    /// itself (`refname == "HEAD"`, a detach) already covers `HEAD`,
    /// `HEAD.lock` and `logs/HEAD`.
    ///
    /// Each name goes through [`WriteRepo::check`], so a per-worktree name
    /// (`refs/bisect/*`, `HEAD`, `logs/HEAD`) is checked under the
    /// worktree's git directory and the rest under the common one.
    pub(crate) fn check_ref_write(&self, refname: &str, also_head: bool) -> Result<Vec<PathBuf>, GitError> {
        self.check_ref_write_with(refname, also_head, true)
    }

    /// [`Self::check_ref_write`] for a ref the write gives no reflog (a tag).
    /// Git opens the ref's log only if one exists, and a directory there is
    /// not an error, so only the directories above the log are walked.
    pub(crate) fn check_ref_write_unlogged(&self, refname: &str) -> Result<Vec<PathBuf>, GitError> {
        self.check_ref_write_with(refname, false, false)
    }

    fn check_ref_write_with(&self, refname: &str, also_head: bool, logged: bool) -> Result<Vec<PathBuf>, GitError> {
        let mut rels = vec![refname.to_string(), format!("{refname}.lock")];
        if logged {
            rels.push(format!("logs/{refname}"));
        }
        if also_head && refname != "HEAD" {
            rels.push("logs/HEAD".to_string());
        }
        rels.push("packed-refs".to_string());
        rels.push("packed-refs.lock".to_string());
        let mut paths: Vec<PathBuf> = rels.iter().map(|rel| self.check(Path::new(rel), Leaf::File)).collect::<Result<_, _>>()?;
        if !logged {
            if let Some(dir) = Path::new("logs").join(refname).parent() {
                paths.push(self.check(dir, Leaf::Dir)?);
            }
        }
        Ok(paths)
    }

    /// Check a path under the work tree root (`mv`, `add`, `rm`).
    pub(crate) fn check_worktree(&self, rel: &Path, leaf: Leaf) -> Result<PathBuf, GitError> {
        let op = self.read.operation();
        let Some(work) = self.read.work_dir() else {
            return Err(GitError::NeedsWorktree {
                operation: op,
                repo: self.git_dir().to_path_buf(),
            });
        };
        containment::check_beneath(op, Root::WorkTree, work, self.read.ceiling(), rel, leaf)
    }
}

impl WriteRepo {
    /// Check a work-tree path that may be a file, a symbolic link or a
    /// directory: the walk is the same, and the leaf kind is the one the disk
    /// shows. Components above the leaf must be real directories either way.
    pub(crate) fn check_worktree_any(&self, rel: &Path) -> Result<PathBuf, GitError> {
        let is_dir = self
            .read
            .work_dir()
            .map(|w| w.join(rel))
            .is_some_and(|p| std::fs::symlink_metadata(p).is_ok_and(|m| m.is_dir()));
        self.check_worktree(rel, if is_dir { Leaf::Dir } else { Leaf::Entry })
    }
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use crate::verbs::write::fixtures::Fixture;

    #[test]
    fn open_wraps_a_plain_and_a_linked_repository() {
        let fx = Fixture::new();
        let plain = fx.write_repo(false);
        assert_eq!(plain.git_dir(), plain.common_dir());
        let linked = fx.write_repo(true);
        assert_ne!(linked.git_dir(), linked.common_dir());
        // The read side still reads through the wrapper.
        assert!(linked.read().head().is_ok());
    }

    #[test]
    fn the_write_store_writes_reflogs_and_the_read_store_does_not() {
        let fx = Fixture::new();
        let repo = fx.write_repo(false);
        assert!(matches!(
            repo.refs().write_reflog,
            gix_ref::store::WriteReflog::Normal
        ));
        assert!(matches!(
            repo.read().refs().write_reflog(),
            gix_ref::store::WriteReflog::Disable
        ));
    }

    #[test]
    fn the_write_repo_holds_a_handle_that_writes_objects() {
        fn writes<T: gix_object::Write>(_: &T) {}
        let fx = Fixture::new();
        let repo = fx.write_repo(false);
        writes(repo.objects());
    }

    #[test]
    fn a_work_tree_check_on_a_bare_repository_needs_a_worktree() {
        let fx = Fixture::bare();
        let repo = fx.write_repo_at(&fx.repo);
        let err = repo.check_worktree(Path::new("a"), Leaf::File).expect_err("bare");
        assert!(matches!(err, GitError::NeedsWorktree { .. }), "{err}");
    }
}
