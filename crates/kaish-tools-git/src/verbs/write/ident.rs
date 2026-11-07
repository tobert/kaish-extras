//! Who a commit is by (git-write-profile.md §5.4).
//!
//! The embedder may pin the author and committer on `WritableGitConfig`. When
//! it did not, the kernel variables `GIT_AUTHOR_NAME`, `GIT_AUTHOR_EMAIL`,
//! `GIT_COMMITTER_NAME`, `GIT_COMMITTER_EMAIL`, `GIT_AUTHOR_DATE` and
//! `GIT_COMMITTER_DATE` are read, and nothing else: `user.name` and
//! `user.email` in a config file are never consulted, so a commit cannot take
//! its identity from a file this shell did not look at.
//!
//! A time has no zone in the kernel's variables, and this crate reads no
//! system zone database (hermetic), so a date written without an offset gets
//! `+0000`, and so does the clock. Git uses the local zone there.

use std::time::{SystemTime, UNIX_EPOCH};

use kaish_tool_api::ToolCtx;
use kaish_types::Value;

use crate::config::Identity;
use crate::error::GitError;
use crate::verbs::write::refusal::{refuse, Override};

/// One identity line: who, and when.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Signature {
    pub(crate) name: String,
    pub(crate) email: String,
    pub(crate) seconds: i64,
    /// The zone, in seconds east of UTC.
    pub(crate) offset: i32,
}

impl Signature {
    /// `secs +hhmm`, git's internal time form.
    pub(crate) fn time_text(&self) -> String {
        let sign = if self.offset < 0 { '-' } else { '+' };
        let abs = self.offset.unsigned_abs();
        format!("{} {sign}{:02}{:02}", self.seconds, abs / 3600, abs % 3600 / 60)
    }

    /// `Name <email> secs +hhmm`, the form `git var GIT_AUTHOR_IDENT` prints.
    pub(crate) fn ident_line(&self) -> String {
        format!("{} <{}> {}", self.name, self.email, self.time_text())
    }

    pub(crate) fn to_actor(&self) -> gix_actor::Signature {
        gix_actor::Signature {
            name: self.name.as_str().into(),
            email: self.email.as_str().into(),
            time: gix_date::Time::new(self.seconds, self.offset),
        }
    }
}

/// The variables identity reads, collected from the kernel before the git
/// work starts (a `ToolCtx` is not available inside it).
#[derive(Debug, Clone, Default)]
pub(crate) struct KernelVars {
    pub(crate) author_name: Option<String>,
    pub(crate) author_email: Option<String>,
    pub(crate) committer_name: Option<String>,
    pub(crate) committer_email: Option<String>,
    pub(crate) author_date: Option<String>,
    pub(crate) committer_date: Option<String>,
}

impl KernelVars {
    pub(crate) fn read(command: &str, ctx: &dyn ToolCtx) -> Result<Self, GitError> {
        let get = |name: &str| -> Result<Option<String>, GitError> {
            let text = match ctx.var(name) {
                None | Some(Value::Null) => return Ok(None),
                Some(Value::String(s)) => s,
                Some(Value::Int(i)) => i.to_string(),
                Some(_) => {
                    return refuse(
                        2,
                        command,
                        format!("The variable {name} is not text."),
                        format!("Set it to text, for example: export {name}='...'."),
                        Override::Never,
                    )
                }
            };
            // An empty variable is an unset one: git does the same for the dates.
            Ok(if text.trim().is_empty() { None } else { Some(text) })
        };
        Ok(Self {
            author_name: get("GIT_AUTHOR_NAME")?,
            author_email: get("GIT_AUTHOR_EMAIL")?,
            committer_name: get("GIT_COMMITTER_NAME")?,
            committer_email: get("GIT_COMMITTER_EMAIL")?,
            author_date: get("GIT_AUTHOR_DATE")?,
            committer_date: get("GIT_COMMITTER_DATE")?,
        })
    }
}

/// The wall clock as `(seconds, 0)`: UTC.
pub(crate) fn now() -> i64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |d| d.as_secs() as i64)
}

/// The two signatures a commit carries.
#[derive(Debug, Clone)]
pub(crate) struct Idents {
    pub(crate) author: Signature,
    pub(crate) committer: Signature,
}

/// Resolve author and committer for `command`.
///
/// `author_flag` is `--author`, which replaces the author's name and email on
/// either path. With a pinned identity nothing is read from the kernel, dates
/// included: the clock supplies them.
pub(crate) fn resolve(
    command: &str,
    pinned: Option<&(Identity, Identity)>,
    vars: &KernelVars,
    author_flag: Option<(String, String)>,
    now: i64,
) -> Result<Idents, GitError> {
    if let Some((author, committer)) = pinned {
        // The pinned identities were checked when they were built; `--author`
        // is the caller's text and is checked here, as on the unpinned path.
        let (name, email) = match author_flag {
            Some((n, e)) => check_identity(command, n, e)?,
            None => (author.name().to_string(), author.email().to_string()),
        };
        return Ok(Idents {
            author: Signature { name, email, seconds: now, offset: 0 },
            committer: Signature {
                name: committer.name().to_string(),
                email: committer.email().to_string(),
                seconds: now,
                offset: 0,
            },
        });
    }

    let missing = |which: &str, name: &str, email: &str| {
        refuse::<Idents>(
            2,
            command,
            format!("No {which} identity is set: the variables {name} and {email} are not both set in this shell, and the program embedding this shell has not pinned an identity."),
            format!("Set both, for example: export {name}='Your Name' {email}='you@example.com'."),
            Override::Variables,
        )
    };
    let (a_name, a_email) = match author_flag {
        Some(pair) => pair,
        None => match (&vars.author_name, &vars.author_email) {
            (Some(n), Some(e)) => (n.clone(), e.clone()),
            _ => return missing("author", "GIT_AUTHOR_NAME", "GIT_AUTHOR_EMAIL"),
        },
    };
    // The committer falls back field by field to the author's variables. With
    // `--author` and no author variables there is nobody to fall back to.
    let c_name = vars.committer_name.clone().or_else(|| vars.author_name.clone());
    let c_email = vars.committer_email.clone().or_else(|| vars.author_email.clone());
    let (Some(c_name), Some(c_email)) = (c_name, c_email) else {
        return missing("committer", "GIT_COMMITTER_NAME", "GIT_COMMITTER_EMAIL");
    };

    let when = |var: &str, text: &Option<String>| -> Result<(i64, i32), GitError> {
        match text {
            None => Ok((now, 0)),
            Some(t) => parse_date(t).ok_or_else(|| {
                crate::verbs::write::refusal::Refusal::new(
                    2,
                    command,
                    format!("{var} is '{t}', which is not a date this shell reads."),
                    "Write it as ISO 8601, for example 2026-08-01T10:00:00+02:00, or as git's internal form: 1785578400 +0200.",
                    Override::Variables,
                )
                .into_error()
            }),
        }
    };
    let (a_secs, a_off) = when("GIT_AUTHOR_DATE", &vars.author_date)?;
    // Git's committer date is the clock unless GIT_COMMITTER_DATE is set; it
    // does not borrow the author's.
    let (c_secs, c_off) = when("GIT_COMMITTER_DATE", &vars.committer_date)?;
    let (a_name, a_email) = check_identity(command, a_name, a_email)?;
    let (c_name, c_email) = check_identity(command, c_name, c_email)?;
    Ok(Idents {
        author: Signature { name: a_name, email: a_email, seconds: a_secs, offset: a_off },
        committer: Signature { name: c_name, email: c_email, seconds: c_secs, offset: c_off },
    })
}

/// The committer alone, for a command that makes no author line: `reset`
/// (the reflog entry) and `commit --amend` (the author is the old commit's).
pub(crate) fn resolve_committer(
    command: &str,
    pinned: Option<&(Identity, Identity)>,
    vars: &KernelVars,
    now: i64,
) -> Result<Signature, GitError> {
    if pinned.is_none() {
        let name = vars.committer_name.is_some() || vars.author_name.is_some();
        let email = vars.committer_email.is_some() || vars.author_email.is_some();
        if !(name && email) {
            return refuse(
                2,
                command,
                "No committer identity is set: the variables GIT_COMMITTER_NAME and GIT_COMMITTER_EMAIL are not both set in this shell, and the program embedding this shell has not pinned an identity.",
                "Set both, for example: export GIT_COMMITTER_NAME='Your Name' GIT_COMMITTER_EMAIL='you@example.com'.",
                Override::Variables,
            );
        }
    }
    // `resolve` wants an author too. A stand-in fills the slot only when the
    // author variables are unset, and then the committer variables are set
    // (checked above), so the stand-in is never the committer.
    let mut v = vars.clone();
    // The author's date is not read either: a bad GIT_AUTHOR_DATE cannot
    // refuse a command that makes no author line.
    v.author_date = None;
    if v.author_name.is_none() || v.author_email.is_none() {
        v.author_name = Some("author".to_string());
        v.author_email = Some("author@example.invalid".to_string());
    }
    Ok(resolve(command, pinned, &v, None, now)?.committer)
}

/// The identities of an amended commit: the author is the old commit's
/// (name, email and date) unless `--author` replaces name and email, or
/// `--reset-author` makes the author as new as a fresh commit's. The committer
/// is new either way.
pub(crate) fn resolve_amend(
    command: &str,
    pinned: Option<&(Identity, Identity)>,
    vars: &KernelVars,
    author_flag: Option<(String, String)>,
    now: i64,
    kept: &Signature,
    reset_author: bool,
) -> Result<Idents, GitError> {
    if reset_author {
        return resolve(command, pinned, vars, author_flag, now);
    }
    let committer = resolve_committer(command, pinned, vars, now)?;
    // `--author` is the caller's text: checked before any object is written,
    // as `resolve` checks it.
    let (name, email) = match author_flag {
        Some((n, e)) => check_identity(command, n, e)?,
        None => (kept.name.clone(), kept.email.clone()),
    };
    Ok(Idents { author: Signature { name, email, seconds: kept.seconds, offset: kept.offset }, committer })
}

/// Refuse a name or email that git would strip or that would break a header
/// line: git removes `<`, `>` and line breaks from an identity, so writing the
/// text as given would store a different identity from the one asked for.
fn check_identity(command: &str, name: String, email: String) -> Result<(String, String), GitError> {
    match Identity::new(name.trim(), email.trim()) {
        Ok(i) => Ok((i.name().to_string(), i.email().to_string())),
        Err(_) => refuse(
            2,
            command,
            format!("The identity '{}' <{}> cannot be written: a name must not be empty, and neither field may hold '<', '>' or a line break.", name.trim().escape_debug(), email.trim().escape_debug()),
            "Set both to plain text, for example: export GIT_AUTHOR_NAME='Your Name' GIT_AUTHOR_EMAIL='you@example.com', or pass --author 'Your Name <you@example.com>'.",
            Override::Variables,
        ),
    }
}

/// `--author 'Name <email>'`. Git also accepts a pattern that it looks up
/// among existing authors; this accepts the explicit form only.
pub(crate) fn parse_author_flag(command: &str, text: &str) -> Result<(String, String), GitError> {
    let bad = || {
        refuse::<(String, String)>(
            2,
            command,
            format!("--author '{text}' is not in the form 'Name <email>'."),
            "Give both, for example: --author 'Your Name <you@example.com>'.",
            Override::Never,
        )
    };
    let Some((name, rest)) = text.split_once('<') else { return bad() };
    let Some(email) = rest.strip_suffix('>') else { return bad() };
    if name.trim().is_empty() || email.contains(['<', '>']) || email.trim().is_empty() {
        return bad();
    }
    Ok((name.trim().to_string(), email.trim().to_string()))
}

/// A date in a form git reads and this crate can read without a zone
/// database: git's internal `<seconds> <+hhmm>` (a leading `@` is allowed),
/// and ISO 8601 / RFC 3339 `YYYY-MM-DD[T ]HH:MM:SS[.fraction][ ][Z|±hh[:mm]]`.
/// A date with no offset is UTC.
pub(crate) fn parse_date(text: &str) -> Option<(i64, i32)> {
    let s = text.trim();
    let raw = s.strip_prefix('@').unwrap_or(s);
    let mut words = raw.split_whitespace();
    if let Some(first) = words.next() {
        let digits = first.bytes().all(|b| b.is_ascii_digit());
        if digits && (s.starts_with('@') || (9..=11).contains(&first.len())) {
            let secs: i64 = first.parse().ok()?;
            let off = match words.next() {
                None => 0,
                Some(z) => parse_offset(z)?,
            };
            return words.next().is_none().then_some((secs, off));
        }
    }
    parse_iso(s)
}

fn parse_offset(z: &str) -> Option<i32> {
    if z == "Z" {
        return Some(0);
    }
    let (sign, rest) = match z.as_bytes().first()? {
        b'+' => (1, &z[1..]),
        b'-' => (-1, &z[1..]),
        _ => return None,
    };
    let digits: String = rest.chars().filter(|c| *c != ':').collect();
    if !digits.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    let (h, m) = match digits.len() {
        2 => (digits.parse::<i32>().ok()?, 0),
        4 => (digits[..2].parse::<i32>().ok()?, digits[2..].parse::<i32>().ok()?),
        _ => return None,
    };
    if h > 23 || m > 59 {
        return None;
    }
    Some(sign * (h * 3600 + m * 60))
}

fn parse_iso(s: &str) -> Option<(i64, i32)> {
    let b = s.as_bytes();
    let num = |from: usize, len: usize| -> Option<i32> {
        let part = s.get(from..from + len)?;
        part.bytes().all(|c| c.is_ascii_digit()).then(|| part.parse().ok())?
    };
    if b.len() < 19 || b[4] != b'-' || b[7] != b'-' || !(b[10] == b'T' || b[10] == b' ') || b[13] != b':' || b[16] != b':' {
        return None;
    }
    let (y, mo, d) = (num(0, 4)?, num(5, 2)?, num(8, 2)?);
    let (h, mi, sec) = (num(11, 2)?, num(14, 2)?, num(17, 2)?);
    let mut rest = &s[19..];
    if let Some(frac) = rest.strip_prefix('.') {
        let n = frac.bytes().take_while(u8::is_ascii_digit).count();
        if n == 0 {
            return None;
        }
        rest = &frac[n..];
    }
    let rest = rest.trim_start();
    let off = if rest.is_empty() { 0 } else { parse_offset(rest)? };
    let date = jiff::civil::DateTime::new(i16::try_from(y).ok()?, mo as i8, d as i8, h as i8, mi as i8, sec as i8, 0).ok()?;
    let ts = jiff::tz::Offset::from_seconds(off).ok()?.to_timestamp(date).ok()?;
    Some((ts.as_second(), off))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dates_read_in_the_forms_git_reads() {
        for (text, want) in [
            ("2026-08-01T10:00:00+00:00", (1785578400, 0)),
            ("2026-08-01T10:00:00Z", (1785578400, 0)),
            ("2026-08-01 10:00:00 +0200", (1785571200, 7200)),
            ("2026-08-01T10:00:00.123+01:00", (1785574800, 3600)),
            ("2026-08-01T10:00:00", (1785578400, 0)),
            ("@1785578400", (1785578400, 0)),
            ("1785578400 +0530", (1785578400, 19800)),
            ("@1785578400 -0400", (1785578400, -14400)),
            ("1785578400", (1785578400, 0)),
        ] {
            assert_eq!(parse_date(text), Some(want), "{text}");
        }
        for bad in ["", "garbage", "2026-08-01", "2026-13-01T10:00:00Z", "1785578400 +9999", "2026-08-01T10:00:00+25:00", "1785578400 +0200 x"] {
            assert_eq!(parse_date(bad), None, "{bad}");
        }
    }

    #[test]
    fn a_signature_prints_as_git_var_does() {
        let s = Signature { name: "A B".into(), email: "a@x.y".into(), seconds: 1785578400, offset: -19800 };
        assert_eq!(s.ident_line(), "A B <a@x.y> 1785578400 -0530");
    }

    #[test]
    fn the_author_flag_wants_name_and_email() {
        assert_eq!(parse_author_flag("c", "A B <a@b.c>").expect("ok"), ("A B".to_string(), "a@b.c".to_string()));
        for bad in ["A B", "<a@b.c>", "A <>", "A <a@b.c", "A <a<b>@c>"] {
            assert!(parse_author_flag("c", bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn nothing_set_refuses_with_exit_2_naming_the_variables() {
        let err = resolve("git commit -m x", None, &KernelVars::default(), None, 0).expect_err("refuses");
        assert_eq!(err.exit_code(), 2);
        let text = err.to_string();
        assert!(text.contains("GIT_AUTHOR_NAME") && text.contains("GIT_AUTHOR_EMAIL"), "{text}");
        assert!(text.contains("pinned an identity"), "{text}");
    }
}
