//! Loose-object writes for the index verbs (git-write-profile.md §6, W15).
//!
//! Every blob goes through [`gix_object::Write`] on the repository's object
//! database handle, after the containment walk has vouched for
//! `objects/xx/yyyy`. The pinned crates fsync nothing, so this module does:
//! each new object file, and its fan-out directory, are synced before the
//! index lock commits, so the index never names an object a crash could lose.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

use gix_index::hash::ObjectId;
use gix_object::{Exists as _, Kind, Write as _};

use crate::error::GitError;
use crate::verbs::write::containment::Leaf;
use crate::verbs::write::repo::WriteRepo;

pub(crate) struct BlobWriter<'a> {
    repo: &'a WriteRepo,
    operation: &'static str,
    files: Vec<PathBuf>,
    dirs: BTreeSet<PathBuf>,
}

/// The blob id of `bytes`, without writing anything.
pub(crate) fn blob_id(operation: &'static str, bytes: &[u8]) -> Result<ObjectId, GitError> {
    gix_object::compute_hash(gix_index::hash::Kind::Sha1, Kind::Blob, bytes)
        .map_err(|e| GitError::repository(operation, "hashing a blob", Path::new("."), e))
}

impl<'a> BlobWriter<'a> {
    pub(crate) fn new(repo: &'a WriteRepo, operation: &'static str) -> Self {
        Self { repo, operation, files: Vec::new(), dirs: BTreeSet::new() }
    }

    /// Write `bytes` as a blob unless the database already holds it.
    pub(crate) fn write(&mut self, bytes: &[u8]) -> Result<ObjectId, GitError> {
        self.write_object(Kind::Blob, bytes)
    }

    /// Write `bytes` as an object of `kind` unless the database already holds
    /// it. The commit verb writes its trees and the commit through here, so
    /// one [`BlobWriter::sync`] covers everything the ref will name.
    pub(crate) fn write_object(&mut self, kind: Kind, bytes: &[u8]) -> Result<ObjectId, GitError> {
        let id = gix_object::compute_hash(gix_index::hash::Kind::Sha1, kind, bytes)
            .map_err(|e| GitError::repository(self.operation, "hashing an object", Path::new("."), e))?;
        if self.repo.objects().exists(&id) {
            return Ok(id);
        }
        let hex = id.to_string();
        let rel = format!("objects/{}/{}", &hex[..2], &hex[2..]);
        let abs = self.repo.check(Path::new(&rel), Leaf::Replaced)?;
        let written = self
            .repo
            .objects()
            .write_buf(kind, bytes)
            .map_err(|e| GitError::repository(self.operation, "writing an object", self.repo.common_dir(), std::io::Error::other(e.to_string())))?;
        if written != id {
            return Err(GitError::repository(
                self.operation,
                "writing an object",
                self.repo.common_dir(),
                std::io::Error::other("the object database stored a different id from the one computed"),
            ));
        }
        if let Some(dir) = abs.parent() {
            self.dirs.insert(dir.to_path_buf());
        }
        self.files.push(abs);
        Ok(id)
    }

    /// Fsync every object file written, then its directory.
    pub(crate) fn sync(self) -> Result<(), GitError> {
        let fail = |what: &str, path: &Path, e: std::io::Error| {
            GitError::repository(self.operation, what.to_string(), path, e)
        };
        for f in &self.files {
            std::fs::File::open(f)
                .and_then(|h| h.sync_all())
                .map_err(|e| fail("syncing a new object", f, e))?;
        }
        for d in &self.dirs {
            std::fs::File::open(d)
                .and_then(|h| h.sync_all())
                .map_err(|e| fail("syncing an object directory", d, e))?;
        }
        Ok(())
    }
}
