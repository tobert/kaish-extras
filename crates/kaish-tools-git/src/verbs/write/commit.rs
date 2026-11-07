//! `git commit` (git-write-profile.md §3, §4, §5, §6).
//!
//! Accepts `-m` (repeatable, one paragraph each), `-F <file>`, `-a`,
//! `--only <paths>` / `<paths>` / `-- <paths>`, `--trailer`, `--allow-empty`,
//! `--author`, `--no-verify` (see below) and `--amend` (with `--no-edit` and
//! `--reset-author`). Refuses everything that needs an editor, a
//! terminal or a signer. `--amend` is a branch move that drops the old tip, so
//! it goes through [`rewrite::check_move`] before anything is written.
//!
//! The order of a commit is the design's (§6): the index lock first when the
//! index changes (`-a`, `--only`), then blobs, trees, the commit, an fsync of
//! every new loose object, and last the ref transaction. A crash before the
//! transaction leaves unreachable objects and nothing else.
//!
//! Where it differs from git, on purpose, and kept in docs/issues.md: the
//! summary line shows a rename as a delete and a create; a bare `--trailer`
//! key is refused; the committer date is the clock unless
//! `GIT_COMMITTER_DATE` is set, and a date without an offset is UTC.

use std::collections::BTreeMap;
use std::path::Path;
use std::sync::Arc;

use clap::Parser;
use gix_index::entry::{Flags, Mode};
use gix_index::hash::ObjectId;
use kaish_tool_api::GlobalFlags;

use crate::config::{Identity, RewritePolicy};
use crate::error::GitError;
use crate::hooks::{GitHooks, HookContext, RewrittenCommit};
use crate::verbs::write::add::{self, AddOptions, Plan, Tracked};
use crate::verbs::write::attributes::AttributeReader;
use crate::verbs::write::head;
use crate::verbs::write::hookrun::{self, note, Tx};
use crate::verbs::write::ident::{self, KernelVars};
use crate::verbs::write::indexfile::{self, IndexTx, Snapshot, Upsert};
use crate::verbs::write::message::{self, Trailer};
use crate::verbs::write::objects::BlobWriter;
use crate::verbs::write::refmove::{self, HeadTarget, MoveKind};
use crate::verbs::write::rewrite::{self, MoveCheck, Mover, NewTip};
use crate::verbs::write::refusal::{refuse, Override};
use crate::verbs::write::relpath::{os, show, RelPath};
use crate::verbs::write::repo::WriteRepo;
use crate::verbs::write::spec::{Outcome, SpecSet};
use crate::verbs::write::stage;
use crate::verbs::write::state::{config_bool, config_last, ensure_writable_with, hooks_dir, StagePolicy};
use crate::verbs::write::tree::{self, Item};

const OP: &str = "commit";

/// The hooks git runs for a commit, in the order it runs them. A provider is
/// told which of them exist.
const COMMIT_HOOKS: &[&str] = &["pre-commit", "prepare-commit-msg", "commit-msg", "post-commit", "reference-transaction"];

/// Git also runs `post-rewrite` after an amend.
const AMEND_HOOKS: &[&str] = &["post-rewrite"];

/// The most bytes of one blob the summary counts lines in.
const SUMMARY_BLOB_CAP: u64 = 16 * 1024 * 1024;

/// `git commit`'s argv surface.
#[derive(Parser, Debug)]
#[command(
    name = "commit",
    about = "Record the index as a new commit on the current branch. The message comes from -m or -F, and there is no editor. Never runs hooks or signs unless the embedder provides that"
)]
pub(crate) struct CommitArgs {
    /// The commit message. Repeat the flag for more paragraphs: each -m is a
    /// paragraph, and the first is the subject. Cleaned as git does with -m:
    /// trailing whitespace off every line, blank-line runs collapsed, leading
    /// and trailing blank lines dropped; lines starting with `#` stay.
    #[arg(short = 'm', long = "message", value_name = "MSG")]
    pub message: Vec<String>,

    /// Read the commit message from this file (a path in this shell's
    /// filesystem), cleaned the same way as -m. Not with -m.
    #[arg(short = 'F', long = "file", value_name = "FILE")]
    pub file: Option<String>,

    /// Stage every change to tracked files first (changed and deleted, no new
    /// files), then commit, as `git add -u` followed by `git commit` would.
    /// Not with paths.
    #[arg(short = 'a', long = "all")]
    pub all: bool,

    /// Commit only the named paths, with their working-tree content, and
    /// leave other staged changes staged. Paths must be tracked. Naming
    /// paths without this flag means the same.
    #[arg(short = 'o', long = "only")]
    pub only: bool,

    /// Add a trailer line to the message, `Key: value`, placed as
    /// `git interpret-trailers` places it: in the last paragraph when that
    /// is a trailer block, else in a new paragraph. Repeatable.
    #[arg(long = "trailer", value_name = "KEY: VALUE")]
    pub trailer: Vec<String>,

    /// Commit even when the tree is the same as the parent's.
    #[arg(long = "allow-empty")]
    pub allow_empty: bool,

    /// Use this author instead of GIT_AUTHOR_NAME and GIT_AUTHOR_EMAIL, in
    /// the form 'Name <email>'.
    #[arg(long = "author", value_name = "NAME <EMAIL>")]
    pub author: Option<String>,

    /// Skip the pre-commit and commit-msg hooks. Refused when the repository
    /// has a hook and the embedder provides no hook runner; with a runner,
    /// the runner decides what it means.
    #[arg(short = 'n', long = "no-verify")]
    pub no_verify: bool,

    /// Print nothing on success.
    #[arg(short = 'q', long = "quiet")]
    pub quiet: bool,

    /// Replace the current commit with a new one made from the index. The
    /// author (name, email and date) stays unless --author or --reset-author
    /// is given. Give the new message with -m or -F, or keep the old one with
    /// --no-edit; there is no editor. Refused when the rewrite policy forbids
    /// it: by default, when a remote-tracking ref (the last fetch, not the
    /// remote) already has the commit.
    #[arg(long = "amend")]
    pub amend: bool,

    /// With --amend, keep the commit's message (trailers and cleanup still
    /// apply). -m or -F wins when given.
    #[arg(long = "no-edit")]
    pub no_edit: bool,

    /// With --amend, make the author the identity in the variables, with the
    /// current time, instead of the old commit's.
    #[arg(long = "reset-author")]
    pub reset_author: bool,

    /// Refused: this shell has no autosquash. Make a normal commit.
    #[arg(long = "fixup", value_name = "COMMIT")]
    pub fixup: Option<String>,

    /// Refused: this shell has no autosquash. Make a normal commit.
    #[arg(long = "squash", value_name = "COMMIT")]
    pub squash: Option<String>,

    /// Refused: this shell never signs.
    #[arg(short = 'S', long = "gpg-sign", num_args = 0..=1, require_equals = true, value_name = "KEYID")]
    pub gpg_sign: Option<Option<String>>,

    /// Accepted: this shell never signs.
    #[arg(long = "no-gpg-sign")]
    pub no_gpg_sign: bool,

    /// Refused: choosing hunks needs a terminal this shell does not have.
    #[arg(short = 'p', long = "patch")]
    pub patch: bool,

    /// Refused: the interactive menu needs a terminal this shell does not have.
    #[arg(long = "interactive")]
    pub interactive: bool,

    /// Refused: there is no editor in this shell.
    #[arg(short = 'e', long = "edit")]
    pub edit: bool,

    /// Refused: the verbose template is for an editor, and there is none.
    #[arg(short = 'v', long = "verbose")]
    pub verbose: bool,

    /// Refused: stage the paths with `git add`, then commit.
    #[arg(short = 'i', long = "include")]
    pub include: bool,

    /// Repository to change. Defaults to the current directory; discovery
    /// searches upward, never past the mount root that contains the path.
    #[arg(long = "repo", value_name = "PATH")]
    pub repo: Option<String>,

    #[command(flatten)]
    pub global: GlobalFlags,

    /// Paths to commit, relative to the current directory, as with --only.
    #[arg(hide = true)]
    pub operands: Vec<String>,
}

impl CommitArgs {
    /// The refusals that need no repository. `command` is the text as typed.
    pub(crate) fn refuse_flags(&self, command: &str) -> Result<(), GitError> {
        let no_message = "Pass the message with -m, for example: git commit -m '<message>'.";
        if self.fixup.is_some() || self.squash.is_some() {
            let flag = if self.fixup.is_some() { "--fixup" } else { "--squash" };
            return refuse(
                2,
                command,
                format!("{flag} makes a commit for an autosquash to fold later, and this shell has no autosquash, so the commit would stay in history as it is."),
                "Make a normal commit instead: git commit -m '<message>'.",
                Override::Never,
            );
        }
        if self.gpg_sign.is_some() {
            return refuse(
                2,
                command,
                "This shell never signs a commit.",
                "Commit without signing: git commit -m '<message>'. A signed commit needs real git, outside this shell.",
                Override::Never,
            );
        }
        if self.patch || self.interactive {
            return refuse(
                2,
                command,
                "Choosing hunks needs a terminal, and this shell has none.",
                "Stage whole files, then commit: git add <path>, then git commit -m '<message>'.",
                Override::Never,
            );
        }
        if self.edit {
            return refuse(2, command, "There is no editor in this shell.", no_message, Override::Never);
        }
        if self.verbose {
            return refuse(
                2,
                command,
                "-v adds the diff to an editor template, and there is no editor in this shell.",
                "Review the staged changes with git diff --cached, then commit: git commit -m '<message>'.",
                Override::Never,
            );
        }
        if self.include {
            return refuse(
                2,
                command,
                "-i stages the named paths in addition to the index, and this shell commits what is staged or what --only names.",
                "Stage them first, then commit: git add <path>, then git commit -m '<message>'.",
                Override::Never,
            );
        }
        if self.no_edit && !self.amend {
            return refuse(
                2,
                command,
                "--no-edit keeps the old message, and only --amend has an old message.",
                "Give the message: git commit -m '<message>', or amend: git commit --amend --no-edit.",
                Override::Never,
            );
        }
        if self.reset_author && !self.amend {
            return refuse(
                2,
                command,
                "--reset-author renews the author of an amended commit, and there is no --amend here.",
                "Amend: git commit --amend --no-edit --reset-author.",
                Override::Never,
            );
        }
        if self.reset_author && self.author.is_some() {
            return refuse(
                2,
                command,
                "--reset-author and --author both choose the author, and git refuses the pair.",
                "Use one: git commit --amend --no-edit --author 'Name <email>', or git commit --amend --no-edit --reset-author.",
                Override::Never,
            );
        }
        if !self.message.is_empty() && self.file.is_some() {
            return refuse(2, command, "-m and -F both give the message.", "Use one of them.", Override::Never);
        }
        if self.amend && self.message.is_empty() && self.file.is_none() && !self.no_edit {
            return refuse(
                2,
                command,
                "There is no editor in this shell; pass -m '<message>' or --no-edit.",
                "Give the new message: git commit --amend -m '<message>', or keep the old one: git commit --amend --no-edit.",
                Override::Never,
            );
        }
        if !self.amend && self.message.is_empty() && self.file.is_none() {
            return refuse(
                2,
                command,
                "There is no editor in this shell; pass -m '<message>'.",
                no_message,
                Override::Never,
            );
        }
        Ok(())
    }
}

/// What `--amend` carries.
pub(crate) struct Amend {
    pub(crate) reset_author: bool,
}

pub(crate) struct CommitOptions {
    /// The message as given, before cleanup: `-m` paragraphs joined, or the
    /// file's text. `None` is `--amend --no-edit`: the old message stays.
    pub(crate) raw_message: Option<String>,
    pub(crate) amend: Option<Amend>,
    pub(crate) trailers: Vec<Trailer>,
    pub(crate) all: bool,
    /// Repo-relative paths; non-empty means `--only`.
    pub(crate) specs: Vec<String>,
    pub(crate) allow_empty: bool,
    pub(crate) author: Option<(String, String)>,
    pub(crate) no_verify: bool,
    pub(crate) quiet: bool,
    pub(crate) vars: KernelVars,
    pub(crate) command: String,
}

/// What the embedder installed that a commit consults.
pub(crate) struct CommitEnv {
    pub(crate) hooks: Option<Arc<dyn GitHooks>>,
    pub(crate) identity: Option<(Identity, Identity)>,
    pub(crate) policy: RewritePolicy,
}

/// The parts of the commit being amended that the new one is built from.
struct Amended {
    parents: Vec<ObjectId>,
    author: ident::Signature,
    message: String,
    extra_headers: Vec<(gix_object::bstr::BString, gix_object::bstr::BString)>,
}

/// Read the commit `--amend` replaces. Refuses what it cannot carry over
/// exactly: an `encoding` header, or a message that is not UTF-8.
fn read_amended(repo: &WriteRepo, cmd: &str, oid: ObjectId) -> Result<Amended, GitError> {
    use gix_object::FindExt;
    let mut buf = Vec::new();
    let c = repo
        .objects()
        .find_commit(&oid, &mut buf)
        .map_err(|e| GitError::repository(OP, "reading the commit to amend", repo.git_dir(), e))?;
    if c.encoding.is_some() {
        return refuse(
            4,
            cmd,
            "The commit to amend has an encoding header, and this shell writes commits without one.",
            "Make a new commit instead: git commit -m '<message>'.",
            Override::Repository,
        );
    }
    let Ok(message) = std::str::from_utf8(c.message) else {
        return refuse(
            4,
            cmd,
            "The message of the commit to amend is not valid UTF-8, and this shell reads only UTF-8 messages.",
            "Give a new message: git commit --amend -m '<message>'.",
            Override::Repository,
        );
    };
    let a = c
        .author()
        .map_err(|e| GitError::repository(OP, "reading the author to amend", repo.git_dir(), e))?;
    let time = a
        .time()
        .map_err(|e| GitError::repository(OP, "reading the author date to amend", repo.git_dir(), e))?;
    let (Ok(name), Ok(email)) = (std::str::from_utf8(a.name), std::str::from_utf8(a.email)) else {
        return refuse(
            4,
            cmd,
            "The author of the commit to amend is not valid UTF-8, and this shell reads only UTF-8 identities.",
            "Make a new commit instead: git commit -m '<message>'.",
            Override::Repository,
        );
    };
    let author = ident::Signature {
        name: name.to_string(),
        email: email.to_string(),
        seconds: time.seconds,
        offset: time.offset,
    };
    // Git carries the other headers over and drops a signature, which no
    // longer matches.
    let extra_headers = c
        .extra_headers
        .iter()
        .filter(|(k, _)| *k != "gpgsig" && *k != "gpgsig-sha256")
        .map(|(k, v)| ((*k).to_owned(), v.as_ref().to_owned()))
        .collect();
    Ok(Amended { parents: c.parents().collect(), author, message: message.to_string(), extra_headers })
}

type Map = BTreeMap<RelPath, (ObjectId, Mode)>;

/// The index change a commit makes, and the tree it commits.
struct Prepared {
    plan: Plan,
    /// The final path map: what the commit's tree holds.
    items: Map,
}

pub(crate) fn run(repo: &WriteRepo, env: &CommitEnv, opts: &CommitOptions) -> Result<Outcome, GitError> {
    let cmd = opts.command.as_str();
    let policy = ensure_writable_with(repo, cmd, false)?;
    ensure_commit_settings(repo, cmd)?;
    let only = !opts.specs.is_empty();
    let writes_index = opts.all || only;
    let (hooks_dir, present) = scan_hooks(repo, cmd, writes_index, opts.amend.is_some())?;
    if env.hooks.is_none() {
        refuse_hooks(cmd, &present, opts.no_verify)?;
    }
    let head = repo.read().head()?;
    let target = refmove::head_target(repo, OP)?;
    // Walk the ref's whole write set and look for its locks before any object
    // is written, so a planted link or a stale lock refuses the commit with
    // `.git` untouched. The move does both again just before the transaction.
    refmove::precheck(repo, cmd, &target)?;
    let old = head
        .oid
        .as_deref()
        .map(|hex| ObjectId::from_hex(hex.as_bytes()).map_err(|e| GitError::repository(OP, "reading HEAD", repo.git_dir(), e)))
        .transpose()?;
    let amended = match (&opts.amend, old) {
        (None, _) => None,
        (Some(_), None) => {
            return refuse(
                1,
                cmd,
                "There is no commit to amend: this branch has no commits yet.",
                "Make the first commit: git commit -m '<message>'.",
                Override::Never,
            )
        }
        (Some(_), Some(oid)) => Some(read_amended(repo, cmd, oid)?),
    };
    if let Some(a) = &amended {
        // Before any object is written: a refusal leaves `.git` as it was.
        rewrite::check_move(
            repo,
            &MoveCheck { command: cmd, mover: Mover::Amend, policy: env.policy },
            old,
            &NewTip::Replacement(a.parents.clone()),
        )?;
    }
    let idents = match (&amended, &opts.amend) {
        (Some(a), Some(am)) => ident::resolve_amend(
            cmd,
            env.identity.as_ref(),
            &opts.vars,
            opts.author.clone(),
            ident::now(),
            &a.author,
            am.reset_author,
        )?,
        _ => ident::resolve(cmd, env.identity.as_ref(), &opts.vars, opts.author.clone(), ident::now())?,
    };
    let head_map = head::flatten(repo, OP)?;
    // What "nothing changed" is measured against: HEAD's tree for a commit,
    // the amended commit's first parent's for an amend.
    let base_map = match &amended {
        None => head_map.clone(),
        Some(a) => match a.parents.first() {
            Some(p) => head::flatten_commit(repo, OP, *p)?,
            None => Map::new(),
        },
    };
    let merge_amend = amended.as_ref().is_some_and(|a| a.parents.len() > 1);

    // Plan from an unlocked read, so a refusal creates no file in `.git`.
    let snap = Snapshot::read(repo, cmd)?;
    let mut prepared = prepare(repo, &snap.state, snap.mtime, &policy, &head_map, opts)?;
    let message = final_message(opts, amended.as_ref().map(|a| a.message.as_str()))?;
    check_not_empty(&prepared.items, &base_map, opts.allow_empty || merge_amend, cmd, amended.is_some())?;
    // Every refusal the tree writer could raise comes before the first blob.
    validate_tree(repo, cmd, &prepared)?;

    let mut tx: Option<IndexTx> = None;
    if writes_index {
        let locked = IndexTx::lock(repo, cmd)?;
        if locked.snap.bytes != snap.bytes {
            prepared = prepare(repo, locked.state(), locked.snap.mtime, &policy, &head_map, opts)?;
            check_not_empty(&prepared.items, &base_map, opts.allow_empty || merge_amend, cmd, amended.is_some())?;
            validate_tree(repo, cmd, &prepared)?;
        }
        tx = Some(locked);
    }

    let mut writer = BlobWriter::new(repo, OP);
    if let Some(tx) = tx.as_mut() {
        apply_to_index(repo, &mut writer, tx, &prepared.plan).map_err(GitError::after_objects)?;
    } else {
        // Plain commit: nothing is staged here, but a plan with files is
        // impossible without -a or --only.
        debug_assert!(prepared.plan.files.is_empty());
    }
    let items: Vec<Item> = prepared.items.iter().map(|(p, (id, m))| Item { path: p.clone(), mode: *m, id: *id }).collect();
    // Blobs may be on disk by now: a refusal from here on says so.
    let tree_id = tree::write_tree(repo, &mut writer, cmd, items).map_err(GitError::after_objects)?;

    // Hooks. The tree is written; a refusal here leaves unreachable objects.
    let tree_hex = tree_id.to_string();
    let work = repo.read().work_dir();
    let ctx = HookContext {
        worktree: work,
        git_dir: repo.git_dir(),
        hooks_dir: &hooks_dir,
        tree: Some(&tree_hex),
        hooks_present: &present,
        no_verify: opts.no_verify,
    };
    let mut err = String::new();
    let mut message = message;
    if let Some(h) = env.hooks.as_ref() {
        match h.pre_commit(&ctx) {
            Ok(out) => note(&mut err, &out.message),
            Err(e) => return hook_refusal(cmd, "pre-commit", &e.message).map_err(GitError::after_objects),
        }
        let mut text = message.clone();
        match h.commit_msg(&ctx, &mut text) {
            Ok(out) => note(&mut err, &out.message),
            Err(e) => return hook_refusal(cmd, "commit-msg", &e.message).map_err(GitError::after_objects),
        }
        if text != message {
            // Git cleans the file again after the hook has been at it.
            message = message::cleanup(&text);
            if message.is_empty() {
                return refuse(
                    1,
                    cmd,
                    "The commit-msg hook left the message empty.",
                    "Run the commit again with a message: git commit -m '<message>'.",
                    Override::Never,
                )
                .map_err(GitError::after_objects);
            }
        }
    }

    let mut commit = gix_object::Commit {
        tree: tree_id,
        parents: Default::default(),
        author: idents.author.to_actor(),
        committer: idents.committer.to_actor(),
        encoding: None,
        message: message.as_str().into(),
        extra_headers: amended.as_ref().map(|a| a.extra_headers.clone()).unwrap_or_default(),
    };
    match &amended {
        Some(a) => commit.parents.extend(a.parents.iter().copied()),
        None => {
            if let Some(parent) = old {
                commit.parents.push(parent);
            }
        }
    }
    let is_root = commit.parents.is_empty();
    let mut buf = Vec::new();
    gix_object::WriteTo::write_to(&commit, &mut buf)
        .map_err(|e| GitError::repository(OP, "encoding the commit", Path::new("."), e))?;
    let commit_id = writer.write_object(gix_object::Kind::Commit, &buf).map_err(GitError::after_objects)?;
    // The objects reach the disk before the ref that names them.
    writer.sync().map_err(GitError::after_objects)?;

    let kind = if amended.is_some() {
        "commit (amend)"
    } else if old.is_none() {
        "commit (initial)"
    } else {
        "commit"
    };
    let reflog = format!("{kind}: {}", message::subject(&message));
    // The ref's name as git's hook sees it: the branch, or HEAD when detached.
    let ref_name = match &target {
        HeadTarget::Branch(full) => full.clone(),
        HeadTarget::Detached => "HEAD".to_string(),
    };
    let ref_tx = Tx::via_head(hookrun::oid_or_null(old), commit_id.to_string(), ref_name);
    if let Some(h) = env.hooks.as_ref() {
        // The commit object is written; a refusal here leaves it unreachable.
        hookrun::prepared(h.as_ref(), &ctx, MoveKind::COMMIT, cmd, std::slice::from_ref(&ref_tx), &mut err)
            .map_err(GitError::after_objects)?;
    }
    if let Err(e) = refmove::move_head(repo, cmd, &target, old, commit_id, &reflog, &idents.committer) {
        if let Some(h) = env.hooks.as_ref() {
            hookrun::aborted(h.as_ref(), &ctx, &ref_tx, &mut err);
        }
        return Err(e.after_objects());
    }
    if let Some(h) = env.hooks.as_ref() {
        hookrun::committed(h.as_ref(), &ctx, &ref_tx, &mut err);
    }

    if let Some(tx) = tx {
        // The ref has moved; the index follows, as in git. A failure here
        // leaves the commit made and the index as it was.
        tx.commit(repo).map_err(|e| index_after_commit(&e, commit_id))?;
    }
    if let Some(h) = env.hooks.as_ref() {
        let out = h.post_commit(&ctx, &commit_id.to_string());
        note(&mut err, &out.message);
        if let Some(prev) = amended.as_ref().and(old) {
            let out = h.post_rewrite(&ctx, "amend", &[RewrittenCommit::new(prev.to_string(), commit_id.to_string())]);
            note(&mut err, &out.message);
        }
    }

    let detached = matches!(target, HeadTarget::Detached);
    if detached {
        note(
            &mut err,
            &format!(
                "git commit: HEAD is detached, so commit {commit_id} is on no branch. To keep it on a branch: git switch -c <name>."
            ),
        );
    }
    let summary = summarize(repo, &base_map, &prepared.items, &mut err)?;
    let short = commit_id.to_string()[..7].to_string();
    // Git says "root-commit" for a commit made on an unborn branch, not for an
    // amended root commit.
    let label = match (&head.branch, old.is_none()) {
        (_, true) if detached => "detached HEAD (root-commit)".to_string(),
        (Some(b), true) => format!("{b} (root-commit)"),
        (Some(b), false) => b.clone(),
        (None, true) => "detached HEAD (root-commit)".to_string(),
        (None, false) => "detached HEAD".to_string(),
    };
    let mut text = format!("[{label} {short}] {}\n", message::subject(&message));
    if idents.author.name != idents.committer.name || idents.author.email != idents.committer.email {
        text.push_str(&format!(" Author: {} <{}>\n", idents.author.name, idents.author.email));
    }
    if amended.is_some() && !opts.amend.as_ref().is_some_and(|a| a.reset_author) {
        // Git shows the author date on an amend, where it can differ from the
        // committer's. With --reset-author the date is the committer's, and
        // git leaves the line out.
        let when = gix_date::Time::new(idents.author.seconds, idents.author.offset)
            .format_or_unix(gix_date::time::format::DEFAULT);
        text.push_str(&format!(" Date: {when}\n"));
    }
    text.push_str(&summary.text);
    Ok(Outcome {
        text: if opts.quiet { String::new() } else { text },
        err,
        json: serde_json::json!({
            "commit": commit_id.to_string(),
            "tree": tree_hex,
            "parent": commit.parents.first().map(|o| o.to_string()),
            "amended": amended.as_ref().and(old).map(|o| o.to_string()),
            "branch": head.branch,
            "root": is_root,
            "files_changed": summary.files,
            "insertions": summary.insertions,
            "deletions": summary.deletions,
            "author": idents.author.ident_line(),
            "committer": idents.committer.ident_line(),
        }),
    })
}

/// Run the tree writer's checks on the final path map, writing nothing.
fn validate_tree(repo: &WriteRepo, cmd: &str, prepared: &Prepared) -> Result<(), GitError> {
    let items: Vec<Item> = prepared.items.iter().map(|(p, (id, m))| Item { path: p.clone(), mode: *m, id: *id }).collect();
    let planned = prepared.plan.files.iter().map(|s| s.id).collect();
    tree::validate_tree(repo, cmd, items, &planned)?;
    // Every planned blob's object path passes containment before the first is
    // written, so a planted link at a later blob's directory refuses with no
    // blob on disk.
    let vet = BlobWriter::new(repo, OP);
    for staged in &prepared.plan.files {
        vet.vet(&staged.id)?;
    }
    Ok(())
}

fn hook_refusal<T>(command: &str, hook: &str, why: &str) -> Result<T, GitError> {
    refuse(
        1,
        command,
        format!("The {hook} hook refused the commit: {why}"),
        "Fix what it names, then make the commit again.",
        Override::Embedder,
    )
}

fn index_after_commit(e: &GitError, commit: ObjectId) -> GitError {
    GitError::Usage {
        operation: OP,
        message: format!("the commit {commit} was made, and the branch now names it, but the index could not be written afterward ({e}). Run git status; the index still holds the state from before the commit"),
    }
}

/// The cleaned message with trailers, refusing an empty one as git does.
fn final_message(opts: &CommitOptions, kept: Option<&str>) -> Result<String, GitError> {
    let raw = match (&opts.raw_message, kept) {
        (Some(m), _) => m.as_str(),
        (None, Some(k)) => k,
        (None, None) => "",
    };
    let cleaned = message::cleanup(raw);
    if cleaned.is_empty() {
        return refuse(
            1,
            &opts.command,
            "Aborting commit due to an empty commit message.",
            "Give a message with text in it: git commit -m '<message>'.",
            Override::Never,
        );
    }
    Ok(message::add_trailers(&cleaned, &opts.trailers))
}

fn check_not_empty(items: &Map, head_map: &Map, allow_empty: bool, command: &str, amend: bool) -> Result<(), GitError> {
    if allow_empty || items != head_map {
        return Ok(());
    }
    if amend {
        return refuse(
            1,
            command,
            "Amending would leave HEAD's commit empty: its tree would be the same as its parent's.",
            "To amend anyway: git commit --amend --allow-empty -m '<message>'.",
            Override::Flag("--allow-empty"),
        );
    }
    refuse(
        1,
        command,
        "Nothing to commit: the tree to commit is the same as the parent's.",
        "Stage changes with git add <path>, or commit every tracked change with git commit -am '<message>'. To commit anyway: git commit --allow-empty -m '<message>'.",
        Override::Flag("--allow-empty"),
    )
}

/// Settings that would make this shell's commit differ from git's.
fn ensure_commit_settings(repo: &WriteRepo, command: &str) -> Result<(), GitError> {
    for (section, key, shown) in [("commit", "gpgsign", "commit.gpgSign"), ("tag", "gpgsign", "tag.gpgSign")] {
        if config_bool(repo, command, section, key, shown)? == Some(true) {
            return refuse(
                4,
                command,
                format!("{shown} is true in this repository, and this shell never signs."),
                format!("Commit with real git, outside this shell, or turn signing off for this repository: git config {shown} false."),
                Override::Repository,
            );
        }
    }
    let bad = |what: String, fix: String| refuse::<()>(4, command, what, fix, Override::Repository);
    if config_last(repo, "i18n", "commitencoding")?.is_some() {
        bad(
            "i18n.commitEncoding is set, and this shell writes commits without an encoding header.".to_string(),
            "Commit with real git, outside this shell, or unset it: git config --unset i18n.commitEncoding.".to_string(),
        )?;
    }
    if let Some(v) = config_last(repo, "commit", "cleanup")? {
        if !matches!(v.to_ascii_lowercase().as_str(), "default" | "whitespace") {
            bad(
                format!("commit.cleanup is '{v}', and this shell cleans a message in git's whitespace mode only."),
                "Commit with real git, outside this shell, or unset it: git config --unset commit.cleanup.".to_string(),
            )?;
        }
    }
    if config_last(repo, "core", "logallrefupdates")?.is_some_and(|v| matches!(v.to_ascii_lowercase().as_str(), "false" | "no" | "off" | "0" | "never")) {
        bad(
            "core.logAllRefUpdates is off, and this shell always writes the reflog.".to_string(),
            "Commit with real git, outside this shell, or turn it on: git config core.logAllRefUpdates true.".to_string(),
        )?;
    }
    Ok(())
}

/// A hook file git would run: a regular file with an execute bit, or a link
/// (which this shell does not follow), and not a `.sample`.
pub(crate) fn hook_present(dir: &Path, name: &str) -> bool {
    use std::os::unix::fs::PermissionsExt;
    match std::fs::symlink_metadata(dir.join(name)) {
        Ok(m) if m.file_type().is_symlink() => true,
        Ok(m) => m.is_file() && m.permissions().mode() & 0o111 != 0,
        Err(_) => false,
    }
}

fn scan_hooks(repo: &WriteRepo, command: &str, writes_index: bool, amend: bool) -> Result<(std::path::PathBuf, Vec<String>), GitError> {
    let dir = hooks_dir(repo, command)?;
    let mut names: Vec<&str> = COMMIT_HOOKS.to_vec();
    if amend {
        names.extend_from_slice(AMEND_HOOKS);
    }
    if writes_index {
        names.push("post-index-change");
    }
    let present = names.into_iter().filter(|n| hook_present(&dir, n)).map(str::to_string).collect();
    Ok((dir, present))
}

fn refuse_hooks(command: &str, present: &[String], no_verify: bool) -> Result<(), GitError> {
    let Some(first) = present.first() else { return Ok(()) };
    let list = present.join(", ");
    if no_verify {
        return refuse(
            4,
            command,
            format!("--no-verify cannot skip the hooks here: the repository has {list}, and this shell runs no hooks, so skipping would look like running them."),
            "Commit with real git, outside this shell: git commit --no-verify -m '<message>', or ask the embedder for a hook provider.",
            Override::Embedder,
        );
    }
    refuse(
        4,
        command,
        format!("The repository has a {first} hook{}, and this shell runs no hooks.", if present.len() > 1 { format!(" (all present: {list})") } else { String::new() }),
        "Commit with real git, outside this shell: git commit -m '<message>', or ask the embedder for a hook provider.",
        Override::Embedder,
    )
}

/// Plan the index change and the tree for this commit from `state`.
fn prepare(
    repo: &WriteRepo,
    state: &gix_index::State,
    mtime: Option<filetime::FileTime>,
    policy: &StagePolicy,
    head_map: &Map,
    opts: &CommitOptions,
) -> Result<Prepared, GitError> {
    let cmd = opts.command.as_str();
    if !opts.specs.is_empty() {
        let plan = build_only(repo, state, policy, head_map, opts)?;
        let mut items = head_map.clone();
        overlay(&mut items, &plan);
        return Ok(Prepared { plan, items });
    }
    let mut items: Map = state
        .entries()
        .iter()
        .filter(|e| e.stage() == gix_index::entry::Stage::Unconflicted)
        // Intent-to-add entries are not committed: git's write-tree skips them.
        .filter(|e| !e.flags.contains(Flags::INTENT_TO_ADD))
        .map(|e| (e.path(state).to_owned(), (e.id, e.mode)))
        .collect();
    let plan = if opts.all {
        add::build(
            repo,
            state,
            mtime,
            policy,
            &AddOptions { specs: Vec::new(), all: false, update: true, force: false, command: cmd.to_string() },
        )?
    } else {
        Plan::default()
    };
    overlay(&mut items, &plan);
    Ok(Prepared { plan, items })
}

/// `--only`: the matched paths' working-tree content on top of HEAD's tree.
fn build_only(
    repo: &WriteRepo,
    state: &gix_index::State,
    policy: &StagePolicy,
    head_map: &Map,
    opts: &CommitOptions,
) -> Result<Plan, GitError> {
    let cmd = opts.command.as_str();
    let mut specs = SpecSet::new(OP, &opts.specs)?;
    let tracked = Tracked::all(state);
    // Known paths: the index's and HEAD's, as git's `list_paths` overlays
    // HEAD's tree on the index to match against.
    let mut known: BTreeMap<&[u8], Option<Mode>> = BTreeMap::new();
    for (p, (_, m)) in head_map {
        known.insert(p.as_slice(), Some(*m));
    }
    for t in &tracked {
        known.insert(t.path.as_slice(), Some(t.mode));
    }
    let mut plan = Plan::default();
    let mut attrs = AttributeReader::new(repo, cmd);
    for (path, mode) in &known {
        if !specs.matches(path) {
            continue;
        }
        let tr = tracked.iter().find(|t| t.path.as_slice() == *path);
        if tr.is_some_and(|t| t.flags.contains(Flags::SKIP_WORKTREE) || t.flags.contains(Flags::ASSUME_VALID)) {
            continue;
        }
        let abs = match repo.check_worktree_any(os(path)) {
            Ok(p) => p,
            Err(GitError::WriteThroughLink { found: "not a directory", .. }) => {
                plan.removals.push((*path).into());
                continue;
            }
            Err(e) => return Err(e),
        };
        let meta = match std::fs::symlink_metadata(&abs) {
            Ok(m) => m,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                plan.removals.push((*path).into());
                continue;
            }
            Err(e) => return Err(GitError::repository(OP, "reading a working-tree path", &abs, e)),
        };
        // A gitlink whose path is now a file or a link is committed as one.
        if *mode == Some(Mode::COMMIT) && meta.is_dir() {
            if let Some(id) = add::gitlink_head(repo, &abs)? {
                let stat = gix_index::fs::Metadata::from_path_no_follow(&abs)
                    .ok()
                    .and_then(|m| gix_index::entry::Stat::from_fs(&m).ok())
                    .unwrap_or_default();
                plan.gitlinks.push(((*path).into(), id, stat));
            }
            continue;
        }
        if meta.is_dir() {
            plan.removals.push((*path).into());
            continue;
        }
        plan.files.push(stage::inspect(repo, &mut attrs, policy, cmd, path, *mode)?);
    }
    if let Some(missing) = specs.unmatched().first() {
        return refuse(
            1,
            cmd,
            format!("The pathspec '{missing}' did not match any file known to git, and --only commits tracked paths."),
            format!("Stage it first, then commit: git add {missing}, then git commit -m '<message>'."),
            Override::Repository,
        );
    }
    plan.removals.sort();
    Ok(plan)
}

/// Apply a plan to a path map, with the replacements the index applies when a
/// file becomes a directory or the reverse.
fn overlay(map: &mut Map, plan: &Plan) {
    for p in &plan.removals {
        map.remove(p);
    }
    let adds = plan
        .files
        .iter()
        .map(|s| (s.rel.as_slice(), s.id, s.mode))
        .chain(plan.gitlinks.iter().map(|(p, id, _)| (p.as_slice(), *id, Mode::COMMIT)));
    for (path, id, mode) in adds {
        let mut cur = path;
        while let Some(i) = cur.iter().rposition(|b| *b == b'/') {
            cur = &cur[..i];
            map.remove(cur);
        }
        let mut below: RelPath = path.into();
        below.push(b'/');
        let doomed: Vec<RelPath> = map.range(below.clone()..).take_while(|(k, _)| k.starts_with(&below)).map(|(k, _)| k.clone()).collect();
        for k in doomed {
            map.remove(&k);
        }
        map.insert(path.into(), (id, mode));
    }
}

/// Write the planned blobs and put the planned entries in the locked index.
fn apply_to_index(repo: &WriteRepo, writer: &mut BlobWriter<'_>, tx: &mut IndexTx, plan: &Plan) -> Result<(), GitError> {
    let mut upserts = Vec::new();
    for staged in &plan.files {
        let bytes = stage::reread(repo, staged)?;
        let id = writer.write(&bytes)?;
        upserts.push(Upsert { path: staged.rel.clone(), stat: staged.stat, id, mode: staged.mode });
    }
    for (path, id, stat) in &plan.gitlinks {
        upserts.push(Upsert { path: path.clone(), stat: *stat, id: *id, mode: Mode::COMMIT });
    }
    for path in &plan.removals {
        indexfile::remove(tx.state_mut(), path);
    }
    indexfile::upsert_all(tx.state_mut(), upserts);
    Ok(())
}

struct Summary {
    text: String,
    files: usize,
    insertions: u64,
    deletions: u64,
}

/// The lines git prints after the first one: the shortstat and the mode
/// lines. A rename shows as a delete and a create.
fn summarize(repo: &WriteRepo, before: &Map, after: &Map, err: &mut String) -> Result<Summary, GitError> {
    use crate::diffcore::{line_delta, LineDelta, Side};
    let mut paths: Vec<&RelPath> = before.keys().chain(after.keys()).collect();
    paths.sort();
    paths.dedup();
    let (mut files, mut ins, mut del) = (0usize, 0u64, 0u64);
    let mut modes = String::new();
    let mut over: Vec<String> = Vec::new();
    for path in paths {
        let (old, new) = (before.get(path), after.get(path));
        let shown = show(path);
        if old == new {
            continue;
        }
        files += 1;
        let side = |e: Option<&(ObjectId, Mode)>| match e {
            None => Side::Absent,
            Some((_, m)) if *m == Mode::COMMIT => Side::Gitlink,
            Some((id, _)) => Side::Object(*id),
        };
        match (old, new) {
            (None, Some((_, m))) => modes.push_str(&format!(" create mode {:06o} {shown}\n", m.bits())),
            (Some((_, m)), None) => modes.push_str(&format!(" delete mode {:06o} {shown}\n", m.bits())),
            (Some((_, a)), Some((_, b))) if a != b => {
                modes.push_str(&format!(" mode change {:06o} => {:06o} {shown}\n", a.bits(), b.bits()));
            }
            _ => {}
        }
        // A gitlink is one line, "Subproject commit <oid>", on each side.
        let is_link = |e: Option<&(ObjectId, Mode)>| e.is_some_and(|(_, m)| *m == Mode::COMMIT);
        if is_link(old) || is_link(new) {
            if old.is_some() && is_link(old) {
                del += 1;
            }
            if new.is_some() && is_link(new) {
                ins += 1;
            }
            continue;
        }
        if old.map(|e| e.0) == new.map(|e| e.0) {
            continue;
        }
        match line_delta(repo.read(), OP, side(old), side(new), SUMMARY_BLOB_CAP)? {
            LineDelta::Counted { added, deleted } => {
                ins += added;
                del += deleted;
            }
            LineDelta::OverCap => over.push(shown),
            LineDelta::Binary | LineDelta::Gitlink => {}
        }
    }
    if !over.is_empty() {
        note(
            err,
            &format!(
                "git commit: the line counts leave out {} (a side is over {SUMMARY_BLOB_CAP} bytes).",
                over.join(", ")
            ),
        );
    }
    let mut text = String::new();
    if files > 0 {
        text.push_str(&format!(" {files} file{} changed", if files == 1 { "" } else { "s" }));
        if ins > 0 || del == 0 {
            text.push_str(&format!(", {ins} insertion{}(+)", if ins == 1 { "" } else { "s" }));
        }
        if del > 0 || ins == 0 {
            text.push_str(&format!(", {del} deletion{}(-)", if del == 1 { "" } else { "s" }));
        }
        text.push('\n');
    }
    text.push_str(&modes);
    Ok(Summary { text, files, insertions: ins, deletions: del })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn overlay_replaces_a_file_with_a_directory_and_back() {
        let id = ObjectId::null(gix_index::hash::Kind::Sha1);
        let mut map: Map = BTreeMap::new();
        map.insert("a".into(), (id, Mode::FILE));
        map.insert("d/x".into(), (id, Mode::FILE));
        let plan = Plan {
            files: vec![
                stage::Staged { rel: "a/inner".into(), mode: Mode::FILE, stat: Default::default(), id },
                stage::Staged { rel: "d".into(), mode: Mode::FILE, stat: Default::default(), id },
            ],
            ..Plan::default()
        };
        overlay(&mut map, &plan);
        let keys: Vec<String> = map.keys().map(|k| show(k)).collect();
        assert_eq!(keys, ["a/inner", "d"]);
    }
}
