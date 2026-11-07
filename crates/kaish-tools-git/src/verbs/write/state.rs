//! Repository states and settings an index write refuses to work in
//! (git-write-profile.md §4), and the settings that decide what `add` stores.
//!
//! Everything here reads: `lstat` on marker files and the repository's own
//! `config`. User and system config are never read (D.2), so the refusals are
//! a floor; the tool description says so.

use std::path::Path;

use crate::error::GitError;
use crate::verbs::write::refusal::{refuse, Override};
use crate::verbs::write::repo::WriteRepo;

/// What `core.autocrlf` is set to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Autocrlf {
    Off,
    True,
    Input,
}

/// The repository settings an index write consults.
#[derive(Debug, Clone, Copy)]
pub(crate) struct StagePolicy {
    /// `core.filemode`; default true.
    pub(crate) filemode: bool,
    pub(crate) autocrlf: Autocrlf,
    /// `core.eol` is set to anything.
    pub(crate) eol_set: bool,
}

/// A per-worktree marker and the operation it means is in progress.
const IN_PROGRESS: &[(&str, &str, &str)] = &[
    ("MERGE_HEAD", "a merge", "git merge --abort"),
    ("CHERRY_PICK_HEAD", "a cherry-pick", "git cherry-pick --abort"),
    ("REVERT_HEAD", "a revert", "git revert --abort"),
    ("REBASE_HEAD", "a rebase", "git rebase --abort"),
    ("rebase-merge", "a rebase", "git rebase --abort"),
    ("rebase-apply", "a rebase or patch application", "git rebase --abort"),
    ("sequencer", "a cherry-pick or revert sequence", "git cherry-pick --abort"),
    ("BISECT_LOG", "a bisect", "git bisect reset"),
];

fn parse_bool(v: &str) -> Option<bool> {
    match v.to_ascii_lowercase().as_str() {
        "" | "true" | "yes" | "on" | "1" => Some(true),
        "false" | "no" | "off" | "0" => Some(false),
        _ => None,
    }
}

/// The last value of `section.key` (no subsection) in the repository's config.
pub(crate) fn config_last(repo: &WriteRepo, section: &str, key: &str) -> Result<Option<String>, GitError> {
    Ok(repo
        .read()
        .config_values(section, key)?
        .into_iter()
        .rfind(|(sub, _)| sub.is_none())
        .map(|(_, v)| v))
}

fn config_flag(repo: &WriteRepo, section: &str, key: &str) -> Result<Option<bool>, GitError> {
    Ok(config_last(repo, section, key)?.and_then(|v| parse_bool(&v)))
}

/// Refuse when the repository is in a state an index write must not run in,
/// and return the settings the write needs. Writes nothing.
pub(crate) fn ensure_writable(repo: &WriteRepo, command: &str) -> Result<StagePolicy, GitError> {
    ensure_writable_with(repo, command, true)
}

/// [`ensure_writable`], where `index_hook` says whether a `post-index-change`
/// hook refuses. `commit` passes `false` and does its own hook accounting,
/// because an embedder's hook provider changes the answer.
pub(crate) fn ensure_writable_with(repo: &WriteRepo, command: &str, index_hook: bool) -> Result<StagePolicy, GitError> {
    let op = repo.read().operation();
    if repo.read().work_dir().is_none() {
        return Err(GitError::NeedsWorktree { operation: op, repo: repo.git_dir().to_path_buf() });
    }

    for (marker, what, abort) in IN_PROGRESS {
        // `lstat`, never `exists`: a link planted here must not make us read
        // through it, and its presence alone answers the question.
        if std::fs::symlink_metadata(repo.git_dir().join(marker)).is_ok() {
            return refuse(
                4,
                command,
                format!("{what} is in progress in this repository, and this shell does not write the index in the middle of one."),
                format!("Finish or end it with real git, outside this shell: {abort}."),
                Override::Repository,
            );
        }
    }

    let bad = |key: &str, why: &str, fix: String| {
        refuse::<()>(
            4,
            command,
            format!("{key} {why}, and this shell would stage different content from real git."),
            fix,
            Override::Repository,
        )
    };
    if config_flag(repo, "core", "symlinks")? == Some(false) {
        bad("core.symlinks=false", "is set", "Use real git for this repository, outside this shell: git add <path>.".into())?;
    }
    if config_flag(repo, "core", "ignorecase")? == Some(true) {
        bad("core.ignorecase=true", "is set", "Use real git for this repository, outside this shell: git add <path>.".into())?;
    }
    if config_flag(repo, "core", "splitindex")? == Some(true) {
        bad("core.splitIndex=true", "is set", "Turn it off with real git, outside this shell: git config core.splitIndex false.".into())?;
    }
    if config_flag(repo, "core", "sparsecheckout")? == Some(true) {
        bad("core.sparseCheckout=true", "is set", "Use real git for this repository, outside this shell: git sparse-checkout disable.".into())?;
    }
    if config_last(repo, "core", "attributesfile")?.is_some() {
        bad("core.attributesFile", "is set to a file this shell does not read", "Unset it with real git, outside this shell: git config --unset core.attributesFile.".into())?;
    }

    if index_hook {
        ensure_no_index_hook(repo, command)?;
    }

    let autocrlf = match config_last(repo, "core", "autocrlf")?.as_deref() {
        None => Autocrlf::Off,
        Some(v) if v.eq_ignore_ascii_case("input") => Autocrlf::Input,
        Some(v) => match parse_bool(v) {
            Some(true) => Autocrlf::True,
            Some(false) => Autocrlf::Off,
            // An unreadable value: git treats it as an error. Stay on the safe side.
            None => Autocrlf::True,
        },
    };
    Ok(StagePolicy {
        filemode: config_flag(repo, "core", "filemode")?.unwrap_or(true),
        autocrlf,
        eol_set: config_last(repo, "core", "eol")?.is_some(),
    })
}

/// The directory git looks for hooks in: `core.hooksPath` when the
/// repository sets it at all, else `<common dir>/hooks`.
///
/// A `hooksPath` outside the working tree is refused, not followed: this
/// shell cannot look there, so it cannot tell which hooks would run.
pub(crate) fn hooks_dir(repo: &WriteRepo, command: &str) -> Result<std::path::PathBuf, GitError> {
    match config_last(repo, "core", "hookspath")? {
        None => Ok(repo.common_dir().join("hooks")),
        Some(path) => {
            let p = Path::new(&path);
            let inside = !p.is_absolute()
                && p.components().all(|c| matches!(c, std::path::Component::Normal(_) | std::path::Component::CurDir));
            match (inside, repo.read().work_dir()) {
                (true, Some(work)) => Ok(work.join(p)),
                _ => refuse(
                    4,
                    command,
                    "core.hooksPath points outside the working tree, so this shell cannot tell whether a hook would run.",
                    "Use real git for this write, outside this shell, for example: git add <path>.",
                    Override::Repository,
                ),
            }
        }
    }
}

/// Git runs `post-index-change` after it writes the index. This shell cannot
/// run a hook, so a repository that has one is refused rather than skipped.
fn ensure_no_index_hook(repo: &WriteRepo, command: &str) -> Result<(), GitError> {
    const HOOK: &str = "post-index-change";
    let dir = hooks_dir(repo, command)?;
    if std::fs::symlink_metadata(dir.join(HOOK)).is_ok() {
        return refuse(
            4,
            command,
            "The repository has a post-index-change hook, and this shell runs no hooks.",
            "Use real git for this write, outside this shell, or ask the embedder for a hook provider.",
            Override::Embedder,
        );
    }
    Ok(())
}

/// Refuse when `core.logAllRefUpdates` is off: this shell always writes the
/// reflog, and a branch move that differs from git's there is not one to make.
pub(crate) fn ensure_reflog_on(repo: &WriteRepo, command: &str) -> Result<(), GitError> {
    if config_last(repo, "core", "logallrefupdates")?
        .is_some_and(|v| matches!(v.to_ascii_lowercase().as_str(), "false" | "no" | "off" | "0" | "never"))
    {
        return refuse(
            4,
            command,
            "core.logAllRefUpdates is off, and this shell always writes the reflog.",
            "Use real git, outside this shell, or turn it on: git config core.logAllRefUpdates true.",
            Override::Repository,
        );
    }
    Ok(())
}
