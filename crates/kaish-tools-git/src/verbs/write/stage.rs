//! Reading a working-tree path the way `git add` stores it (git-write-profile.md
//! §5.3, §6): `lstat`, never a followed link; a symlink stores its link text
//! with mode 120000; the exec bit comes from the `lstat` mode.
//!
//! Staging is two passes so a refusal writes nothing. [`inspect`] reads each
//! file, checks it for conversions, and keeps only its id and stat. Once every
//! file has passed, [`BlobWriter`](crate::verbs::write::objects::BlobWriter)
//! calls [`reread`] to get the bytes again and writes the object. A file that
//! changed in between fails the second pass before the index is touched.

use std::os::unix::fs::PermissionsExt;

use gix_index::entry::{Mode, Stat};
use gix_index::hash::ObjectId;

use crate::error::GitError;
use crate::verbs::write::attributes::{conversion_reason, AttributeReader};
use crate::verbs::write::containment::Leaf;
use crate::verbs::write::objects::blob_id;
use crate::verbs::write::refusal::{refuse, Override};
use crate::verbs::write::relpath::{os, show, RelPath};
use crate::verbs::write::repo::WriteRepo;
use crate::verbs::write::state::StagePolicy;

/// The largest file this build reads whole to stage: 128 MiB.
pub(crate) const MAX_STAGE_BYTES: u64 = 128 * 1024 * 1024;

/// A path that passed inspection.
#[derive(Debug, Clone)]
pub(crate) struct Staged {
    pub(crate) rel: RelPath,
    pub(crate) mode: Mode,
    pub(crate) stat: Stat,
    pub(crate) id: ObjectId,
}

/// The mode a working-tree file gets: git's, from the `lstat` mode, with
/// `core.filemode=false` keeping an existing entry's exec bit.
pub(crate) fn mode_of(meta: &std::fs::Metadata, existing: Option<Mode>, policy: &StagePolicy) -> Mode {
    if meta.file_type().is_symlink() {
        return Mode::SYMLINK;
    }
    if !policy.filemode {
        return match existing {
            Some(m) if m == Mode::FILE || m == Mode::FILE_EXECUTABLE => m,
            _ => Mode::FILE,
        };
    }
    if meta.permissions().mode() & 0o100 != 0 {
        Mode::FILE_EXECUTABLE
    } else {
        Mode::FILE
    }
}

/// Read `rel`, check it, and return what to stage. `Ok(None)` is not used:
/// every problem is a refusal that names the path.
pub(crate) fn inspect(
    repo: &WriteRepo,
    attrs: &mut AttributeReader<'_>,
    policy: &StagePolicy,
    command: &str,
    rel: &[u8],
    existing: Option<Mode>,
) -> Result<Staged, GitError> {
    let op = repo.read().operation();
    let shown = show(rel);
    let abs = repo.check_worktree(os(rel), Leaf::Entry)?;
    let meta = std::fs::symlink_metadata(&abs)
        .map_err(|e| GitError::repository(op, "reading a working-tree path", &abs, e))?;
    let ft = meta.file_type();
    if !ft.is_file() && !ft.is_symlink() {
        return refuse(
            4,
            command,
            format!("'{shown}' is neither a regular file, a symbolic link nor a git directory, and git stores nothing else."),
            "Leave it out of the command, or remove it from the working tree.",
            Override::Repository,
        );
    }
    // The stat is taken before the read: a write racing the read then leaves a
    // stat that does not match the content, which git treats as modified.
    let gmeta = gix_index::fs::Metadata::from_path_no_follow(&abs)
        .map_err(|e| GitError::repository(op, "reading a working-tree path", &abs, e))?;
    let stat = Stat::from_fs(&gmeta)
        .map_err(|e| GitError::repository(op, "reading a working-tree path", &abs, e))?;
    let bytes = match crate::worktree::read_worktree_blob(op, &shown, &abs, &meta, MAX_STAGE_BYTES) {
        Ok(b) => b,
        Err(GitError::BlobTooLarge { size, .. }) => {
            return refuse(
                1,
                command,
                format!("'{shown}' is {size} bytes, over the {MAX_STAGE_BYTES}-byte limit this shell reads to stage a file."),
                "Stage it with real git, outside this shell: git add <path>.",
                Override::Repository,
            )
        }
        Err(e) => return Err(e),
    };
    if ft.is_file() {
        let conv = attrs.conversions(rel)?;
        if let Some(why) = conversion_reason(&conv, policy, &bytes) {
            return refuse(
                4,
                command,
                format!("'{shown}' would be stored differently by real git: {why}. This shell stores the file's bytes as they are."),
                "Stage it with real git, outside this shell: git add <path>.",
                Override::Repository,
            );
        }
    }
    Ok(Staged {
        rel: rel.into(),
        mode: mode_of(&meta, existing, policy),
        stat,
        id: blob_id(op, &bytes)?,
    })
}

/// Read `staged` again for writing, and fail if it no longer hashes to the id
/// inspection computed.
pub(crate) fn reread(repo: &WriteRepo, staged: &Staged) -> Result<Vec<u8>, GitError> {
    let op = repo.read().operation();
    let abs = repo.check_worktree(os(&staged.rel), Leaf::Entry)?;
    let meta = std::fs::symlink_metadata(&abs)
        .map_err(|e| GitError::repository(op, "reading a working-tree path", &abs, e))?;
    let bytes = crate::worktree::read_worktree_blob(op, &show(&staged.rel), &abs, &meta, MAX_STAGE_BYTES)?;
    if blob_id(op, &bytes)? != staged.id {
        return Err(GitError::repository(
            op,
            format!("staging '{}'", show(&staged.rel)),
            &abs,
            std::io::Error::other("the file changed while it was being staged; run the command again"),
        ));
    }
    Ok(bytes)
}
