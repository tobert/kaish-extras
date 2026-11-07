//! `git var GIT_AUTHOR_IDENT` and `git var GIT_COMMITTER_IDENT`
//! (git-write-profile.md §4, §5.4): the identity the next commit will use, in
//! git's `Name <email> <seconds> <+hhmm>` form.
//!
//! It resolves identity exactly as `commit` does, from the same inputs, so
//! the line it prints is the line the next commit writes, apart from the
//! clock when no date is set.

use clap::Parser;
use kaish_tool_api::GlobalFlags;

use crate::config::Identity;
use crate::error::GitError;
use crate::verbs::write::ident::{self, KernelVars};
use crate::verbs::write::refusal::{refuse, Override};
use crate::verbs::write::spec::Outcome;

/// `git var`'s argv surface.
#[derive(Parser, Debug)]
#[command(
    name = "var",
    about = "Print the identity the next commit will use: GIT_AUTHOR_IDENT or GIT_COMMITTER_IDENT, as 'Name <email> <seconds> <+hhmm>'. Read from the pinned identity or the GIT_AUTHOR_* and GIT_COMMITTER_* variables, never from a config file"
)]
pub(crate) struct VarArgs {
    #[command(flatten)]
    pub global: GlobalFlags,

    /// GIT_AUTHOR_IDENT or GIT_COMMITTER_IDENT.
    #[arg(value_name = "VARIABLE")]
    pub name: Option<String>,
}

pub(crate) fn run(
    command: &str,
    name: Option<&str>,
    pinned: Option<&(Identity, Identity)>,
    vars: &KernelVars,
) -> Result<Outcome, GitError> {
    let Some(name) = name else {
        return refuse(
            2,
            command,
            "No variable was named.",
            "Name one: git var GIT_AUTHOR_IDENT.",
            Override::Never,
        );
    };
    let want_author = match name {
        "GIT_AUTHOR_IDENT" => true,
        "GIT_COMMITTER_IDENT" => false,
        other => {
            return refuse(
                2,
                command,
                format!("This shell answers git var for GIT_AUTHOR_IDENT and GIT_COMMITTER_IDENT only, not '{other}'."),
                "Ask for one of those: git var GIT_AUTHOR_IDENT.",
                Override::Never,
            )
        }
    };
    let idents = ident::resolve(command, pinned, vars, None, ident::now())?;
    let sig = if want_author { idents.author } else { idents.committer };
    let line = sig.ident_line();
    Ok(Outcome {
        text: format!("{line}\n"),
        err: String::new(),
        json: serde_json::json!({ "variable": name, "ident": line, "name": sig.name, "email": sig.email, "seconds": sig.seconds, "offset_seconds": sig.offset }),
    })
}
