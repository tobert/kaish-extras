//! A commit message: assembly from `-m` and `-F`, git's `whitespace` cleanup,
//! and `--trailer` placement (git-write-profile.md §4).
//!
//! `git commit -m` and `-F` never open an editor, so git cleans the message
//! with its `whitespace` mode, not `strip`: trailing whitespace goes, blank
//! lines collapse and the ends are trimmed, and `#` lines stay. Each result
//! here was checked against git 2.56 and `tests/write_commit.rs` holds the
//! oracle comparison.

use crate::error::GitError;
use crate::verbs::write::refusal::{refuse, Override};

/// Join `-m` values the way git does: each is a paragraph of its own.
pub(crate) fn join_paragraphs(parts: &[String]) -> String {
    let mut out = String::new();
    for part in parts {
        if !out.is_empty() {
            out.push('\n');
        }
        out.push_str(part);
        if !out.ends_with('\n') {
            out.push('\n');
        }
    }
    out
}

/// git's `strbuf_stripspace` without comment stripping: trailing whitespace
/// off every line, runs of blank lines collapsed to one, leading and trailing
/// blank lines dropped, and one final `\n` when anything is left.
pub(crate) fn cleanup(raw: &str) -> String {
    let mut out = String::new();
    let mut pending_blank = false;
    for line in raw.split('\n') {
        let line = line.trim_end_matches(|c: char| c.is_ascii_whitespace() || c == '\u{b}');
        if line.is_empty() {
            pending_blank = !out.is_empty();
            continue;
        }
        if pending_blank {
            out.push('\n');
            pending_blank = false;
        }
        out.push_str(line);
        out.push('\n');
    }
    out
}

/// The first line of a message, which is what a reflog entry carries.
pub(crate) fn subject(message: &str) -> &str {
    message.split('\n').next().unwrap_or("")
}

/// One `--trailer` argument: `Key: value` or `Key=value`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Trailer {
    pub(crate) key: String,
    pub(crate) value: String,
}

impl Trailer {
    /// Parse `Key: value` or `Key=value`. Git accepts a bare key and writes
    /// `Key:` with nothing after it; this refuses it, because a trailer with
    /// no value is a typo far more often than an intent.
    pub(crate) fn parse(command: &str, arg: &str) -> Result<Self, GitError> {
        let bad = |why: &str| {
            refuse::<Trailer>(
                2,
                command,
                format!("The trailer '{arg}' {why}."),
                "Give the trailer as 'Key: value', for example: --trailer 'Signed-off-by: Your Name <you@example.com>'.",
                Override::Never,
            )
        };
        let Some(at) = arg.find([':', '=']) else {
            return bad("has no ':' or '=' between a key and a value");
        };
        let key = arg[..at].trim();
        let value = arg[at + 1..].trim();
        if key.is_empty() || !key.chars().all(|c| c.is_ascii_alphanumeric() || c == '-') {
            return bad("has a key that is empty or holds characters other than letters, digits and '-'");
        }
        if value.is_empty() {
            return bad("has no value after the key");
        }
        if value.contains(['\n', '\r']) {
            return bad("has a value that spans lines");
        }
        Ok(Trailer { key: key.to_string(), value: value.to_string() })
    }
}

/// Whether `line` is `Token<spaces>:` as `git interpret-trailers` reads a
/// message line: a token of letters, digits and `-`, optional blanks, `:`.
fn trailer_parts(line: &str) -> Option<(&str, &str)> {
    let colon = line.find(':')?;
    let token = line[..colon].trim_end_matches([' ', '\t']);
    if token.is_empty() || !token.chars().all(|c| c.is_ascii_alphanumeric() || c == '-') {
        return None;
    }
    Some((token, line[colon + 1..].trim()))
}

/// Add `trailers` to a cleaned message, as `git commit --trailer` does: into
/// the message's final paragraph when that paragraph is a trailer block, else
/// in a new paragraph. A trailer equal to the one just before it is skipped
/// (git's default `ifexists=addIfDifferentNeighbor`).
pub(crate) fn add_trailers(message: &str, trailers: &[Trailer]) -> String {
    if trailers.is_empty() {
        return message.to_string();
    }
    let mut lines: Vec<String> = message.lines().map(str::to_string).collect();
    let in_block = last_paragraph_is_trailers(&lines);
    if !in_block {
        lines.push(String::new());
    }
    for t in trailers {
        let same_as_last = lines
            .iter()
            .rev()
            .find(|l| !l.starts_with([' ', '\t']))
            .and_then(|l| trailer_parts(l))
            .is_some_and(|(k, v)| k.eq_ignore_ascii_case(&t.key) && v == t.value);
        if same_as_last {
            continue;
        }
        lines.push(format!("{}: {}", t.key, t.value));
    }
    let mut out = lines.join("\n");
    out.push('\n');
    out
}

/// Git's `find_trailer_block_start` rule over the last paragraph. The first
/// paragraph is the subject and never a trailer block.
fn last_paragraph_is_trailers(lines: &[String]) -> bool {
    let end = lines.len();
    if end == 0 {
        return false;
    }
    let mut start = end;
    while start > 0 && !lines[start - 1].is_empty() {
        start -= 1;
    }
    if start == 0 {
        return false;
    }
    let mut trailer = 0usize;
    let mut other = 0usize;
    let mut recognized = false;
    for line in &lines[start..end] {
        if line.starts_with("(cherry picked from commit ") {
            recognized = true;
            trailer += 1;
        } else if line.starts_with([' ', '\t']) {
            // A continuation belongs to the trailer above it.
            if trailer == 0 {
                other += 1;
            }
        } else if let Some((token, _)) = trailer_parts(line) {
            if token.eq_ignore_ascii_case("Signed-off-by") {
                recognized = true;
            }
            trailer += 1;
        } else {
            other += 1;
        }
    }
    (trailer > 0 && other == 0) || (recognized && trailer * 3 >= other)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cleanup_follows_gits_whitespace_mode() {
        assert_eq!(cleanup("  \n\nsubject   \n\n\n\nbody  \n# keep\n\n\n"), "subject\n\nbody\n# keep\n");
        assert_eq!(cleanup(""), "");
        assert_eq!(cleanup(" \n \n"), "");
        // A leading space on a text line stays: only trailing space goes.
        assert_eq!(cleanup(" indented\n"), " indented\n");
        assert_eq!(cleanup("a\r\nb\r\n"), "a\nb\n");
    }

    #[test]
    fn paragraphs_join_with_one_blank_line() {
        let parts = vec!["one".to_string(), "two\n".to_string(), "three".to_string()];
        assert_eq!(join_paragraphs(&parts), "one\n\ntwo\n\nthree\n");
    }

    #[test]
    fn a_trailer_needs_a_key_a_separator_and_a_value() {
        assert!(Trailer::parse("git commit", "Fixes: 12").is_ok());
        assert_eq!(Trailer::parse("git commit", "fixes=2").expect("ok").key, "fixes");
        for bad in ["bad trailer", "Fixes", "Fixes:", ": v", "a b: v"] {
            let e = Trailer::parse("git commit", bad).expect_err(bad);
            assert_eq!(e.exit_code(), 2, "{bad}");
        }
    }

    #[test]
    fn trailers_extend_a_trailer_block_or_start_one() {
        let t = |k: &str, v: &str| Trailer { key: k.into(), value: v.into() };
        assert_eq!(add_trailers("subj\n", &[t("Fixes", "1")]), "subj\n\nFixes: 1\n");
        assert_eq!(
            add_trailers("subj\n\nCo-authored-by: Z <z@z>\n", &[t("Fixes", "1")]),
            "subj\n\nCo-authored-by: Z <z@z>\nFixes: 1\n"
        );
        // A one-paragraph message is a subject, even when it reads like a trailer.
        assert_eq!(add_trailers("Fixes: 1\n", &[t("Fixes", "2")]), "Fixes: 1\n\nFixes: 2\n");
        // Equal neighbors collapse; a different one between keeps both.
        assert_eq!(add_trailers("s\n", &[t("A", "1"), t("A", "1")]), "s\n\nA: 1\n");
        assert_eq!(add_trailers("s\n", &[t("A", "1"), t("B", "1"), t("A", "1")]), "s\n\nA: 1\nB: 1\nA: 1\n");
    }
}
