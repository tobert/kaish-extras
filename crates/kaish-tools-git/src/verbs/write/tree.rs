//! Writing the tree a commit points at (git-write-profile.md §2, feasibility
//! §2).
//!
//! `gix-index` has no write-tree, and `gix_object::Tree` writes its entries in
//! the order given, checking them only with a `debug_assert`. So this module
//! owns the order: git sorts a directory's entries by name with a `/` appended
//! to every tree's name, which puts `a.b` before the directory `a` and `a-b`
//! before both, and the oracle test in `tests/write_commit.rs` commits a
//! repository built to trip any other order.

use std::path::Path;

use gix_index::entry::Mode;
use gix_index::hash::ObjectId;
use gix_object::bstr::BString;
use gix_object::tree::{Entry, EntryKind};
use gix_object::{Exists as _, Kind, WriteTo as _};

use crate::error::GitError;
use crate::verbs::status::MAX_STATUS_TREE_DEPTH;
use crate::verbs::write::objects::BlobWriter;
use crate::verbs::write::refusal::{refuse, Override};
use crate::verbs::write::relpath::{show, RelPath};
use crate::verbs::write::repo::WriteRepo;

/// One file-like entry of the tree to write: a blob, a link or a gitlink.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Item {
    pub(crate) path: RelPath,
    pub(crate) mode: Mode,
    pub(crate) id: ObjectId,
}

/// Write the trees for `items` and return the root tree's id.
pub(crate) fn write_tree(
    repo: &WriteRepo,
    writer: &mut BlobWriter<'_>,
    command: &str,
    mut items: Vec<Item>,
) -> Result<ObjectId, GitError> {
    items.sort_by(|a, b| a.path.cmp(&b.path));
    if let Some(w) = items.windows(2).find(|w| w[0].path == w[1].path) {
        return refuse(
            4,
            command,
            format!("The index lists '{}' twice at stage 0.", show(&w[0].path)),
            "Repair the index with real git, outside this shell: git add <path>.",
            Override::Repository,
        );
    }
    build(repo, writer, command, &items, 0, 0)
}

/// The first `/`-separated component, and the rest, when more follow.
fn split_first_component(rest: &[u8]) -> Option<(&[u8], &[u8])> {
    let i = rest.iter().position(|b| *b == b'/')?;
    Some((&rest[..i], &rest[i + 1..]))
}

fn kind_of(mode: Mode) -> Option<EntryKind> {
    if mode == Mode::FILE {
        Some(EntryKind::Blob)
    } else if mode == Mode::FILE_EXECUTABLE {
        Some(EntryKind::BlobExecutable)
    } else if mode == Mode::SYMLINK {
        Some(EntryKind::Link)
    } else if mode == Mode::COMMIT {
        Some(EntryKind::Commit)
    } else {
        None
    }
}

/// A path component git would refuse to put in a tree. Checked on every
/// component of every entry, because a hostile index can name anything.
fn bad_component(name: &[u8]) -> bool {
    name.is_empty() || name == b"." || name == b".." || name.eq_ignore_ascii_case(b".git") || name.contains(&0)
}

fn build(
    repo: &WriteRepo,
    writer: &mut BlobWriter<'_>,
    command: &str,
    items: &[Item],
    prefix_len: usize,
    depth: usize,
) -> Result<ObjectId, GitError> {
    if depth > MAX_STATUS_TREE_DEPTH {
        return Err(GitError::TreeTooDeep { operation: "commit", limit: MAX_STATUS_TREE_DEPTH });
    }
    let mut entries: Vec<Entry> = Vec::new();
    let mut i = 0;
    while i < items.len() {
        let item = &items[i];
        let rest = &item.path[prefix_len..];
        match split_first_component(rest) {
            None => {
                if bad_component(rest) {
                    return bad_path(command, &item.path);
                }
                let Some(kind) = kind_of(item.mode) else {
                    return refuse(
                        4,
                        command,
                        format!("'{}' has an index mode this shell does not write into a tree.", show(&item.path)),
                        "Repair the index with real git, outside this shell: git add <path>.",
                        Override::Repository,
                    );
                };
                // A gitlink names a commit in another repository.
                if kind != EntryKind::Commit && !repo.objects().exists(&item.id) {
                    return refuse(
                        4,
                        command,
                        format!("The index names an object for '{}' that the object database does not hold.", show(&item.path)),
                        "Stage the file again: git add <path>.",
                        Override::Repository,
                    );
                }
                entries.push(Entry { mode: kind.into(), filename: BString::from(rest), oid: item.id });
                i += 1;
            }
            Some((name, _)) => {
                if bad_component(name) {
                    return bad_path(command, &item.path);
                }
                let dir_len = prefix_len + name.len() + 1;
                let end = i + items[i..]
                    .iter()
                    .take_while(|n| n.path.len() > dir_len && n.path[prefix_len..dir_len] == item.path[prefix_len..dir_len])
                    .count();
                let id = build(repo, writer, command, &items[i..end], dir_len, depth + 1)?;
                entries.push(Entry { mode: EntryKind::Tree.into(), filename: BString::from(name), oid: id });
                i = end;
            }
        }
    }
    // Git's order: names compared as bytes with `/` appended to a tree's.
    entries.sort_by_cached_key(sort_key);
    if let Some(w) = entries.windows(2).find(|w| w[0].filename == w[1].filename) {
        return refuse(
            4,
            command,
            format!("The index holds both a file and a directory named '{}'.", show(&w[0].filename)),
            "Repair the index with real git, outside this shell: git rm --cached <path>.",
            Override::Repository,
        );
    }
    let mut buf = Vec::new();
    gix_object::Tree { entries }
        .write_to(&mut buf)
        .map_err(|e| GitError::repository("commit", "encoding a tree", Path::new("."), e))?;
    writer.write_object(Kind::Tree, &buf)
}

fn sort_key(e: &Entry) -> Vec<u8> {
    let mut key = e.filename.to_vec();
    if e.mode.is_tree() {
        key.push(b'/');
    }
    key
}

fn bad_path<T>(command: &str, path: &[u8]) -> Result<T, GitError> {
    refuse(
        4,
        command,
        format!("'{}' has a component git never stores in a tree: empty, '.', '..' or '.git'.", show(path)),
        "Remove the entry with real git, outside this shell: git rm --cached <path>.",
        Override::Repository,
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_directory_sorts_as_if_its_name_ended_in_a_slash() {
        let mk = |name: &str, tree: bool| Entry {
            mode: if tree { EntryKind::Tree.into() } else { EntryKind::Blob.into() },
            filename: BString::from(name),
            oid: ObjectId::null(gix_index::hash::Kind::Sha1),
        };
        let mut entries = [mk("a", true), mk("a.b", false), mk("a-b", false), mk("a0", false), mk("b", false)];
        entries.sort_by_cached_key(sort_key);
        let names: Vec<String> = entries.iter().map(|e| e.filename.to_string()).collect();
        assert_eq!(names, ["a-b", "a.b", "a", "a0", "b"]);
    }

    #[test]
    fn dangerous_components_are_refused() {
        for bad in [&b""[..], b".", b"..", b".git", b".GIT", b".Git"] {
            assert!(bad_component(bad), "{bad:?}");
        }
        assert!(!bad_component(b".gitignore"));
    }
}
