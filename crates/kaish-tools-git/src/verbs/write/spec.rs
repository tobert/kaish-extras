//! Pathspecs for the write verbs, and the result type they share.
//!
//! One [`PathFilter`] per spec so a spec that matched nothing can be named:
//! git refuses `add nomatch` with "pathspec 'nomatch' did not match any
//! files", and an agent that typed a wrong name should be told which one.

use crate::error::GitError;
use crate::pathfilter::PathFilter;

/// What a write verb returns: the text git would print (often none) and a
/// typed summary for `--json`.
pub(crate) struct Outcome {
    pub(crate) text: String,
    /// Notes for stderr that go with a success: a hook's output, a detached
    /// HEAD. Empty for most verbs.
    pub(crate) err: String,
    pub(crate) json: serde_json::Value,
}

pub(crate) struct SpecSet {
    items: Vec<Item>,
}

struct Item {
    text: String,
    filter: PathFilter,
    glob: bool,
    matched: bool,
}

pub(crate) fn is_glob(spec: &str) -> bool {
    spec.contains(['*', '?', '['])
}

impl SpecSet {
    /// Parse repo-relative specs; pathspec magic is refused by name.
    pub(crate) fn new(op: &'static str, specs: &[String]) -> Result<Self, GitError> {
        let mut items = Vec::new();
        for s in specs {
            items.push(Item {
                text: s.clone(),
                filter: PathFilter::parse(op, std::slice::from_ref(s))?,
                glob: is_glob(s),
                matched: false,
            });
        }
        Ok(Self { items })
    }

    /// No spec was given: every path is wanted.
    pub(crate) fn is_empty(&self) -> bool {
        self.items.is_empty()
    }

    pub(crate) fn any_glob(&self) -> bool {
        self.items.iter().any(|i| i.glob)
    }

    /// The specs `path` matches, without recording a match.
    pub(crate) fn hits(&self, path: &[u8]) -> Vec<usize> {
        self.items
            .iter()
            .enumerate()
            .filter(|(_, i)| i.filter.matches_bytes(path))
            .map(|(n, _)| n)
            .collect()
    }

    pub(crate) fn mark(&mut self, hits: &[usize]) {
        for h in hits {
            self.items[*h].matched = true;
        }
    }

    /// Whether `path` is wanted; records the match.
    pub(crate) fn matches(&mut self, path: &[u8]) -> bool {
        if self.items.is_empty() {
            return true;
        }
        let hits = self.hits(path);
        self.mark(&hits);
        !hits.is_empty()
    }

    /// Whether a spec names `path` exactly (not a glob, not a parent).
    pub(crate) fn names_exactly(&self, path: &[u8]) -> bool {
        self.items.iter().any(|i| !i.glob && i.text.as_bytes() == path)
    }

    /// Whether a directory could hold a match: a spec is the directory, above
    /// it, or below it, or any spec is a glob.
    pub(crate) fn relevant_dir(&self, dir: &[u8]) -> bool {
        let below = |outer: &[u8], inner: &[u8]| inner.len() > outer.len() && inner.starts_with(outer) && inner[outer.len()] == b'/';
        self.items.is_empty()
            || self.items.iter().any(|i| {
                let text = i.text.as_bytes();
                i.glob || text.is_empty() || text == dir || dir.is_empty() || below(dir, text) || below(text, dir)
            })
    }

    pub(crate) fn literals(&self) -> Vec<String> {
        self.items.iter().filter(|i| !i.glob && !i.text.is_empty()).map(|i| i.text.clone()).collect()
    }

    pub(crate) fn mark_literal(&mut self, path: &[u8]) {
        for i in &mut self.items {
            if !i.glob && i.text.as_bytes() == path {
                i.matched = true;
            }
        }
    }

    pub(crate) fn is_marked(&self, text: &str) -> bool {
        self.items.iter().any(|i| i.text == text && i.matched)
    }

    /// The specs that matched nothing.
    pub(crate) fn unmatched(&self) -> Vec<&str> {
        self.items.iter().filter(|i| !i.matched && !i.text.is_empty()).map(|i| i.text.as_str()).collect()
    }
}
