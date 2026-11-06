//! [`WriteRepo`]: a [`ReadRepo`] plus the one handle that can write refs
//! (feasibility §7).
//!
//! `ReadRepo` keeps `WriteReflog::Disable` and never lends a transaction.
//! `WriteRepo` builds its own ref store with `WriteReflog::Normal`, so a ref
//! move through it appends a reflog line. Everything else (discovery,
//! revision resolution, the object reader) is `ReadRepo`'s, reused through
//! [`WriteRepo::read`].
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

        let options = gix_ref::store::init::Options {
            write_reflog: gix_ref::store::WriteReflog::Normal,
            prohibit_windows_device_names: cfg!(windows),
            ..Default::default()
        };
        // Same two layouts as `ReadRepo`'s store (repo.rs).
        let refs = if read.git_dir() == read.common_dir() {
            gix_ref::file::Store::at(read.git_dir().to_path_buf(), options)
        } else {
            gix_ref::file::Store::for_linked_worktree(
                read.git_dir().to_path_buf(),
                read.common_dir().to_path_buf(),
                options,
            )
        };
        Ok(Self { read, refs, _token: token })
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
            _ => self.common_dir(),
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

#[cfg(test)]
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
            repo.read().refs().write_reflog,
            gix_ref::store::WriteReflog::Disable
        ));
    }

    #[test]
    fn a_work_tree_check_on_a_bare_repository_needs_a_worktree() {
        let fx = Fixture::bare();
        let repo = fx.write_repo_at(&fx.repo);
        let err = repo.check_worktree(Path::new("a"), Leaf::File).expect_err("bare");
        assert!(matches!(err, GitError::NeedsWorktree { .. }), "{err}");
    }
}
