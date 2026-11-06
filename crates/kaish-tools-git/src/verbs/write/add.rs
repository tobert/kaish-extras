//! `git add` (git-write-profile.md §4, §5.3, §6).
//!
//! Accepts `<paths>`, `.`, `-A`/`--all`, `-u`/`--update` and `-f`/`--force`.
//! Refuses `-p` and `-i` (no terminal), a plain `add` of an ignored path
//! (with git's hint naming `-f`), a nested repository that is not already a
//! gitlink, and any file real git would store differently from its bytes.
//!
//! Where it differs from git, on purpose: git adds the files that are not
//! ignored and exits 1 for the ignored ones; this refuses the whole command so
//! nothing is half-staged (docs/issues.md, "add atomicity").

use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;

use clap::Parser;
use gix_index::entry::{Flags, Mode, Stat};
use gix_index::hash::ObjectId;
use gix_object::bstr::ByteSlice;
use kaish_tool_api::GlobalFlags;

use crate::error::GitError;
use crate::repo::ReadRepo;
use crate::verbs::write::attributes::AttributeReader;
use crate::verbs::write::ignore::Ignore;
use crate::verbs::write::indexfile::{self, IndexTx, Snapshot, Upsert};
use crate::verbs::write::objects::BlobWriter;
use crate::verbs::write::refusal::{refuse, Override};
use crate::verbs::write::repo::WriteRepo;
use crate::verbs::write::spec::{Outcome, SpecSet};
use crate::verbs::write::stage::{self, Staged};
use crate::verbs::write::state::{ensure_writable, StagePolicy};

const OP: &str = "add";

/// `git add`'s argv surface.
#[derive(Parser, Debug)]
#[command(
    name = "add",
    about = "Stage working-tree content: new files, changed files and deletions. Stores each file's bytes as they are, and refuses a file that real git would convert (filters, line endings, $Id)"
)]
pub(crate) struct AddArgs {
    /// Stage every change in the whole working tree: new files, changed files
    /// and deleted files. With no paths, `-A` means the whole repository.
    #[arg(short = 'A', long = "all")]
    pub all: bool,

    /// Stage changes to tracked files only: changed and deleted files, no new
    /// files. With no paths, `-u` means the whole repository.
    #[arg(short = 'u', long = "update")]
    pub update: bool,

    /// Stage a file even though an ignore rule matches it. Without `-f`, naming
    /// an ignored path refuses the command with exit 1.
    #[arg(short = 'f', long = "force")]
    pub force: bool,

    /// Refused: choosing hunks needs a terminal this shell does not have.
    /// Stage whole files with `git add <path>`.
    #[arg(short = 'p', long = "patch")]
    pub patch: bool,

    /// Refused: the interactive menu needs a terminal this shell does not
    /// have. Stage whole files with `git add <path>`.
    #[arg(short = 'i', long = "interactive")]
    pub interactive: bool,

    /// Repository to change. Defaults to the current directory; discovery
    /// searches upward, never past the mount root that contains the path.
    #[arg(long = "repo", value_name = "PATH")]
    pub repo: Option<String>,

    #[command(flatten)]
    pub global: GlobalFlags,

    /// Paths to stage, relative to the current directory: files, directories
    /// (everything below, except ignored files) or `*`/`?`/`[...]` patterns.
    /// `.` is the current directory.
    #[arg(hide = true)]
    pub operands: Vec<String>,
}

pub(crate) struct AddOptions {
    /// Repo-relative; `""` is the repository root.
    pub specs: Vec<String>,
    pub all: bool,
    pub update: bool,
    pub force: bool,
    pub command: String,
}

#[derive(Default)]
pub(crate) struct Plan {
    pub(crate) files: Vec<Staged>,
    pub(crate) removals: Vec<String>,
    pub(crate) gitlinks: Vec<(String, ObjectId, Stat)>,
    pub(crate) added: Vec<String>,
    pub(crate) modified: Vec<String>,
    pub(crate) refreshed: usize,
}

impl Plan {
    fn is_noop(&self) -> bool {
        self.files.is_empty() && self.removals.is_empty() && self.gitlinks.is_empty()
    }
}

pub(crate) fn run(repo: &WriteRepo, opts: &AddOptions) -> Result<Outcome, GitError> {
    let policy = ensure_writable(repo, &opts.command)?;
    if opts.specs.is_empty() && !opts.all && !opts.update {
        return Ok(Outcome {
            text: "Nothing specified, nothing added.\n".to_string(),
            err: String::new(),
            json: serde_json::json!({ "added": [], "modified": [], "removed": [], "refreshed": 0 }),
        });
    }
    let snap = Snapshot::read(repo, &opts.command)?;
    let mut plan = build(repo, &snap.state, snap.mtime, &policy, opts)?;
    if plan.is_noop() {
        return Ok(report(&plan));
    }
    let mut tx = IndexTx::lock(repo, &opts.command)?;
    if tx.snap.bytes != snap.bytes {
        plan = build(repo, tx.state(), tx.snap.mtime, &policy, opts)?;
    }

    let mut writer = BlobWriter::new(repo, OP);
    let mut upserts = Vec::new();
    for staged in &plan.files {
        let bytes = stage::reread(repo, staged)?;
        let id = writer.write(&bytes)?;
        upserts.push(Upsert { path: staged.rel.clone(), stat: staged.stat, id, mode: staged.mode });
    }
    // Objects are on disk before the index that names them.
    writer.sync()?;
    for (path, id, stat) in &plan.gitlinks {
        upserts.push(Upsert { path: path.clone(), stat: *stat, id: *id, mode: Mode::COMMIT });
    }
    for path in &plan.removals {
        indexfile::remove(tx.state_mut(), path);
    }
    indexfile::upsert_all(tx.state_mut(), upserts);
    tx.commit(repo)?;
    Ok(report(&plan))
}

fn report(plan: &Plan) -> Outcome {
    Outcome {
        text: String::new(),
        err: String::new(),
        json: serde_json::json!({
            "added": plan.added,
            "modified": plan.modified,
            "removed": plan.removals,
            "refreshed": plan.refreshed,
        }),
    }
}

/// One stage-0 index entry, copied out so the state can be edited while it
/// is read.
pub(crate) struct Tracked {
    pub(crate) path: String,
    pub(crate) mode: Mode,
    pub(crate) id: ObjectId,
    pub(crate) stat: Stat,
    pub(crate) flags: Flags,
}

impl Tracked {
    pub(crate) fn all(state: &gix_index::State) -> Vec<Tracked> {
        state
            .entries()
            .iter()
            .filter(|e| e.stage() == gix_index::entry::Stage::Unconflicted)
            .map(|e| Tracked {
                path: e.path(state).to_str_lossy().into_owned(),
                mode: e.mode,
                id: e.id,
                stat: e.stat,
                flags: e.flags,
            })
            .collect()
    }

    /// Whether the working-tree file differs from this entry in content or
    /// mode. Git's `rm` refreshes the index first, so a stat mismatch alone
    /// is not a difference: the content decides.
    pub(crate) fn differs(
        &self,
        abs: &Path,
        meta: &std::fs::Metadata,
        index_mtime: Option<filetime::FileTime>,
        policy: &StagePolicy,
    ) -> bool {
        if unchanged(abs, meta, self, index_mtime, policy) {
            return false;
        }
        if stage::mode_of(meta, Some(self.mode), policy) != self.mode {
            return true;
        }
        match crate::worktree::read_worktree_blob(OP, &self.path, abs, meta, stage::MAX_STAGE_BYTES) {
            Ok(bytes) => crate::verbs::write::objects::blob_id(OP, &bytes).map_or(true, |id| id != self.id),
            Err(_) => true,
        }
    }
}

pub(crate) fn build(
    repo: &WriteRepo,
    state: &gix_index::State,
    index_mtime: Option<filetime::FileTime>,
    policy: &StagePolicy,
    opts: &AddOptions,
) -> Result<Plan, GitError> {
    let rd = repo.read();
    let work = rd.work_dir().expect("ensure_writable checked the work tree").to_path_buf();
    let mut specs = SpecSet::new(OP, &opts.specs)?;
    let mut attrs = AttributeReader::new(repo, &opts.command);
    let mut plan = Plan::default();
    let mut to_stage: BTreeMap<String, Option<Mode>> = BTreeMap::new();

    let tracked = Tracked::all(state);
    let tracked_paths: BTreeSet<&str> = tracked.iter().map(|t| t.path.as_str()).collect();
    let gitlinks: BTreeSet<&str> = tracked.iter().filter(|t| t.mode == Mode::COMMIT).map(|t| t.path.as_str()).collect();

    // Phase 1: tracked paths, changed or gone.
    for t in &tracked {
        if !specs.matches(&t.path) {
            continue;
        }
        if t.flags.contains(Flags::SKIP_WORKTREE) {
            if specs.names_exactly(&t.path) {
                return refuse(
                    1,
                    &opts.command,
                    format!("'{}' is outside the sparse-checkout definition, so its working-tree file is not the one the index tracks.", t.path),
                    "Leave it out of the command.",
                    Override::Repository,
                );
            }
            continue;
        }
        if t.flags.contains(Flags::ASSUME_VALID) {
            continue;
        }
        let abs = match repo.check_worktree_any(Path::new(&t.path)) {
            Ok(p) => p,
            // A parent directory was replaced by a file: this path is gone.
            Err(GitError::WriteThroughLink { found: "not a directory", .. }) => {
                plan.removals.push(t.path.clone());
                continue;
            }
            Err(e) => return Err(e),
        };
        let meta = match std::fs::symlink_metadata(&abs) {
            Ok(m) => m,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                plan.removals.push(t.path.clone());
                continue;
            }
            Err(e) => return Err(GitError::repository(OP, "reading a working-tree path", &abs, e)),
        };
        if t.mode == Mode::COMMIT {
            if let Some(id) = gitlink_head(repo, &abs)? {
                if id != t.id {
                    let stat = gix_index::fs::Metadata::from_path_no_follow(&abs)
                        .ok()
                        .and_then(|m| Stat::from_fs(&m).ok())
                        .unwrap_or_default();
                    plan.gitlinks.push((t.path.clone(), id, stat));
                    plan.modified.push(t.path.clone());
                }
            }
            continue;
        }
        let ft = meta.file_type();
        if ft.is_dir() || (!ft.is_file() && !ft.is_symlink()) {
            // A file replaced by a directory (its children are staged by the
            // walk below, which replaces this entry) or by something git
            // cannot store: the file is gone as far as the index goes.
            if ft.is_dir() {
                plan.removals.push(t.path.clone());
                continue;
            }
            return refuse(
                4,
                &opts.command,
                format!("'{}' is neither a regular file, a symbolic link nor a git directory, and git stores nothing else.", t.path),
                "Leave it out of the command, or remove it from the working tree.",
                Override::Repository,
            );
        }
        if unchanged(&abs, &meta, t, index_mtime, policy) {
            continue;
        }
        to_stage.insert(t.path.clone(), Some(t.mode));
    }

    // Phase 2: untracked paths.
    let mut ignored_hits: Vec<String> = Vec::new();
    let mut embedded: Vec<String> = Vec::new();
    if !opts.update {
        let mut ignore = if opts.force { None } else { Some(Ignore::new(OP, rd, &work, state)?) };
        let mut walker = Walker {
            repo,
            work: &work,
            specs: &mut specs,
            ignore: ignore.as_mut(),
            tracked: &tracked_paths,
            gitlinks: &gitlinks,
            found: Vec::new(),
            ignored_hits: &mut ignored_hits,
            embedded: &mut embedded,
        };
        walker.dir("")?;
        let found = walker.found;
        for rel in found {
            to_stage.entry(rel).or_insert(None);
        }

        // A path named exactly that the walk did not reach: inside an ignored
        // directory, or past a prune.
        for lit in specs.literals() {
            if specs.is_marked(&lit) {
                continue;
            }
            let abs = repo.check_worktree_any(Path::new(&lit))?;
            let Ok(meta) = std::fs::symlink_metadata(&abs) else { continue };
            specs.mark_literal(&lit);
            if meta.is_dir() || tracked_paths.contains(lit.as_str()) {
                continue;
            }
            let ign = match ignore.as_mut() {
                Some(i) => i.ignored_with_parents(OP, rd, &work, &lit, false)?,
                None => false,
            };
            if ign {
                ignored_hits.push(lit);
            } else {
                to_stage.entry(lit).or_insert(None);
            }
        }
    }

    if !embedded.is_empty() {
        embedded.sort();
        return refuse(
            4,
            &opts.command,
            format!("'{}' is a git repository of its own that the index does not track as a submodule. Real git would add it as an embedded repository, which almost never is what was meant.", embedded[0]),
            format!("Leave it out of the command, for example: git add <other-path>. To track it as a submodule, use real git outside this shell: git submodule add <url> {}.", embedded[0]),
            Override::Repository,
        );
    }
    if !ignored_hits.is_empty() {
        ignored_hits.sort();
        ignored_hits.dedup();
        return refuse(
            1,
            &opts.command,
            format!("The following paths are ignored by one of your .gitignore files: {}.", ignored_hits.join(", ")),
            format!("To stage them anyway, run: git add -f {}.", ignored_hits.join(" ")),
            Override::Flag("-f"),
        );
    }
    let missing = specs.unmatched();
    if !missing.is_empty() {
        return refuse(
            1,
            &opts.command,
            format!("The pathspec '{}' did not match any files.", missing[0]),
            "Check the name with: git status.",
            Override::Repository,
        );
    }

    // Pass 1 of staging: read, check, hash. Nothing is written yet.
    for (rel, existing) in to_stage {
        let staged = stage::inspect(repo, &mut attrs, policy, &opts.command, &rel, existing)?;
        match tracked.iter().find(|t| t.path == rel) {
            Some(t) if t.id == staged.id && t.mode == staged.mode => plan.refreshed += 1,
            Some(_) => plan.modified.push(rel.clone()),
            None => plan.added.push(rel.clone()),
        }
        plan.files.push(staged);
    }
    plan.removals.sort();
    Ok(plan)
}

/// Whether the index entry still describes the file: git's `ie_match_stat`,
/// including its content check for an entry as new as the index.
fn unchanged(
    abs: &Path,
    meta: &std::fs::Metadata,
    t: &Tracked,
    index_mtime: Option<filetime::FileTime>,
    policy: &StagePolicy,
) -> bool {
    let Ok(gmeta) = gix_index::fs::Metadata::from_path_no_follow(abs) else { return false };
    let Ok(now) = Stat::from_fs(&gmeta) else { return false };
    let opts = gix_index::entry::stat::Options { trust_ctime: true, check_stat: true, use_nsec: true, use_stdev: false };
    if !t.stat.matches(&now, opts) {
        return false;
    }
    if stage::mode_of(meta, Some(t.mode), policy) != t.mode {
        return false;
    }
    let racy = match index_mtime {
        Some(m) => t.stat.is_racy(m, opts),
        // No index file: nothing was ever recorded as clean.
        None => true,
    };
    if !racy {
        return true;
    }
    match crate::worktree::read_worktree_blob(OP, &t.path, abs, meta, stage::MAX_STAGE_BYTES) {
        Ok(bytes) => crate::verbs::write::objects::blob_id(OP, &bytes).is_ok_and(|id| id == t.id),
        Err(_) => false,
    }
}

/// The HEAD commit of a nested repository, or `None` when the directory has no
/// repository of its own (an empty submodule directory).
pub(crate) fn gitlink_head(repo: &WriteRepo, abs: &Path) -> Result<Option<ObjectId>, GitError> {
    if std::fs::symlink_metadata(abs.join(".git")).is_err() {
        return Ok(None);
    }
    let rd = repo.read();
    let nested = ReadRepo::discover(OP, abs, rd.ceiling())?;
    let same = std::fs::canonicalize(abs).is_ok_and(|c| c == nested.root());
    if !same {
        return Ok(None);
    }
    match nested.head()?.oid {
        Some(hex) => Ok(Some(ObjectId::from_hex(hex.as_bytes()).map_err(|e| {
            GitError::repository(OP, "reading a submodule's HEAD", abs, e)
        })?)),
        None => Ok(None),
    }
}

struct Walker<'a> {
    repo: &'a WriteRepo,
    work: &'a Path,
    specs: &'a mut SpecSet,
    ignore: Option<&'a mut Ignore>,
    tracked: &'a BTreeSet<&'a str>,
    gitlinks: &'a BTreeSet<&'a str>,
    found: Vec<String>,
    ignored_hits: &'a mut Vec<String>,
    embedded: &'a mut Vec<String>,
}

impl Walker<'_> {
    fn is_ignored(&mut self, rel: &str, is_dir: bool) -> Result<bool, GitError> {
        let rd = self.repo.read();
        match self.ignore.as_deref_mut() {
            Some(i) => i.matches(OP, rd, self.work, rel, is_dir),
            None => Ok(false),
        }
    }

    fn dir(&mut self, rel_dir: &str) -> Result<(), GitError> {
        let abs = if rel_dir.is_empty() { self.work.to_path_buf() } else { crate::worktree::join_repo_relative(self.work, rel_dir) };
        let mut names = Vec::new();
        for entry in std::fs::read_dir(&abs).map_err(|e| GitError::repository(OP, "listing a working-tree directory", &abs, e))? {
            let entry = entry.map_err(|e| GitError::repository(OP, "listing a working-tree directory", &abs, e))?;
            match entry.file_name().into_string() {
                Ok(n) => names.push(n),
                Err(_) => {
                    return refuse(
                        4,
                        format!("git {OP}"),
                        format!("The directory '{rel_dir}' holds a file whose name is not valid UTF-8, which this shell cannot name or stage."),
                        "Rename it, then run the command again.",
                        Override::Repository,
                    )
                }
            }
        }
        names.sort();
        for name in names {
            if name.eq_ignore_ascii_case(".git") {
                continue;
            }
            let rel = if rel_dir.is_empty() { name } else { format!("{rel_dir}/{name}") };
            let full = crate::worktree::join_repo_relative(self.work, &rel);
            let meta = match std::fs::symlink_metadata(&full) {
                Ok(m) => m,
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => continue,
                Err(e) => return Err(GitError::repository(OP, "reading a working-tree path", &full, e)),
            };
            if meta.is_dir() {
                self.visit_dir(&rel, &full)?;
            } else if !self.tracked.contains(rel.as_str()) {
                let hits = self.specs.hits(&rel);
                if hits.is_empty() && !self.specs.is_empty() {
                    continue;
                }
                if self.is_ignored(&rel, false)? {
                    if self.specs.names_exactly(&rel) {
                        self.specs.mark(&hits);
                        self.ignored_hits.push(rel);
                    }
                    continue;
                }
                self.specs.mark(&hits);
                self.found.push(rel);
            }
        }
        Ok(())
    }

    fn visit_dir(&mut self, rel: &str, full: &Path) -> Result<(), GitError> {
        if self.gitlinks.contains(rel) {
            return Ok(());
        }
        if !self.specs.relevant_dir(rel) {
            return Ok(());
        }
        if self.is_ignored(rel, true)? {
            if self.specs.names_exactly(rel) {
                self.specs.mark_literal(rel);
            }
            return Ok(());
        }
        if std::fs::symlink_metadata(full.join(".git")).is_ok() {
            self.embedded.push(rel.to_string());
            return Ok(());
        }
        // A literal directory spec that exists is a match even if it holds
        // nothing to stage (git: `add emptydir` is not an error).
        if self.specs.names_exactly(rel) {
            self.specs.mark_literal(rel);
        }
        self.dir(rel)
    }
}
