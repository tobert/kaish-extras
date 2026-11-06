//! The ignore rules `add` consults: `.gitignore` files as `gix-worktree`
//! reads them while descending, plus `info/exclude`. The same construction as
//! `git status` uses (`verbs::status`), so the two agree on what is ignored.

use std::path::Path;

use gix_object::bstr::BStr;

use crate::error::GitError;
use crate::repo::ReadRepo;

pub(crate) struct Ignore {
    stack: gix_worktree::Stack,
}

impl Ignore {
    pub(crate) fn new(
        op: &'static str,
        repo: &ReadRepo,
        work_dir: &Path,
        index: &gix_index::State,
    ) -> Result<Self, GitError> {
        let parse = gix_ignore::search::Ignore::default();
        let mut globals = gix_ignore::Search::default();
        let exclude = repo.read_info_exclude()?;
        if !exclude.is_empty() {
            globals.add_patterns_buffer(&exclude, repo.common_dir().join("info").join("exclude"), None, parse);
        }
        let ignore = gix_worktree::stack::state::Ignore::new(
            gix_ignore::Search::default(),
            globals,
            None,
            gix_worktree::stack::state::ignore::Source::WorktreeThenIdMappingIfNotSkipped,
            parse,
        );
        let state = gix_worktree::stack::State::IgnoreStack(ignore);
        let stack = gix_worktree::Stack::from_state_and_ignore_case(
            work_dir,
            false,
            state,
            index,
            index.path_backing(),
        );
        let _ = op;
        Ok(Self { stack })
    }

    /// Whether the path itself matches an ignore rule.
    pub(crate) fn matches(
        &mut self,
        op: &'static str,
        repo: &ReadRepo,
        work_dir: &Path,
        rel: &str,
        is_dir: bool,
    ) -> Result<bool, GitError> {
        let mode = is_dir.then_some(gix_index::entry::Mode::DIR);
        let bstr: &BStr = rel.into();
        let platform = self
            .stack
            .at_entry(bstr, mode, &repo.objects())
            .map_err(|e| GitError::repository(op, "consulting .gitignore", work_dir, e))?;
        Ok(platform.excluded_kind().is_some())
    }

    /// Whether the path or any directory above it is ignored: a file inside
    /// an ignored directory is ignored.
    pub(crate) fn ignored_with_parents(
        &mut self,
        op: &'static str,
        repo: &ReadRepo,
        work_dir: &Path,
        rel: &str,
        is_dir: bool,
    ) -> Result<bool, GitError> {
        let mut acc = String::new();
        let parts: Vec<&str> = rel.split('/').collect();
        for part in &parts[..parts.len() - 1] {
            if !acc.is_empty() {
                acc.push('/');
            }
            acc.push_str(part);
            if self.matches(op, repo, work_dir, &acc, true)? {
                return Ok(true);
            }
        }
        self.matches(op, repo, work_dir, rel, is_dir)
    }
}
