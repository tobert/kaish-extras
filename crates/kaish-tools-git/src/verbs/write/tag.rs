//! `git tag <name> [<rev>]` and `git tag -a <name> -m <msg> [<rev>]`
//! (git-write-profile.md §4, §6).
//!
//! A lightweight tag is a ref at the object `<rev>` names. An annotated tag is
//! a tag object, written byte for byte as git writes it, and a ref at the tag
//! object. Git makes an annotated tag for `-m` and `-F` too, with or without
//! `-a`. The tagger is the committer identity, resolved as a commit's is.
//!
//! The message is cleaned as git's `tag` cleans it, which is not `commit`'s
//! cleanup: `#` lines are comments and are dropped (checked with `-m`, `-F`
//! and an editor-less run against git 2.56). An empty message is allowed.
//! The object is encoded here and not by `gix-object`, whose tag encoder
//! leaves out the blank line git writes before an empty message.
//!
//! A tag has no reflog: `core.logAllRefUpdates` at its default logs branches,
//! remote-tracking branches, notes and HEAD, and gix-ref keeps the same rule.
//! `always` would log tags, so it refuses here.
//!
//! The object is written, then synced, then the ref is created; a refusal
//! after the object is on disk says so (`Wrote::UnreachableObjects`).

use clap::Parser;
use gix_index::hash::ObjectId;
use gix_object::Find as _;

use crate::error::GitError;
use crate::verbs::tag::TagArgs;
use crate::verbs::write::branch::{blocked_refusal, scan_ref_hooks, RefsEnv};
use crate::verbs::write::hookrun::{self, Tx};
use crate::verbs::write::ident::{self, KernelVars};
use crate::verbs::write::objects::BlobWriter;
use crate::verbs::write::refmove::{self, MoveKind};
use crate::verbs::write::refname::{self, RefKind};
use crate::verbs::write::refusal::{refuse, Override};
use crate::verbs::write::repo::WriteRepo;
use crate::verbs::write::spec::Outcome;
use crate::verbs::write::state::{config_bool, config_last};

const OP: &str = "tag";

/// `git tag` under the commit profile: the listing flags, plus the creation
/// flags and the spellings that are refused with a next step.
#[derive(Parser, Debug)]
#[command(
    name = "tag",
    about = "List the repository's tags, or create one with git tag <name> [<rev>] (lightweight) or git tag -a <name> -m <message> [<rev>] (annotated). Creates only: it never deletes, moves or signs a tag"
)]
pub(crate) struct TagWriteArgs {
    #[command(flatten)]
    pub read: TagArgs,

    #[command(flatten)]
    pub extra: TagExtra,
}

/// The flags beyond the listing ones.
#[derive(clap::Args, Debug)]
pub(crate) struct TagExtra {
    /// Create an annotated tag: a tag object with a message and the tagger.
    /// Needs -m or -F, because there is no editor. -m or -F alone make an
    /// annotated tag too, as git does.
    #[arg(short = 'a', long = "annotate")]
    pub annotate: bool,

    /// The tag message. Repeat the flag for more paragraphs. Cleaned as git
    /// cleans a tag message: trailing whitespace off every line, lines that
    /// start with `#` dropped, blank-line runs collapsed, leading and
    /// trailing blank lines dropped. An empty message is allowed.
    #[arg(short = 'm', long = "message", value_name = "MSG")]
    pub message: Vec<String>,

    /// Read the tag message from this file (a path in this shell's
    /// filesystem), cleaned the same way as -m. Not with -m.
    #[arg(short = 'F', long = "file", value_name = "FILE")]
    pub file: Option<String>,

    /// Refused: this shell never deletes a tag.
    #[arg(short = 'd', long = "delete")]
    pub delete: bool,

    /// Refused: this shell never moves a tag that exists.
    #[arg(short = 'f', long = "force")]
    pub force: bool,

    /// Refused: this shell never signs.
    #[arg(short = 's', long = "sign")]
    pub sign: bool,

    /// Refused: this shell never signs.
    #[arg(short = 'u', long = "local-user", value_name = "KEYID")]
    pub local_user: Option<String>,
}

impl TagExtra {
    /// Whether a creation flag was given.
    pub(crate) fn creates(&self) -> bool {
        self.annotate || !self.message.is_empty() || self.file.is_some()
    }

    /// The refusals that need no repository.
    pub(crate) fn refusal(&self, command: &str) -> Option<GitError> {
        let (why, next): (&str, &str) = if self.delete {
            (
                "git tag -d deletes a tag, and this shell creates tags only.",
                "List the tags with git tag. To delete one, use real git, outside this shell: git tag -d <name>.",
            )
        } else if self.force {
            (
                "git tag -f moves a tag that exists, and a moved tag can mislead whoever fetched the old one. This shell creates tags only.",
                "Create a tag under a new name: git tag <new-name> [<rev>].",
            )
        } else if self.sign || self.local_user.is_some() {
            (
                "This shell never signs a tag.",
                "Create an unsigned annotated tag: git tag -a <name> -m '<message>'. A signed tag needs real git, outside this shell.",
            )
        } else if self.annotate && self.message.is_empty() && self.file.is_none() {
            (
                "There is no editor in this shell, so an annotated tag needs its message on the command line.",
                "Pass the message: git tag -a <name> -m '<message>'.",
            )
        } else if !self.message.is_empty() && self.file.is_some() {
            ("-m and -F both give the message.", "Use one of them.")
        } else {
            return None;
        };
        Some(refuse::<()>(2, command, why, next, Override::Never).unwrap_err())
    }
}

pub(crate) struct TagCreate {
    pub(crate) name: String,
    pub(crate) target: Option<String>,
    /// The message as given, before cleanup; `None` makes a lightweight tag.
    pub(crate) raw_message: Option<String>,
    pub(crate) vars: KernelVars,
    pub(crate) command: String,
}

/// git's `strbuf_stripspace` with comment stripping, the cleanup a tag
/// message gets: a line that starts with `#` goes, trailing whitespace goes
/// from every other line, a run of blank lines becomes one, the blank lines at
/// both ends go, and one `\n` ends what is left.
///
/// Whitespace is git's own `isspace`: space, tab, newline and carriage
/// return. A form feed or a vertical tab stays (checked against git 2.56;
/// `commit`'s cleanup agrees).
pub(crate) fn cleanup(raw: &str) -> String {
    let mut out = String::new();
    let mut pending_blank = false;
    for line in raw.split('\n') {
        if line.starts_with('#') {
            continue;
        }
        let line = line.trim_end_matches([' ', '\t', '\r']);
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

/// Settings that would make this shell's tag differ from git's.
fn ensure_tag_settings(repo: &WriteRepo, command: &str, annotated: bool) -> Result<(), GitError> {
    let bad = |what: String, fix: String| refuse::<()>(4, command, what, fix, Override::Repository);
    if config_bool(repo, command, "tag", "gpgsign", "tag.gpgSign")? == Some(true) {
        bad(
            "tag.gpgSign is true in this repository, and this shell never signs.".to_string(),
            "Create the tag with real git, outside this shell, or turn signing off for this repository: git config tag.gpgSign false.".to_string(),
        )?;
    }
    if annotated && config_bool(repo, command, "tag", "forcesignannotated", "tag.forceSignAnnotated")? == Some(true) {
        bad(
            "tag.forceSignAnnotated is true in this repository, and this shell never signs.".to_string(),
            "Create the tag with real git, outside this shell, or turn it off for this repository: git config tag.forceSignAnnotated false.".to_string(),
        )?;
    }
    if config_last(repo, "core", "logallrefupdates")?.is_some_and(|v| v.eq_ignore_ascii_case("always")) {
        bad(
            "core.logAllRefUpdates is 'always', which makes git write a reflog for a tag, and this shell writes none.".to_string(),
            "Create the tag with real git, outside this shell, or set it to true: git config core.logAllRefUpdates true.".to_string(),
        )?;
    }
    // `#` and `auto` leave a `-m` message cleaned as this shell cleans it
    // (checked against git: `auto` chooses a character for an editor buffer
    // only, and a message given with -m or -F drops its `#` lines).
    for (key, shown) in [("commentchar", "core.commentChar"), ("commentstring", "core.commentString")] {
        if annotated && config_last(repo, "core", key)?.is_some_and(|v| v != "#" && v != "auto") {
            bad(
                format!("{shown} is set to something other than '#' or 'auto', and this shell drops comment lines from a tag message for '#' only."),
                format!("Create the tag with real git, outside this shell, or unset it: git config --unset {shown}."),
            )?;
        }
    }
    Ok(())
}

pub(crate) fn run(repo: &WriteRepo, env: &RefsEnv, opts: &TagCreate) -> Result<Outcome, GitError> {
    let cmd = opts.command.as_str();
    let full = refname::full_name(RefKind::Tag, cmd, &opts.name)?;
    let annotated = opts.raw_message.is_some();
    ensure_tag_settings(repo, cmd, annotated)?;
    let scan = scan_ref_hooks(repo, env.hooks.is_some(), cmd, &["reference-transaction"], "tag")?;

    if refname::exists(repo, &full)?.is_some() {
        return refuse(
            1,
            cmd,
            format!("A tag named '{}' already exists.", opts.name),
            format!("Choose another name, for example: git tag {}-2. To see what '{}' names: git show {}. Moving a tag takes real git, outside this shell.", opts.name, opts.name, opts.name),
            Override::Never,
        );
    }
    if let Some(why) = refname::blocked_by(repo, &full)? {
        return Err(blocked_refusal(cmd, "tag", &opts.name, &why));
    }

    let target = match &opts.target {
        Some(spec) => repo.read().resolve_object_expr(spec)?,
        None => {
            let head = repo.read().head()?;
            match head.oid.as_deref() {
                Some(hex) => ObjectId::from_hex(hex.as_bytes()).map_err(|e| GitError::repository(OP, "reading HEAD", repo.git_dir(), e))?,
                None => {
                    return refuse(
                        1,
                        cmd,
                        "HEAD has no commits yet, so there is nothing to tag.",
                        "Make the first commit, then tag it: git commit -m '<message>'.",
                        Override::Never,
                    )
                }
            }
        }
    };
    refmove::precheck_ref(repo, cmd, &full)?;

    let ctx = scan.context(repo);
    let mut err = String::new();
    // Creates the tag under the hook's transaction. `objects` says a tag
    // object was written first, so a refusal or failure names it.
    let create = |oid: ObjectId, objects: bool, err: &mut String| -> Result<(), GitError> {
        let after = |e: GitError| if objects { e.after_objects() } else { e };
        let tx = Tx::new(hookrun::null(), oid.to_string(), full.clone());
        if let Some(h) = env.hooks.as_ref() {
            hookrun::prepared(h.as_ref(), &ctx, MoveKind::TAG, cmd, std::slice::from_ref(&tx), err).map_err(after)?;
        }
        if let Err(e) = refmove::create_ref(repo, MoveKind::TAG, cmd, &full, oid, None) {
            if let Some(h) = env.hooks.as_ref() {
                hookrun::aborted(h.as_ref(), &ctx, &tx, err);
            }
            return Err(after(e));
        }
        if let Some(h) = env.hooks.as_ref() {
            hookrun::committed(h.as_ref(), &ctx, &tx, err);
        }
        Ok(())
    };

    let Some(raw) = &opts.raw_message else {
        create(target, false, &mut err)?;
        return Ok(outcome(opts, &full, target, target, "lightweight", err));
    };

    let tagger = ident::resolve_committer(cmd, env.identity.as_ref(), &opts.vars, ident::now())?;
    let mut buf = Vec::new();
    let kind = repo
        .objects()
        .try_find(&target, &mut buf)
        .map_err(|e| GitError::repository(OP, "reading the tagged object", repo.git_dir(), std::io::Error::other(e.to_string())))?
        .ok_or_else(|| GitError::NoSuchRevision {
            operation: OP,
            rev: opts.target.clone().unwrap_or_else(|| target.to_string()),
            repo: repo.git_dir().to_path_buf(),
        })?
        .kind;
    let body = cleanup(raw);
    let mut bytes = format!(
        "object {target}\ntype {kind}\ntag {}\ntagger {}\n\n",
        opts.name,
        tagger.ident_line()
    )
    .into_bytes();
    bytes.extend_from_slice(body.as_bytes());
    let mut writer = BlobWriter::new(repo, OP);
    let tag_id = writer.write_object(gix_object::Kind::Tag, &bytes)?;
    writer.sync().map_err(GitError::after_objects)?;
    create(tag_id, true, &mut err)?;
    Ok(outcome(opts, &full, tag_id, target, "annotated", err))
}

fn outcome(opts: &TagCreate, full: &str, oid: ObjectId, target: ObjectId, kind: &str, err: String) -> Outcome {
    Outcome {
        text: String::new(),
        err,
        json: serde_json::json!({ "tag": opts.name, "ref": full, "object": oid.to_string(), "target": target.to_string(), "kind": kind }),
    }
}
