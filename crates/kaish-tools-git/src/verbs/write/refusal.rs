//! The five-part refusal every write verb uses (git-write-profile.md §3, W17).
//!
//! The write survey's second-turn probe settled the shape: a bare "disabled in
//! this shell" sent 4 of 5 models to plumbing, while a refusal that gave the
//! reason and one exact next command produced no bypass attempt in 40 cells.
//! Two models invented override flags when the text did not say whether one
//! exists. So a refusal always carries, in this order:
//!
//! 1. the command that was refused,
//! 2. the reason, in one clause,
//! 3. one exact next command,
//! 4. whether an override exists, and who grants it,
//! 5. "Nothing was changed."
//!
//! The fields are separate so a unit test can check each part is present;
//! [`Refusal`] renders them as one paragraph.

use std::fmt;

use crate::error::GitError;

/// Whether anything overrides a refusal, and who grants it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Override {
    /// A policy the embedder sets. No flag changes it.
    Embedder,
    /// A fact about the repository. No flag changes it; only real git, or the
    /// person who owns the repository, can.
    Repository,
    /// Nothing changes it: this shell never does the thing.
    Never,
    /// A flag of this same verb overrides it, and the caller grants it by
    /// typing the flag.
    Flag(&'static str),
    /// Variables in this shell override it, and the caller grants that by
    /// setting them. The embedder can also pin the value.
    Variables,
}

impl Override {
    fn sentence(&self) -> String {
        match self {
            Override::Embedder => "There is no flag that overrides this; the policy is set by \
                                   the program embedding this shell."
                .to_string(),
            Override::Repository => "There is no flag that overrides this; it depends on the \
                                     repository, which you or real git can change."
                .to_string(),
            Override::Never => "There is no flag or setting that overrides this; this shell never does it."
                .to_string(),
            Override::Variables => "Setting those variables in this shell overrides this; you grant \
                                    it by setting them, and the program embedding this shell can \
                                    pin the value instead."
                .to_string(),
            Override::Flag(flag) => {
                format!("The flag {flag} overrides this; you grant it by typing it.")
            }
        }
    }
}

/// One refusal, rendered as five parts.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Refusal {
    /// The exit code: 1 (a git-level no), 2 (usage or policy) or 4 (state or
    /// content this build will not write through).
    pub(crate) code: i64,
    /// The command as the caller typed its verb and flags, for example
    /// `git add -p`.
    pub(crate) command: String,
    /// Why, as a full sentence ending in a period.
    pub(crate) reason: String,
    /// The one exact next command, as a full sentence ending in a period.
    pub(crate) next: String,
    pub(crate) overridable: Override,
}

impl Refusal {
    pub(crate) fn new(
        code: i64,
        command: impl Into<String>,
        reason: impl Into<String>,
        next: impl Into<String>,
        overridable: Override,
    ) -> Self {
        Self {
            code,
            command: command.into(),
            reason: reason.into(),
            next: next.into(),
            overridable,
        }
    }

    pub(crate) fn into_error(self) -> GitError {
        GitError::Refused { code: self.code, text: self.to_string() }
    }
}

impl fmt::Display for Refusal {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "{}: refused. {} {} {} Nothing was changed.",
            self.command,
            self.reason,
            self.next,
            self.overridable.sentence()
        )
    }
}

/// Shorthand for the common shape: an `Err(GitError::Refused(..))`.
pub(crate) fn refuse<T>(
    code: i64,
    command: impl Into<String>,
    reason: impl Into<String>,
    next: impl Into<String>,
    overridable: Override,
) -> Result<T, GitError> {
    Err(Refusal::new(code, command, reason, next, overridable).into_error())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_refusal_renders_all_five_parts_in_order() {
        let text = Refusal::new(
            2,
            "git add -p",
            "This shell has no terminal to answer prompts.",
            "Stage whole files instead: git add <path>.",
            Override::Repository,
        )
        .to_string();
        let order = ["git add -p: refused.", "no terminal", "git add <path>", "no flag that overrides", "Nothing was changed."];
        let mut at = 0;
        for part in order {
            let found = text[at..].find(part).unwrap_or_else(|| panic!("{part:?} missing or out of order: {text}"));
            at += found + part.len();
        }
    }

    #[test]
    fn the_flag_override_names_the_flag_and_who_grants_it() {
        let text = Refusal::new(1, "git add x", "x is ignored.", "Run: git add -f x.", Override::Flag("-f")).to_string();
        assert!(text.contains("The flag -f overrides this; you grant it by typing it."), "{text}");
    }
}
