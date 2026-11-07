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

/// Git's `git_parse_maybe_bool`, which `git_config_bool` wraps: `None` for a
/// key with no `=` (true), `""` is false, `true`/`yes`/`on` and
/// `false`/`no`/`off` in any case, and otherwise an integer where nonzero is
/// true (`git_parse_int`: decimal, `0x` hex or `0` octal, an optional sign and
/// an optional `k`, `m` or `g` factor, the result within an `int`). The inner
/// `None` is git's "bad boolean config value".
pub(crate) fn parse_git_bool(value: Option<&str>) -> Option<bool> {
    let Some(v) = value else { return Some(true) };
    if v.is_empty() {
        return Some(false);
    }
    let lower = v.to_ascii_lowercase();
    match lower.as_str() {
        "true" | "yes" | "on" => return Some(true),
        "false" | "no" | "off" => return Some(false),
        _ => {}
    }
    parse_git_int(v).map(|n| n != 0)
}

/// `git_parse_signed` with `maximum_signed_value_of_type(int)`.
fn parse_git_int(v: &str) -> Option<i64> {
    // strtoimax skips leading white space, then takes a sign.
    let t = v.trim_start_matches([' ', '\t', '\n', '\x0b', '\x0c', '\r']);
    let (neg, t) = match t.as_bytes().first()? {
        b'-' => (true, &t[1..]),
        b'+' => (false, &t[1..]),
        _ => (false, t),
    };
    let tb = t.as_bytes();
    let (radix, digits) = if tb.len() > 2 && (tb[0] == b'0') && (tb[1] | 0x20) == b'x' && tb[2].is_ascii_hexdigit() {
        (16, &t[2..])
    } else if tb.first() == Some(&b'0') {
        (8, t)
    } else {
        (10, t)
    };
    let end = digits.find(|c: char| !c.is_digit(radix)).unwrap_or(digits.len());
    if end == 0 {
        return None;
    }
    let mag = i128::from_str_radix(&digits[..end], radix).ok()?;
    let val = if neg { -mag } else { mag };
    if val > i128::from(i64::MAX) || val < i128::from(i64::MIN) {
        return None;
    }
    let factor: i128 = match &digits[end..] {
        "" => 1,
        "k" | "K" => 1024,
        "m" | "M" => 1024 * 1024,
        "g" | "G" => 1024 * 1024 * 1024,
        _ => return None,
    };
    // Measured against git 2.56: the scaled value must fit an `int`, and
    // -2147483648 does.
    i32::try_from(val * factor).ok().map(i64::from)
}

/// The last value of `section.key` (no subsection) in the repository's config,
/// and whether it has one: the inner `None` is a key written with no `=`.
fn config_entry(repo: &WriteRepo, section: &str, key: &str) -> Result<Option<Option<String>>, GitError> {
    Ok(repo
        .read()
        .config_entries(section, key)?
        .into_iter()
        .rfind(|(sub, _)| sub.is_none())
        .map(|(_, v)| v))
}

/// The last value of `section.key` (no subsection) in the repository's config.
/// A key with no `=` reads as an empty string here; ask [`config_bool`] for a
/// setting git reads as a boolean.
pub(crate) fn config_last(repo: &WriteRepo, section: &str, key: &str) -> Result<Option<String>, GitError> {
    Ok(config_entry(repo, section, key)?.map(Option::unwrap_or_default))
}

/// `section.key` read as git reads a boolean. `shown` is the key as git
/// spells it. A value git calls a bad boolean refuses with exit 4, as git
/// dies on it; `None` is a key that is not set.
pub(crate) fn config_bool(repo: &WriteRepo, command: &str, section: &str, key: &str, shown: &str) -> Result<Option<bool>, GitError> {
    let Some(entry) = config_entry(repo, section, key)? else { return Ok(None) };
    match parse_git_bool(entry.as_deref()) {
        Some(b) => Ok(Some(b)),
        None => refuse(
            4,
            command,
            format!(
                "{shown} is '{}', which is not a boolean, and git stops on it; this shell cannot tell whether git would act on it.",
                entry.unwrap_or_default()
            ),
            format!("Set it to true or false with real git, outside this shell: git config {shown} false."),
            Override::Repository,
        ),
    }
}

/// Refuse when the repository has no working tree, or a merge, rebase,
/// cherry-pick, revert or bisect is in progress. `what` completes "this shell
/// does not ... in the middle of one".
pub(crate) fn ensure_idle(repo: &WriteRepo, command: &str, what: &str) -> Result<(), GitError> {
    let op = repo.read().operation();
    if repo.read().work_dir().is_none() {
        return Err(GitError::NeedsWorktree { operation: op, repo: repo.git_dir().to_path_buf() });
    }
    for (marker, state, abort) in IN_PROGRESS {
        // `lstat`, never `exists`: a link planted here must not make us read
        // through it, and its presence alone answers the question.
        if std::fs::symlink_metadata(repo.git_dir().join(marker)).is_ok() {
            return refuse(
                4,
                command,
                format!("{state} is in progress in this repository, and this shell does not {what} in the middle of one."),
                format!("Finish or end it with real git, outside this shell: {abort}."),
                Override::Repository,
            );
        }
    }
    Ok(())
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
    ensure_idle(repo, command, "write the index")?;

    let bad = |key: &str, why: &str, fix: String| {
        refuse::<()>(
            4,
            command,
            format!("{key} {why}, and this shell would stage different content from real git."),
            fix,
            Override::Repository,
        )
    };
    if config_bool(repo, command, "core", "symlinks", "core.symlinks")? == Some(false) {
        bad("core.symlinks=false", "is set", "Use real git for this repository, outside this shell: git add <path>.".into())?;
    }
    if config_bool(repo, command, "core", "ignorecase", "core.ignoreCase")? == Some(true) {
        bad("core.ignorecase=true", "is set", "Use real git for this repository, outside this shell: git add <path>.".into())?;
    }
    if config_bool(repo, command, "core", "splitindex", "core.splitIndex")? == Some(true) {
        bad("core.splitIndex=true", "is set", "Turn it off with real git, outside this shell: git config core.splitIndex false.".into())?;
    }
    if config_bool(repo, command, "core", "sparsecheckout", "core.sparseCheckout")? == Some(true) {
        bad("core.sparseCheckout=true", "is set", "Use real git for this repository, outside this shell: git sparse-checkout disable.".into())?;
    }
    if config_last(repo, "core", "attributesfile")?.is_some() {
        bad("core.attributesFile", "is set to a file this shell does not read", "Unset it with real git, outside this shell: git config --unset core.attributesFile.".into())?;
    }

    if index_hook {
        ensure_no_index_hook(repo, command)?;
    }

    let autocrlf = match config_last(repo, "core", "autocrlf")?.as_deref() {
        Some(v) if v.eq_ignore_ascii_case("input") => Autocrlf::Input,
        _ => match config_bool(repo, command, "core", "autocrlf", "core.autocrlf")? {
            Some(true) => Autocrlf::True,
            _ => Autocrlf::Off,
        },
    };
    Ok(StagePolicy {
        filemode: config_bool(repo, command, "core", "filemode", "core.fileMode")?.unwrap_or(true),
        autocrlf,
        eol_set: config_last(repo, "core", "eol")?.is_some(),
    })
}

/// The directory git looks for hooks in: `core.hooksPath` when the
/// repository sets it at all, else `<common dir>/hooks`.
///
/// A relative `hooksPath` is resolved inside the working tree through the same
/// `lstat` walk every write uses, so a symbolic link in it refuses and is not
/// read through. A path that is absolute or leaves the working tree is refused
/// too, without being resolved: this shell cannot look there, so it cannot tell
/// which hooks would run. Both refusals name `core.hooksPath`.
pub(crate) fn hooks_dir(repo: &WriteRepo, command: &str) -> Result<std::path::PathBuf, GitError> {
    match config_last(repo, "core", "hookspath")? {
        None => Ok(repo.common_dir().join("hooks")),
        Some(path) => {
            let p = Path::new(&path);
            let inside = !p.is_absolute()
                && p.components().all(|c| matches!(c, std::path::Component::Normal(_) | std::path::Component::CurDir));
            if !inside || repo.read().work_dir().is_none() {
                return refuse(
                    4,
                    command,
                    "core.hooksPath points outside the working tree, so this shell cannot tell whether a hook would run.",
                    "Use real git for this write, outside this shell, for example: git add <path>.",
                    Override::Repository,
                );
            }
            let through_link = |at: &str, found: &str| {
                refuse(
                    4,
                    command,
                    format!("core.hooksPath is '{path}', and '{at}' on the way to it is {found}. This shell does not read through a link, so it cannot tell whether a hook would run."),
                    "Use real git for this write, outside this shell, for example: git add <path>.",
                    Override::Repository,
                )
            };
            match repo.check_worktree_any(p) {
                // The walk lets a link stand as the last component; the
                // hook files are read inside it, so it is refused here.
                Ok(dir) if std::fs::symlink_metadata(&dir).is_ok_and(|m| m.file_type().is_symlink()) => through_link(&path, "a symbolic link"),
                Ok(dir) => Ok(dir),
                Err(GitError::WriteThroughLink { found, path: at, .. }) => through_link(&at, found),
                Err(e) => Err(e),
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

/// Refuse when git would write no reflog where this shell would: this shell
/// always writes one, and a branch move that differs from git's there is not
/// one to make. That is `core.logAllRefUpdates` turned off, and a bare
/// repository that leaves it unset (git's default there is off).
pub(crate) fn ensure_reflog_on(repo: &WriteRepo, command: &str) -> Result<(), GitError> {
    let setting = config_last(repo, "core", "logallrefupdates")?;
    let off = match setting.as_deref() {
        Some(v) if v.eq_ignore_ascii_case("never") || v.eq_ignore_ascii_case("always") => v.eq_ignore_ascii_case("never"),
        Some(_) => config_bool(repo, command, "core", "logallrefupdates", "core.logAllRefUpdates")? == Some(false),
        None => false,
    };
    if off {
        return refuse(
            4,
            command,
            "core.logAllRefUpdates is off, and this shell always writes the reflog.",
            "Use real git, outside this shell, or turn it on: git config core.logAllRefUpdates true.",
            Override::Repository,
        );
    }
    if setting.is_none() && repo.read().is_bare() {
        return refuse(
            4,
            command,
            "This is a bare repository and core.logAllRefUpdates is not set, so git writes no reflog here, and this shell always writes one.",
            "Use real git, outside this shell, or turn it on: git config core.logAllRefUpdates true.",
            Override::Repository,
        );
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::parse_git_bool;

    /// Each row is what `git config --type=bool` printed (or `None` for
    /// "bad boolean config value") for that value, git 2.56. The integration
    /// test in `tests/write_refs.rs` runs the same table against live git.
    #[test]
    fn the_boolean_parser_matches_what_git_printed() {
        let rows: &[(Option<&str>, Option<bool>)] = &[
            (None, Some(true)),
            (Some(""), Some(false)),
            (Some("true"), Some(true)),
            (Some("TRUE"), Some(true)),
            (Some("tRuE"), Some(true)),
            (Some("yes"), Some(true)),
            (Some("YES"), Some(true)),
            (Some("on"), Some(true)),
            (Some("oN"), Some(true)),
            (Some("1"), Some(true)),
            (Some("2"), Some(true)),
            (Some("-1"), Some(true)),
            (Some("0"), Some(false)),
            (Some("00"), Some(false)),
            (Some("+0"), Some(false)),
            (Some("0x10"), Some(true)),
            (Some("0X1F"), Some(true)),
            (Some("0x0"), Some(false)),
            (Some("-0x1"), Some(true)),
            (Some("010"), Some(true)),
            (Some("1k"), Some(true)),
            (Some("1K"), Some(true)),
            (Some("1M"), Some(true)),
            (Some("1g"), Some(true)),
            (Some("0K"), Some(false)),
            (Some("0g"), Some(false)),
            (Some("false"), Some(false)),
            (Some("no"), Some(false)),
            (Some("off"), Some(false)),
            (Some("2147483647"), Some(true)),
            (Some("-2147483648"), Some(true)),
            (Some("2097151k"), Some(true)),
            (Some("-2097152k"), Some(true)),
            (Some("maybe"), None),
            (Some("08"), None),
            (Some("0x"), None),
            (Some("1kb"), None),
            (Some("1 k"), None),
            (Some("1.5"), None),
            (Some("2147483648"), None),
            (Some("-2147483649"), None),
            (Some("2g"), None),
            (Some("3g"), None),
            (Some("2097152k"), None),
            (Some("-2097153k"), None),
            (Some("99999999999g"), None),
        ];
        for (value, want) in rows {
            assert_eq!(parse_git_bool(*value), *want, "value {value:?}");
        }
    }
}
