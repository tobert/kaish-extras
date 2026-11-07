//! HEAD's tree as a flat map, for the verbs that compare the index with it
//! (`rm`'s staged-changes check, `restore --staged`, `reset`).

use std::collections::BTreeMap;

use gix_index::entry::Mode;
use gix_index::hash::ObjectId;
use gix_object::bstr::BString;
use gix_object::tree::EntryKind;
use gix_object::{FindExt, TreeRefIter};

use crate::error::GitError;
use crate::verbs::status::MAX_STATUS_TREE_DEPTH;
use crate::verbs::write::relpath::RelPath;
use crate::verbs::write::repo::WriteRepo;

/// Every non-tree entry of HEAD's tree, by path. Empty on an unborn branch.
pub(crate) fn flatten(repo: &WriteRepo, op: &'static str) -> Result<BTreeMap<RelPath, (ObjectId, Mode)>, GitError> {
    match repo.read().head_tree_id()? {
        Some(root) => flatten_tree(repo, op, root),
        None => Ok(BTreeMap::new()),
    }
}

/// [`flatten`] for the tree of any commit.
pub(crate) fn flatten_commit(repo: &WriteRepo, op: &'static str, commit: ObjectId) -> Result<BTreeMap<RelPath, (ObjectId, Mode)>, GitError> {
    let mut buf = Vec::new();
    let tree = repo
        .objects()
        .find_commit(&commit, &mut buf)
        .map_err(|e| GitError::repository(op, "reading a commit", repo.git_dir(), e))?
        .tree();
    flatten_tree(repo, op, tree)
}

fn flatten_tree(repo: &WriteRepo, op: &'static str, root: ObjectId) -> Result<BTreeMap<RelPath, (ObjectId, Mode)>, GitError> {
    let mut out = BTreeMap::new();
    // An explicit stack, so a deep tree cannot overflow ours.
    let mut stack: Vec<(ObjectId, RelPath, usize)> = vec![(root, BString::default(), 0)];
    let mut buf = Vec::new();
    while let Some((id, prefix, depth)) = stack.pop() {
        if depth > MAX_STATUS_TREE_DEPTH {
            return Err(GitError::TreeTooDeep { operation: op, limit: MAX_STATUS_TREE_DEPTH });
        }
        let data = repo
            .objects()
            .find(&id, &mut buf)
            .map_err(|e| GitError::repository(op, "reading a tree", repo.git_dir(), e))?;
        for entry in TreeRefIter::from_bytes(data.data, gix_index::hash::Kind::Sha1) {
            let entry = entry.map_err(|e| GitError::repository(op, "decoding a tree", repo.git_dir(), e))?;
            let mut path = prefix.clone();
            if !path.is_empty() {
                path.push(b'/');
            }
            path.extend_from_slice(entry.filename);
            let mode = match entry.mode.kind() {
                EntryKind::Tree => {
                    stack.push((entry.oid.to_owned(), path, depth + 1));
                    continue;
                }
                EntryKind::Blob => Mode::FILE,
                EntryKind::BlobExecutable => Mode::FILE_EXECUTABLE,
                EntryKind::Link => Mode::SYMLINK,
                EntryKind::Commit => Mode::COMMIT,
            };
            out.insert(path, (entry.oid.to_owned(), mode));
        }
    }
    Ok(out)
}
