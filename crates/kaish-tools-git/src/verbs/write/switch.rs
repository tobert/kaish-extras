//! `git switch -c <name>` and `git checkout -b <name>` (git-write-profile.md
//! §4, §6): create a branch at HEAD and point HEAD at it.
//!
//! Neither the working tree nor the index changes, because the new branch is
//! at the commit HEAD is already on. That is why a start point other than HEAD
//! is allowed only when it names that same commit: any other start would
//! check out different files (W6), and this shell never writes file content.
//! Git treats a start at HEAD's own commit as no change to the files too
//! (checked: the working tree, including local edits, stays as it is).
//!
//! The writes, in git's order, under one `HEAD.lock`: the new branch with the
//! reflog line `branch: Created from <start as typed>` (`HEAD` when none),
//! `HEAD` rewritten to `ref: refs/heads/<name>`, and the line
//! `checkout: moving from <old> to <new>` appended to `logs/HEAD`, where
//! `<old>` is the branch's short name or, for a detached HEAD, the full oid.
//! gix-ref writes no `HEAD` line for a symbolic update, and its `deref: true`
//! edit can loop forever (docs/issues.md W13), so `refmove::switch_head`
//! writes `HEAD` itself, as `reset` and `commit` do.
//!
//! An unborn HEAD has no commit to branch from. Git then only points `HEAD`
//! at the new name and writes no reflog; this does the same.

use std::cell::Cell;

use clap::Parser;
use kaish_tool_api::GlobalFlags;

use crate::error::GitError;
use crate::hooks::RefUpdate;
use crate::verbs::write::hookrun::{self, Tx};
use crate::verbs::write::branch::{
    blocked_refusal, exists_refusal, scan_ref_hooks, tracking_implied, tracking_refusal, unborn_refusal, RefsEnv,
};
use crate::verbs::write::ident::{self, KernelVars};
use crate::verbs::write::refmove::{self, HeadSwitch, MoveKind};
use crate::verbs::write::refname::{self, RefKind};
use crate::verbs::write::refusal::{refuse, Override};
use crate::verbs::write::repo::WriteRepo;
use crate::verbs::write::spec::Outcome;
use crate::verbs::write::state::{ensure_idle, ensure_reflog_on};

/// `git switch`'s argv surface.
#[derive(Parser, Debug)]
#[command(
    name = "switch",
    about = "Create a branch at HEAD and switch to it: git switch -c <name>. HEAD then names the new branch, and no file in the working tree or the index changes. Switching to a branch that exists is refused, because it would write files"
)]
pub(crate) struct SwitchArgs {
    /// Create this branch at HEAD and switch to it. A start point may follow
    /// the name only when it names the commit HEAD is on; any other start
    /// would change the working tree, and this shell never writes file content.
    #[arg(short = 'c', long = "create", value_name = "NAME")]
    pub create: Option<String>,

    /// Refused: this shell never moves a branch that exists.
    #[arg(short = 'C', long = "force-create", value_name = "NAME")]
    pub force_create: Option<String>,

    /// Refused: it would write files.
    #[arg(short = 'd', long = "detach")]
    pub detach: bool,

    /// Refused: it would write files.
    #[arg(long = "orphan", value_name = "NAME")]
    pub orphan: Option<String>,

    /// Refused: it would write files.
    #[arg(short = 'f', long = "force", visible_alias = "discard-changes")]
    pub force: bool,

    /// Refused: it would write files.
    #[arg(short = 'm', long = "merge")]
    pub merge: bool,

    /// Refused: this shell never writes branch configuration.
    #[arg(short = 't', long = "track", num_args = 0..=1, require_equals = true, value_name = "MODE")]
    pub track: Option<Option<String>>,

    /// Create the branch without upstream tracking.
    #[arg(long = "no-track")]
    pub no_track: bool,

    /// Print nothing on success.
    #[arg(short = 'q', long = "quiet")]
    pub quiet: bool,

    /// Repository to change. Defaults to the current directory; discovery
    /// searches upward, never past the mount root that contains the path.
    #[arg(long = "repo", value_name = "PATH")]
    pub repo: Option<String>,

    #[command(flatten)]
    pub global: GlobalFlags,

    /// A start point after `-c <name>`, or the branch to switch to (refused).
    #[arg(hide = true)]
    pub operands: Vec<String>,
}

/// `git checkout`'s argv surface: `-b` only.
#[derive(Parser, Debug)]
#[command(
    name = "checkout",
    about = "Create a branch at HEAD and switch to it: git checkout -b <name>, the same as git switch -c. Checking out a branch or a path is refused, because it would write files"
)]
pub(crate) struct CheckoutArgs {
    /// Create this branch at HEAD and switch to it. A start point may follow
    /// the name only when it names the commit HEAD is on; any other start
    /// would change the working tree, and this shell never writes file content.
    #[arg(short = 'b', value_name = "NAME")]
    pub create: Option<String>,

    /// Refused: this shell never moves a branch that exists.
    #[arg(short = 'B', value_name = "NAME")]
    pub force_create: Option<String>,

    /// Refused: it would write files.
    #[arg(short = 'd', long = "detach")]
    pub detach: bool,

    /// Refused: it would write files.
    #[arg(long = "orphan", value_name = "NAME")]
    pub orphan: Option<String>,

    /// Refused: it would write files.
    #[arg(short = 'f', long = "force")]
    pub force: bool,

    /// Refused: it would write files.
    #[arg(short = 'm', long = "merge")]
    pub merge: bool,

    /// Refused: choosing hunks needs a terminal, and checking out hunks writes files.
    #[arg(short = 'p', long = "patch")]
    pub patch: bool,

    /// Refused: this shell never writes branch configuration.
    #[arg(short = 't', long = "track", num_args = 0..=1, require_equals = true, value_name = "MODE")]
    pub track: Option<Option<String>>,

    /// Create the branch without upstream tracking.
    #[arg(long = "no-track")]
    pub no_track: bool,

    /// Print nothing on success.
    #[arg(short = 'q', long = "quiet")]
    pub quiet: bool,

    /// Repository to change. Defaults to the current directory; discovery
    /// searches upward, never past the mount root that contains the path.
    #[arg(long = "repo", value_name = "PATH")]
    pub repo: Option<String>,

    #[command(flatten)]
    pub global: GlobalFlags,

    /// A start point after `-b <name>`, or the branch or paths to check out (refused).
    #[arg(hide = true)]
    pub operands: Vec<String>,
}

/// The flags both verbs refuse, as one set.
pub(crate) struct Refused<'a> {
    pub(crate) verb: &'a str,
    pub(crate) create_flag: &'a str,
    pub(crate) force_create: Option<&'a String>,
    pub(crate) detach: bool,
    pub(crate) orphan: Option<&'a String>,
    pub(crate) force: bool,
    pub(crate) merge: bool,
    pub(crate) patch: bool,
    pub(crate) track: bool,
    pub(crate) create: Option<&'a String>,
}

impl Refused<'_> {
    /// The refusal for a flag, or for the missing create flag, with the next
    /// step.
    pub(crate) fn refusal(&self, command: &str) -> Option<GitError> {
        let v = self.verb;
        let c = self.create_flag;
        let files = |what: &str| format!("{what} writes files into the working tree, and this shell never writes file content.");
        let new = format!("To start a branch at HEAD: git {v} {c} <name>. To see where HEAD is: git branch --show-current.");
        let (why, next): (String, String) = if self.patch {
            (
                "git checkout -p asks questions on a terminal, and this shell has none.".to_string(),
                "Unstage whole files instead: git restore --staged <path>.".to_string(),
            )
        } else if self.force_create.is_some() {
            (
                format!("git {v} {} resets a branch that exists, which moves it and can drop commits, and this shell makes new branches only.", if v == "switch" { "-C" } else { "-B" }),
                format!("Make a new branch: git {v} {c} <name>. To make one at another commit without switching to it: git branch <name> <start>."),
            )
        } else if self.detach {
            (files(&format!("git {v} --detach")), new)
        } else if self.orphan.is_some() {
            (files(&format!("git {v} --orphan")), new)
        } else if self.force {
            (files(&format!("git {v} -f")), new)
        } else if self.merge {
            (files(&format!("git {v} -m")), new)
        } else if self.track {
            (
                "Upstream tracking is written into the repository configuration, and this shell never writes configuration.".to_string(),
                format!("Make the branch without tracking: git {v} {c} <name>."),
            )
        } else if self.create.is_none() {
            (
                if v == "switch" {
                    files("git switch <branch>")
                } else {
                    files("git checkout <branch> or <path>")
                },
                format!("{new} To unstage a path: git restore --staged <path>."),
            )
        } else {
            return None;
        };
        Some(refuse::<()>(2, command, why, next, Override::Never).unwrap_err())
    }
}

pub(crate) struct SwitchCreate {
    /// `switch` or `checkout`, for errors.
    pub(crate) op: &'static str,
    pub(crate) name: String,
    pub(crate) start: Option<String>,
    pub(crate) no_track: bool,
    pub(crate) quiet: bool,
    pub(crate) vars: KernelVars,
    pub(crate) command: String,
}

pub(crate) fn run(repo: &WriteRepo, env: &RefsEnv, opts: &SwitchCreate) -> Result<Outcome, GitError> {
    let cmd = opts.command.as_str();
    let kind = MoveKind { op: opts.op, ..MoveKind::SWITCH };
    let full = refname::full_name(RefKind::Branch, cmd, &opts.name)?;
    ensure_idle(repo, cmd, "switch branches")?;
    ensure_reflog_on(repo, cmd)?;
    let scan = scan_ref_hooks(repo, env.hooks.is_some(), cmd, &["reference-transaction", "post-checkout"], "switch")?;

    let head = repo.read().head()?;
    let head_oid = head
        .oid
        .as_deref()
        .map(|hex| {
            gix_index::hash::ObjectId::from_hex(hex.as_bytes())
                .map_err(|e| GitError::repository(opts.op, "reading HEAD", repo.git_dir(), e))
        })
        .transpose()?;
    let was = refmove::head_target(repo, opts.op)?;

    if refname::exists(repo, &full)?.is_some() {
        return Err(exists_refusal(cmd, &opts.name));
    }
    if let Some(why) = refname::blocked_by(repo, &full)? {
        return Err(blocked_refusal(cmd, "branch", &opts.name, &why));
    }
    let shown = opts.start.clone().unwrap_or_else(|| "HEAD".to_string());
    if let Some(start) = opts.start.as_deref() {
        // Git resolves a start of HEAD like any other, so on an unborn HEAD it
        // fails there ("invalid reference: HEAD") instead of switching.
        if head_oid.is_none() && matches!(start, "HEAD" | "@") {
            return Err(unborn_refusal(cmd, head.branch.as_deref()));
        }
        if !opts.no_track && tracking_implied(repo, start)? {
            return Err(tracking_refusal(cmd, start, &opts.name));
        }
        let oid = repo.read().resolve_commit(start)?;
        if Some(oid) != head_oid {
            return refuse(
                2,
                cmd,
                format!("'{start}' is not the commit HEAD is on, so switching to a branch there would change the files in the working tree, and this shell never writes file content."),
                format!("Make the branch without switching to it: git branch {} {start}.", opts.name),
                Override::Never,
            );
        }
    }
    let committer = match head_oid {
        Some(_) => Some(ident::resolve_committer(cmd, env.identity.as_ref(), &opts.vars, ident::now())?),
        None => None,
    };
    let message = format!("branch: Created from {shown}");
    let mut err = String::new();
    // Git's transactions: the new branch (when HEAD has a commit), then HEAD,
    // whose old value is the null oid and whose new value names the branch.
    let mut txs: Vec<Tx> = Vec::new();
    if let Some(oid) = head_oid {
        txs.push(vec![RefUpdate::new(hookrun::null(), oid.to_string(), full.clone())]);
    }
    txs.push(vec![RefUpdate::new(hookrun::null(), format!("ref:{full}"), "HEAD")]);
    let ctx = scan.context(repo);
    if let Some(h) = env.hooks.as_ref() {
        hookrun::prepared(h.as_ref(), &ctx, kind, cmd, &txs, &mut err)?;
    }
    let created = Cell::new(false);
    let switched = refmove::switch_head(
        repo,
        &HeadSwitch {
            kind,
            command: cmd,
            was: &was,
            head_oid,
            full: &full,
            create: committer.as_ref().map(|c| (message.as_str(), c)),
            created: Some(&created),
        },
    );
    if let Some(h) = env.hooks.as_ref() {
        let h = h.as_ref();
        match &switched {
            Ok(()) => txs.iter().for_each(|tx| hookrun::committed(h, &ctx, tx, &mut err)),
            Err(_) => {
                // The branch transaction is first, and it was written when
                // the branch exists; the HEAD one never was.
                let (done, rest) = if created.get() { txs.split_at(1) } else { txs.split_at(0) };
                done.iter().for_each(|tx| hookrun::committed(h, &ctx, tx, &mut err));
                rest.iter().for_each(|tx| hookrun::aborted(h, &ctx, tx, &mut err));
            }
        }
    }
    switched?;
    if let (Some(h), Some(oid)) = (env.hooks.as_ref(), head_oid) {
        let out = h.post_checkout(&ctx, &oid.to_string(), &oid.to_string(), true);
        hookrun::note(&mut err, &out.message);
    }
    if !opts.quiet {
        err.insert_str(0, &format!("Switched to a new branch '{}'\n", opts.name));
    }
    Ok(Outcome {
        text: String::new(),
        err,
        json: serde_json::json!({
            "branch": opts.name,
            "ref": full,
            "commit": head_oid.map(|o| o.to_string()),
            "previous": head.branch,
            "detached": head.detached,
        }),
    })
}
