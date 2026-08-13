//! `--path` filtering: literal paths and simple globs, and nothing else.
//!
//! Shared by every verb that takes `--path` — `status` (B.2), `log` (B.3), and
//! `diff`/`show` when they arrive (B.4/B.5). One implementation, so a filter
//! means the same thing whichever verb an agent hands it to; two copies would
//! drift into two subtly different pathspec dialects, which is the kind of
//! divergence an agent cannot see and cannot debug.
//!
//! The accepted surface is deliberately smaller than git's: a literal path
//! (matching itself and anything beneath it) or a simple glob (`*`, `**`, `?`,
//! `[...]`). Git's pathspec *magic* — `:(exclude)`, `:!`, `:/`, the signature
//! letters — is a loud usage error rather than a silent near-match, because
//! quietly treating `:(exclude)docs` as a literal path named `:(exclude)docs`
//! would answer a question nobody asked.

use gix_object::bstr::BStr;

use crate::error::GitError;

/// A parsed `--path` filter. An empty filter matches everything.
#[derive(Debug)]
pub(crate) struct PathFilter {
    specs: Vec<Spec>,
}

#[derive(Debug)]
enum Spec {
    /// A literal path: matches itself and anything under it.
    Literal(String),
    /// A glob, matched against the whole repo-relative path.
    Glob(gix_glob::Pattern),
}

impl PathFilter {
    /// Parse raw `--path` values, refusing pathspec magic by name.
    pub(crate) fn parse(op: &'static str, paths: &[String]) -> Result<Self, GitError> {
        let mut specs = Vec::new();
        for raw in paths {
            // Git pathspec magic is a loud usage error, never silently matched
            // (B). A leading `:` is the entry point to every magic form.
            if let Some(magic) = pathspec_magic(raw) {
                return Err(GitError::PathspecMagic {
                    operation: op,
                    spec: raw.clone(),
                    magic,
                });
            }
            let normalized = raw.trim_start_matches('/').trim_end_matches('/');
            if normalized.is_empty() {
                continue;
            }
            if normalized.contains(['*', '?', '[']) {
                match gix_glob::Pattern::from_bytes(normalized.as_bytes()) {
                    Some(pattern) => specs.push(Spec::Glob(pattern)),
                    None => {
                        return Err(GitError::Usage {
                            operation: op,
                            message: format!("path '{raw}' is not a usable glob"),
                        })
                    }
                }
            } else {
                specs.push(Spec::Literal(normalized.to_string()));
            }
        }
        Ok(PathFilter { specs })
    }

    /// Whether a repo-relative path passes the filter.
    pub(crate) fn matches(&self, path: &str) -> bool {
        if self.specs.is_empty() {
            return true;
        }
        self.specs.iter().any(|spec| match spec {
            Spec::Literal(lit) => path == lit || path.starts_with(&format!("{lit}/")),
            Spec::Glob(pattern) => {
                let bstr: &BStr = path.into();
                let basename = path.rfind('/').map(|p| p + 1);
                pattern.matches_repo_relative_path(
                    bstr,
                    basename,
                    Some(false),
                    gix_glob::pattern::Case::Sensitive,
                    gix_glob::wildmatch::Mode::empty(),
                )
            }
        })
    }
}

/// Recognize git pathspec magic, returning the offending token if present.
pub(crate) fn pathspec_magic(spec: &str) -> Option<String> {
    if let Some(rest) = spec.strip_prefix(':') {
        // `:(...)` long form, `:!`/`:^` exclude, `:/` from-root, or the
        // short magic signature letters — any leading colon is magic here.
        let token = if rest.starts_with('(') {
            let end = rest.find(')').map(|e| e + 1).unwrap_or(rest.len());
            format!(":{}", &rest[..end])
        } else if let Some(c) = rest.chars().next() {
            format!(":{c}")
        } else {
            ":".to_string()
        };
        return Some(token);
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pathspec_magic_is_recognized() {
        assert_eq!(pathspec_magic(":(exclude)src").as_deref(), Some(":(exclude)"));
        assert_eq!(pathspec_magic(":!src").as_deref(), Some(":!"));
        assert_eq!(pathspec_magic(":/").as_deref(), Some(":/"));
        assert_eq!(pathspec_magic("src/lib.rs"), None);
        assert_eq!(pathspec_magic("*.rs"), None);
    }

    #[test]
    fn literal_path_filter_matches_dir_and_contents() {
        let filter = PathFilter::parse("status", &["src".into()]).expect("parses");
        assert!(filter.matches("src"));
        assert!(filter.matches("src/lib.rs"));
        assert!(!filter.matches("README.md"));
        assert!(!filter.matches("srcx"));
    }

    #[test]
    fn empty_filter_matches_everything() {
        let filter = PathFilter::parse("status", &[]).expect("parses");
        assert!(filter.matches("anything/at/all"));
    }

    /// A glob is matched against the whole repo-relative path, so `*.rs` finds
    /// a file at any depth — the behavior both `status` and `log` rely on.
    #[test]
    fn glob_filter_matches_at_any_depth() {
        let filter = PathFilter::parse("log", &["*.rs".into()]).expect("parses");
        assert!(filter.matches("lib.rs"));
        assert!(filter.matches("src/verbs/log.rs"));
        assert!(!filter.matches("README.md"));
    }

    /// Magic is refused for every verb that shares this filter, with the exit
    /// code that says "your command line", not "your repository".
    #[test]
    fn magic_is_a_usage_error_not_a_literal_path() {
        let err = PathFilter::parse("log", &[":(exclude)docs".into()])
            .expect_err("pathspec magic is refused");
        assert_eq!(err.exit_code(), 2);
    }
}
