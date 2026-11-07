//! Names for the refs `branch`, `tag` and `switch -c` create
//! (git-write-profile.md §4, §6).
//!
//! The rule is the one git applies to a new branch and to a new tag (checked
//! against git 2.56: `git tag HEAD`, `git branch HEAD` and `-x` all refuse): the
//! name is not `HEAD`, does not start with `-`, and `refs/heads/<name>`
//! (`refs/tags/<name>`) passes `check_refname_format`.
//! `gix-ref` owns the format check; `tests/write_refs.rs` holds a table of
//! names against real git so a difference between the two shows as a failure.
//!
//! The second half is the collision git reports when it locks the ref: a ref
//! at `refs/heads/a` makes `refs/heads/a/b` impossible and the reverse. gix-ref
//! reports that as an I/O error from deep inside its lock, so the check is
//! ours, with git's wording.

use crate::error::GitError;
use crate::verbs::write::refusal::{refuse, Override};
use crate::verbs::write::repo::WriteRepo;

/// Which namespace a name goes into.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum RefKind {
    Branch,
    Tag,
}

impl RefKind {
    pub(crate) fn word(self) -> &'static str {
        match self {
            RefKind::Branch => "branch",
            RefKind::Tag => "tag",
        }
    }

    fn prefix(self) -> &'static str {
        match self {
            RefKind::Branch => "refs/heads/",
            RefKind::Tag => "refs/tags/",
        }
    }
}

/// The full ref name for `name`, or the refusal git gives.
pub(crate) fn full_name(kind: RefKind, command: &str, name: &str) -> Result<String, GitError> {
    let full = format!("{}{name}", kind.prefix());
    let bad = name == "HEAD"
        || name.starts_with('-')
        || name.is_empty()
        || gix_ref::FullName::try_from(full.as_str()).is_err();
    if !bad {
        return Ok(full);
    }
    let word = kind.word();
    refuse(
        2,
        command,
        format!(
            "'{}' is not a valid {word} name. A {word} name is not HEAD, does not start with '-', does not end with '/', '.' or '.lock', \
             has no '..', '@{{', '//', space, control character or any of ~ ^ : ? * [ \\, and no path part that starts with '.' or ends with '.lock'.",
            name.escape_debug()
        ),
        format!("Pick a plain name, for example: git {word} {}.", if kind == RefKind::Branch { "feature-x" } else { "v1.0" }),
        Override::Never,
    )
}

/// The ref that stands in the way of creating `full`, and git's reason.
///
/// Git's `cannot lock ref 'refs/heads/a/b': 'refs/heads/a' exists; cannot
/// create 'refs/heads/a/b'` and its reverse, for a ref below the new name.
pub(crate) fn blocked_by(repo: &WriteRepo, full: &str) -> Result<Option<String>, GitError> {
    let op = repo.read().operation();
    let read = |what: String, e: &dyn std::fmt::Display| {
        GitError::repository(op, what, repo.common_dir(), std::io::Error::other(e.to_string()))
    };
    // A ref at a leading part of the name.
    let mut at = 0;
    while let Some(i) = full[at..].find('/') {
        at += i;
        let part = &full[..at];
        at += 1;
        if !part.contains('/') {
            continue;
        }
        if exists(repo, part)?.is_some() {
            return Ok(Some(format!("'{part}' exists; cannot create '{full}'")));
        }
    }
    // A ref below the name. Every ref is listed and filtered: the iterator's
    // prefix form takes a type this crate does not depend on, and a name
    // collision is rare enough that one pass over the refs is fine.
    let below = format!("{full}/");
    let platform = repo.refs().iter().map_err(|e| read("opening packed-refs".to_string(), &e))?;
    let all = platform.all().map_err(|e| read("listing refs".to_string(), &e))?;
    for r in all {
        let r = r.map_err(|e| read("listing refs".to_string(), &e))?;
        if r.name.as_bstr().starts_with(below.as_bytes()) {
            return Ok(Some(format!("'{}' exists; cannot create '{full}'", r.name.as_bstr())));
        }
    }
    Ok(None)
}

/// The ref named exactly `full`, if there is one.
///
/// `try_find` takes a partial name and tries the prefix rules after the name
/// itself (`refs/tags/` + `refs/heads/x`), so a tag named `refs/heads/x` would
/// answer for the branch. The answer is kept only when its name is `full`.
pub(crate) fn exists(repo: &WriteRepo, full: &str) -> Result<Option<gix_ref::Reference>, GitError> {
    let found = repo
        .refs()
        .try_find(full)
        .map_err(|e| GitError::repository(repo.read().operation(), format!("reading ref '{full}'"), repo.common_dir(), e))?;
    Ok(found.filter(|r| r.name.as_bstr() == full.as_bytes()))
}
