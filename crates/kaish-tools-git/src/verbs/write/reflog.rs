//! Reflog message validation (git-write-profile.md §6; feasibility
//! surprise 2).
//!
//! `gix-ref 0.66` declares `Error::MessageWithNewlines` and never raises it:
//! the reflog writer does `writeln!(file, "\t{message}")` with the message
//! as given, so a `\n` in it forges a second reflog line. Every reflog
//! message passes through here before the ref transaction that would write it.

use crate::error::GitError;

/// Refuse a reflog message that contains `\n` or `\r`.
///
/// Returns the message unchanged when it is acceptable. Nothing is stripped:
/// writing a different message from the one the caller built would be a
/// silent change.
pub(crate) fn validate_message<'a>(
    operation: &'static str,
    message: &'a str,
) -> Result<&'a str, GitError> {
    if message.contains(['\n', '\r']) {
        return Err(GitError::ReflogMessageLineBreak { operation });
    }
    Ok(message)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn git_shaped_messages_pass_unchanged() {
        for m in [
            "commit: add two",
            "commit (initial): first",
            "commit (amend): fix",
            "branch: Created from HEAD",
            "checkout: moving from main to topic",
            "",
        ] {
            assert_eq!(validate_message("commit", m).expect("accepted"), m);
        }
    }

    #[test]
    fn a_line_break_is_refused_wherever_it_sits() {
        for m in [
            "commit: a\nb",
            "commit: a\r\nb",
            "commit: a\rb",
            "\ncommit: a",
            "commit: a\n",
            // The forged-line shape: a second reflog entry smuggled in.
            "commit: a\n0000000000000000000000000000000000000000 \
             1111111111111111111111111111111111111111 Mallory <m@x> 0 +0000\tcommit: forged",
        ] {
            let err = validate_message("commit", m).expect_err("must refuse");
            assert_eq!(err.exit_code(), 2, "{m:?}");
            let text = err.to_string();
            assert!(text.starts_with("git commit:"), "{text}");
            assert!(text.contains("nothing was written"), "{text}");
            assert!(!text.contains("forged"), "must not echo the message: {text}");
        }
    }
}
