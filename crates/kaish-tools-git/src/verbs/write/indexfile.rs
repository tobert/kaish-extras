//! Reading, editing and writing `.git/index` (git-write-profile.md §6).
//!
//! The order is git's: take `index.lock`, read the index under the lock, edit,
//! write the new index into the lock, rename it into place. `gix-index`'s own
//! `File::write` locks after the read, which leaves a lost-update window git
//! does not have, so this module does not use it.
//!
//! A verb reads a [`Snapshot`] first, with no lock, and plans from it, so a
//! refusal never creates or removes a file in `.git`. It then takes the lock
//! and re-reads: if the bytes changed since the snapshot the verb plans again
//! from the locked read. The state that is written is always the locked one.
//!
//! What the pinned `gix-index` cannot write is dropped: the untracked cache,
//! fsmonitor and resolve-undo extensions (git rebuilds the first two), and the
//! cache-tree extension is removed because gix writes it stale. A split index
//! (`link`) and a sparse index are refused: `gix-index` would write a partial
//! one or drop the sparse markers.

use std::io::Write as _;
use std::path::{Path, PathBuf};

use gix_index::entry::{Flags, Mode, Stat};
use gix_index::hash::ObjectId;
use gix_index::State;
use gix_object::bstr::BStr;

use crate::error::GitError;
use crate::verbs::write::containment::Leaf;
use crate::verbs::write::refusal::{refuse, Override};
use crate::verbs::write::relpath::{os, show, RelPath};
use crate::verbs::write::repo::WriteRepo;

/// The index as it was on disk, and what it decoded to.
pub(crate) struct Snapshot {
    pub(crate) bytes: Vec<u8>,
    pub(crate) state: State,
    /// The index file's mtime; `None` when there was no index file.
    pub(crate) mtime: Option<filetime::FileTime>,
}

impl Snapshot {
    /// Read and vet the index. Takes no lock and writes nothing.
    pub(crate) fn read(repo: &WriteRepo, command: &str) -> Result<Self, GitError> {
        let op = repo.read().operation();
        let path = repo.check(Path::new("index"), Leaf::Replaced)?;
        let (bytes, mtime) = match std::fs::read(&path) {
            Ok(b) => {
                let m = std::fs::metadata(&path)
                    .map_err(|e| GitError::repository(op, "reading the index", &path, e))?;
                (b, Some(filetime::FileTime::from_last_modification_time(&m)))
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => (Vec::new(), None),
            Err(e) => return Err(GitError::repository(op, "reading the index", &path, e)),
        };
        let state = if mtime.is_none() {
            // No index file: a repository that has staged nothing yet.
            State::new(gix_index::hash::Kind::Sha1)
        } else if bytes.is_empty() {
            // A file of zero bytes is not an empty index. Git refuses it
            // ("index file smaller than expected", exit 128); reading it as
            // empty would let `commit -a` commit a tree with every tracked
            // file deleted.
            return Err(GitError::repository(
                op,
                "decoding the index",
                &path,
                std::io::Error::other("index file smaller than expected"),
            ));
        } else {
            crate::index_depth_guard::refuse_if_cache_tree_too_deep(op, &bytes)?;
            State::from_bytes(
                &bytes,
                mtime.unwrap_or_else(filetime::FileTime::zero),
                gix_index::hash::Kind::Sha1,
                gix_index::decode::Options::default(),
            )
            .map_err(|e| GitError::repository(op, "decoding the index", &path, e))?
            .0
        };
        vet(&state, command)?;
        Ok(Self { bytes, state, mtime })
    }
}

fn vet(state: &State, command: &str) -> Result<(), GitError> {
    if state.link().is_some() {
        return refuse(
            4,
            command,
            "The index is a split index, which this shell would write back as a partial one.",
            "Merge it with real git, outside this shell: git update-index --no-split-index.",
            Override::Repository,
        );
    }
    if state.is_sparse() {
        return refuse(
            4,
            command,
            "The index is a sparse index, whose directory entries this shell would drop.",
            "Expand it with real git, outside this shell: git sparse-checkout disable.",
            Override::Repository,
        );
    }
    if let Some((e, path)) = state
        .entries()
        .iter()
        .map(|e| (e, e.path(state)))
        .find(|(e, _)| e.stage() != gix_index::entry::Stage::Unconflicted)
    {
        let _ = e;
        return refuse(
            4,
            command,
            format!("'{}' has unresolved merge conflict entries in the index.", show(path)),
            "Resolve it with real git, outside this shell: git add <path> after fixing the file, or git rm <path>.",
            Override::Repository,
        );
    }
    Ok(())
}

/// An `index.lock` this process created and has not yet renamed into place.
pub(crate) struct IndexTx {
    lock_path: PathBuf,
    index_path: PathBuf,
    file: Option<std::fs::File>,
    pub(crate) snap: Snapshot,
    done: bool,
}

impl IndexTx {
    /// Create `index.lock` exclusively, then read the index under it.
    pub(crate) fn lock(repo: &WriteRepo, command: &str) -> Result<Self, GitError> {
        let op = repo.read().operation();
        let lock_path = repo.check(Path::new("index.lock"), Leaf::Replaced)?;
        let index_path = repo.check(Path::new("index"), Leaf::Replaced)?;
        let file = match std::fs::OpenOptions::new().write(true).create_new(true).open(&lock_path) {
            Ok(f) => f,
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
                return refuse(
                    4,
                    command,
                    "The file index.lock already exists in the git directory, so another git process may be running. This shell does not remove a lock it did not take.",
                    "Wait and run the same command again. If no other git process is running, remove the lock with real git's environment: rm .git/index.lock.",
                    Override::Repository,
                )
            }
            Err(e) => return Err(GitError::repository(op, "creating index.lock", &lock_path, e)),
        };
        // From here the lock exists and `Drop` removes it on any early return.
        let mut tx = Self { lock_path, index_path, file: Some(file), snap: Snapshot { bytes: Vec::new(), state: State::new(gix_index::hash::Kind::Sha1), mtime: None }, done: false };
        tx.snap = Snapshot::read(repo, command)?;
        Ok(tx)
    }

    pub(crate) fn state(&self) -> &State {
        &self.snap.state
    }

    pub(crate) fn state_mut(&mut self) -> &mut State {
        &mut self.snap.state
    }

    /// Smudge, write, sync and rename the lock into place.
    pub(crate) fn commit(mut self, repo: &WriteRepo) -> Result<(), GitError> {
        let op = repo.read().operation();
        let old_mtime = self.snap.mtime;
        {
            let state = &mut self.snap.state;
            state.remove_tree();
            state.remove_entries(|_, _, e| e.flags.contains(Flags::REMOVE));
            state.sort_entries();
            smudge_racy(repo, state, old_mtime);
        }
        let state = std::mem::replace(&mut self.snap.state, State::new(gix_index::hash::Kind::Sha1));
        let file = gix_index::File::from_state(state, self.index_path.clone());
        let mut lock = self.file.take().expect("the lock file is open until commit");
        {
            let mut out = std::io::BufWriter::with_capacity(64 * 1024, &mut lock);
            file.write_to(&mut out, gix_index::write::Options::default())
                .map_err(|e| GitError::repository(op, "writing the index", &self.lock_path, e))?;
            out.flush().map_err(|e| GitError::repository(op, "writing the index", &self.lock_path, e))?;
        }
        lock.sync_all().map_err(|e| GitError::repository(op, "syncing index.lock", &self.lock_path, e))?;
        drop(lock);
        // The target may have changed since `lock()`; walk it again.
        repo.check(Path::new("index"), Leaf::Replaced)?;
        std::fs::rename(&self.lock_path, &self.index_path)
            .map_err(|e| GitError::repository(op, "renaming index.lock into place", &self.index_path, e))?;
        self.done = true;
        Ok(())
    }
}

impl Drop for IndexTx {
    fn drop(&mut self) {
        if !self.done {
            // Only the lock this value created; a failure to remove it has no
            // caller left to tell, and the next run reports the stale lock.
            let _ = std::fs::remove_file(&self.lock_path);
        }
    }
}

/// Git's racy-entry rule on write (`ce_smudge_racily_clean_entry`).
///
/// An entry whose mtime second is not older than the index file's was never
/// proven clean by a later index. If the working-tree file still matches the
/// entry's stat but its content no longer hashes to the entry's id, a newer
/// index would make `git status` trust a stat that lies. Setting the size to 0
/// makes the next `git status` compare content. Applied to EVERY entry, the
/// ones this write touched included, against the old index file's mtime: that
/// is the threshold git uses (`istate->timestamp`), and it is never later than
/// the new index's, so it covers the new index as well. No old index (a new
/// repository) means nothing was ever recorded, and nothing is smudged.
///
/// Git compares seconds only unless built with `USE_NSEC`, and so does this.
/// An entry is smudged when its mtime second is not older than the old index
/// file's, the file's mtime second and size equal the entry's, and the file's
/// content does not hash to the entry's id (or cannot be read). An entry whose
/// content is unchanged keeps its size, as in git
/// (`a_racy_entry_with_unchanged_content_is_smudged_exactly_when_git_smudges_it`).
/// The stat comparison is narrower than git's `ce_match_stat_basic`, which also
/// compares ctime, inode, uid, gid and device; so an entry whose file differs
/// from the entry in one of those, and in content, can be smudged here where
/// git leaves it alone. Git's next `status` rehashes that entry either way.
/// A file that is gone is not smudged: nothing can match it.
fn smudge_racy(repo: &WriteRepo, state: &mut State, old_mtime: Option<filetime::FileTime>) {
    let Some(old) = old_mtime else { return };
    if repo.read().work_dir().is_none() {
        return;
    }
    let threshold = old.unix_seconds() as u32;
    let (entries, paths) = state.entries_mut_and_pathbacking();
    for e in entries.iter_mut() {
        if e.mode == Mode::COMMIT || e.flags.contains(Flags::REMOVE) || e.stat.mtime.secs == 0 && e.stat.mtime.nsecs == 0 {
            continue;
        }
        if threshold > e.stat.mtime.secs {
            continue;
        }
        let rel: &[u8] = e.path_in(paths);
        if !crate::worktree::is_repo_relative_bytes(rel) {
            e.stat.size = 0;
            continue;
        }
        let Ok(abs) = repo.check_worktree(os(rel), Leaf::Entry) else {
            e.stat.size = 0;
            continue;
        };
        let Ok(meta) = gix_index::fs::Metadata::from_path_no_follow(&abs) else { continue };
        let Ok(now) = Stat::from_fs(&meta) else {
            e.stat.size = 0;
            continue;
        };
        if now.mtime.secs != e.stat.mtime.secs || now.size != e.stat.size {
            continue;
        }
        let Ok(std_meta) = std::fs::symlink_metadata(&abs) else { continue };
        match crate::worktree::read_worktree_blob("add", &show(rel), &abs, &std_meta, crate::verbs::write::stage::MAX_STAGE_BYTES) {
            Ok(bytes) => match crate::verbs::write::objects::blob_id("add", &bytes) {
                Ok(id) if id == e.id => {}
                _ => e.stat.size = 0,
            },
            Err(_) => e.stat.size = 0,
        }
    }
}

/// Look up the stage-0 entry for `path`.
pub(crate) fn find(state: &State, path: &BStr) -> Option<usize> {
    state.entry_index_by_path_and_stage(path, gix_index::entry::Stage::Unconflicted)
}

/// One entry to put in the index.
pub(crate) struct Upsert {
    pub(crate) path: RelPath,
    pub(crate) stat: Stat,
    pub(crate) id: ObjectId,
    pub(crate) mode: Mode,
}

/// Insert or replace stage-0 entries, replacing what collides as git's `add`
/// does: a file named like one of a new path's parent directories, and
/// anything below a new path that is now a file.
///
/// One pass over the index for all collisions, not one per path, so `add -A`
/// over many files stays linear. The state must be sorted on entry (it is, as
/// read); [`IndexTx::commit`] sorts again after the pushes.
pub(crate) fn upsert_all(state: &mut State, items: Vec<Upsert>) {
    if items.is_empty() {
        return;
    }
    let new_paths: std::collections::HashSet<&[u8]> = items.iter().map(|u| u.path.as_slice()).collect();
    let mut parents: std::collections::HashSet<&[u8]> = std::collections::HashSet::new();
    for u in &items {
        let mut cur: &[u8] = u.path.as_slice();
        while let Some(i) = cur.iter().rposition(|b| *b == b'/') {
            cur = &cur[..i];
            if !parents.insert(cur) {
                break;
            }
        }
    }
    {
        let (entries, paths) = state.entries_mut_and_pathbacking();
        for e in entries.iter_mut() {
            let p: &[u8] = e.path_in(paths);
            let mut collides = parents.contains(p);
            let mut cur: &[u8] = p;
            while !collides {
                let Some(i) = cur.iter().rposition(|b| *b == b'/') else { break };
                cur = &cur[..i];
                collides = new_paths.contains(cur);
            }
            if collides {
                e.flags.insert(Flags::REMOVE);
            }
        }
    }
    let mut pending = Vec::new();
    for u in items {
        match find(state, u.path.as_slice().into()) {
            Some(i) => {
                let e = &mut state.entries_mut()[i];
                e.stat = u.stat;
                e.id = u.id;
                e.mode = u.mode;
                e.flags.remove(Flags::REMOVE | Flags::INTENT_TO_ADD);
            }
            None => pending.push(u),
        }
    }
    for u in pending {
        state.dangerously_push_entry(u.stat, u.id, Flags::empty(), u.mode, u.path.as_slice().into());
    }
}

/// Mark the stage-0 entry for `path` for removal.
pub(crate) fn remove(state: &mut State, path: &[u8]) {
    if let Some(i) = find(state, path.into()) {
        state.entries_mut()[i].flags.insert(Flags::REMOVE);
    }
}
