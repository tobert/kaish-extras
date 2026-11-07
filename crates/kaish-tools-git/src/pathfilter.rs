//! `--path` filtering: literal paths and simple globs, and nothing else.
//!
//! Shared by every verb that takes `--path` — `status` (B.2), `log` (B.3), and
//! `diff`/`show` when they arrive (B.4/B.5). One implementation, so a filter
//! means the same thing whichever verb an agent hands it to; two copies would
//! drift into two subtly different pathspec dialects, which is the kind of
//! divergence an agent cannot see and cannot debug.
//!
//! The accepted surface is deliberately smaller than git's: a literal path
//! (matching itself and anything beneath it) or a simple glob (`*`, `?`,
//! `[...]`, and `\\` as the escape). A glob follows git's non-magic pathspec
//! (dir.c `match_pathspec_item`): it is wildmatched against the WHOLE
//! repo-relative path, anchored at the start, with no `WM_PATHNAME`, so `*`
//! and `?` cross `/` and `**` is two stars. `ca*` matches `ca.txt` and
//! `cab/x` and not `d/ca.txt`; `*.txt` matches `d/ca.txt`. This is not
//! gitignore's rule (a pattern without a slash matches the basename), which
//! is what this module used before. Git's pathspec *magic* — `:(exclude)`, `:!`, `:/`, the signature
//! letters — is a loud usage error rather than a silent near-match, because
//! quietly treating `:(exclude)docs` as a literal path named `:(exclude)docs`
//! would answer a question nobody asked.

use crate::error::GitError;

/// A parsed `--path` filter. An empty filter matches everything.
#[derive(Debug)]
pub(crate) struct PathFilter {
    specs: Vec<Spec>,
}

#[derive(Debug)]
enum Spec {
    /// A literal path: matches itself and anything under it.
    Literal(Vec<u8>),
    /// A path with glob characters. Git tries the text as a literal first
    /// (a file named `a[1].txt` matches the spec `a[1].txt`), then as a
    /// wildmatch on the whole path.
    Glob(Vec<u8>),
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
            if normalized.contains(['*', '?', '[', '\\']) {
                specs.push(Spec::Glob(normalized.as_bytes().to_vec()));
            } else {
                specs.push(Spec::Literal(normalized.as_bytes().to_vec()));
            }
        }
        Ok(PathFilter { specs })
    }

    /// Whether a repo-relative path passes the filter.
    pub(crate) fn matches(&self, path: &str) -> bool {
        self.matches_bytes(path.as_bytes())
    }

    /// [`matches`](Self::matches) for a path that is bytes: an index or tree
    /// path need not be UTF-8, and must match a glob such as `*` all the same.
    pub(crate) fn matches_bytes(&self, path: &[u8]) -> bool {
        if self.specs.is_empty() {
            return true;
        }
        self.specs.iter().any(|spec| match spec {
            Spec::Literal(lit) => literal_matches(lit, path),
            Spec::Glob(pattern) => {
                literal_matches(pattern, path)
                    || gix_glob::wildmatch(pattern.as_slice().into(), path.into(), gix_glob::wildmatch::Mode::empty())
            }
        })
    }
}

/// The path is `lit` or lies under it.
fn literal_matches(lit: &[u8], path: &[u8]) -> bool {
    path == lit || (path.len() > lit.len() && path.starts_with(lit) && path[lit.len()] == b'/')
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

    /// The table measured against git 2.56 (`git ls-files -- '<spec>'` over
    /// these paths): a glob is wildmatched against the whole path, anchored at
    /// the start, and `*` crosses `/`.
    #[test]
    fn a_glob_matches_the_whole_path_as_git_does() {
        let paths = ["ca.txt", "cb.txt", "cab/x", "d/ca.txt", "d/e/ca.txt", "a1.txt", "a[1].txt", "d/other.md"];
        for (spec, want) in [
            ("*.txt", vec!["a1.txt", "a[1].txt", "ca.txt", "cb.txt", "d/ca.txt", "d/e/ca.txt"]),
            ("ca*", vec!["ca.txt", "cab/x"]),
            ("d/*", vec!["d/ca.txt", "d/e/ca.txt", "d/other.md"]),
            ("d", vec!["d/ca.txt", "d/e/ca.txt", "d/other.md"]),
            ("?a.txt", vec!["ca.txt"]),
            ("**/ca.txt", vec!["d/ca.txt", "d/e/ca.txt"]),
            ("*/ca.txt", vec!["d/ca.txt", "d/e/ca.txt"]),
            ("a[1].txt", vec!["a1.txt", "a[1].txt"]),
            ("a\\[1\\].txt", vec!["a[1].txt"]),
            ("c*b", vec![]),
        ] {
            let filter = PathFilter::parse("log", &[spec.into()]).expect("parses");
            let mut got: Vec<&str> = paths.iter().copied().filter(|p| filter.matches(p)).collect();
            got.sort();
            assert_eq!(got, want, "spec {spec:?}");
        }
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
