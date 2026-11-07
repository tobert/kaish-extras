//! Reflog messages (git-write-profile.md §6; feasibility surprise 2).
//!
//! `gix-ref 0.66` declares `Error::MessageWithNewlines` and never raises it:
//! the reflog writer does `writeln!(file, "\t{message}")` with the message
//! as given, so a `\n` in it forges a second reflog line. Every reflog
//! message passes through [`copy_message`] before the ref transaction that
//! would write it.
//!
//! [`copy_message`] is git's `copy_reflog_msg` (refs.c): it skips leading
//! whitespace, turns each run of whitespace into one space, and drops
//! trailing whitespace. Whitespace is git's own `isspace` (git-compat-util.h):
//! space, tab, newline and carriage return; a vertical tab or form feed is not
//! whitespace there and stays. So a commit subject with a
//! carriage return or a tab commits, and the reflog holds the same bytes git
//! would write.

/// The message as git writes it into a reflog line.
pub(crate) fn copy_message(message: &str) -> String {
    let is_space = |c: char| matches!(c, ' ' | '\t' | '\n' | '\r');
    let mut out = String::with_capacity(message.len());
    let mut was_space = true;
    for c in message.chars() {
        if was_space && is_space(c) {
            continue;
        }
        was_space = is_space(c);
        out.push(if was_space { ' ' } else { c });
    }
    out.truncate(out.trim_end_matches(' ').len());
    out
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
            assert_eq!(copy_message(m), m);
        }
    }

    #[test]
    fn whitespace_collapses_as_git_collapses_it() {
        for (given, want) in [
            ("commit: a\nb", "commit: a b"),
            ("commit: a\r\nb", "commit: a b"),
            ("commit: a\rb", "commit: a b"),
            ("\ncommit: a", "commit: a"),
            ("commit: a\n", "commit: a"),
            ("commit: a \t  b", "commit: a b"),
            ("   ", ""),
        ] {
            assert_eq!(copy_message(given), want, "{given:?}");
        }
    }

    /// The forged-line shape from the feasibility study: the result holds no
    /// line break, so it cannot start a second reflog entry.
    #[test]
    fn a_forged_second_line_stays_on_one_line() {
        let forged = "commit: a\n0000000000000000000000000000000000000000 \
                      1111111111111111111111111111111111111111 Mallory <m@x> 0 +0000\tcommit: forged";
        let out = copy_message(forged);
        assert!(!out.contains(['\n', '\r', '\t']), "{out:?}");
    }
}
