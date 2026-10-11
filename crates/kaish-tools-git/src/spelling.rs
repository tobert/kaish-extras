//! Git's spellings this build refuses, and what to type instead.
//!
//! An agent types git's flags because git's flags are what it was trained on.
//! When a verb accepts the spelling, the clap struct carries it (`-a`,
//! `--cached`, `--oneline`); this module is the other half: a flag the verb
//! does not support is refused **saying that it is not supported**, with the
//! command to type instead whenever one exists. The next call should work.
//!
//! clap's own unknown-argument text is not fit for that reader. It suggests
//! `-- --oneline`, which here would turn the flag into a pathspec; it suggests
//! `--patch` for `--cached`, by edit distance; and it ends in `try '--help'`,
//! which this tool does not answer. So the message is rebuilt from clap's
//! error context rather than passed through.

use clap::error::{ContextKind, ContextValue, ErrorKind};

use crate::config::{GitConfig, Verb};
use crate::error::GitError;

/// git commands that change a repository. This build has none of them.
const WRITE_VERBS: &[&str] = &[
    "add", "am", "apply", "checkout", "cherry-pick", "clean", "clone", "commit", "fetch",
    "gc", "init", "merge", "mv", "notes", "pull", "push", "rebase", "reset", "restore",
    "revert", "rm", "sparse-checkout", "stash", "submodule", "switch", "update-index",
    "update-ref",
];

/// git read commands this build has no verb for, each with the sentences
/// that name a verb answering the same question. A sentence is shown only
/// when every verb it names is enabled ([`runnable`]): an embedder may
/// subtract any of them.
const READ_VERBS: &[(&str, &[&str])] = &[
    (
        "rev-parse",
        &[
            "'git info' reports the repository root and the commit HEAD is on.",
            "'git log -n 1 --json <rev>' resolves a revision to its full oid.",
        ],
    ),
    (
        "ls-files",
        &[
            "'git ls --recursive' lists every file in HEAD's tree.",
            "'git status' lists what the index and working tree change against it.",
        ],
    ),
    ("ls-tree", &["'git ls <rev> [<path>]' lists a tree; add --recursive for every file."]),
    ("blame", &["'git log --path <file>' lists the commits that changed a file, newest first."]),
    ("cat-file", &["'git show <rev>:<path>' prints a file as it was at a revision."]),
    ("remote", &["'git branch --remote' lists the remote-tracking branches a fetch left."]),
    ("describe", &["'git tag --contains <rev>' lists the tags whose history includes a commit."]),
    ("shortlog", &["'git log --json' carries each commit's author to count."]),
    ("reflog", &["'git log' walks history from a revision; the reflog is not read."]),
    ("grep", &["'git ls --recursive' lists the tracked files, and kaish's own grep searches them."]),
    ("config", &[]),
    ("whatchanged", &["'git log --stat' counts what each commit changed."]),
];

/// Whether every `git <verb>` a sentence names is a verb `config` enables. A
/// next step must work in the build that printed it, so a hint naming a
/// subtracted verb is not shown.
pub(crate) fn runnable(sentence: &str, config: &GitConfig) -> bool {
    sentence.match_indices("git ").all(|(i, _)| {
        let rest = &sentence[i + 4..];
        let named = Verb::ALL
            .iter()
            .filter(|v| {
                let word = v.as_str();
                rest.starts_with(word)
                    && !rest[word.len()..].starts_with(|c: char| c.is_alphanumeric() || c == '-')
            })
            .max_by_key(|v| v.as_str().len());
        // "this git is read-only" names no verb.
        named.is_none_or(|v| config.has(*v))
    })
}

/// The refusal for a git command this build has no verb for, or `None` when
/// the word is not a git command it knows.
pub(crate) fn unsupported_verb(tool: &str, word: &str, config: &GitConfig) -> Option<GitError> {
    let (mut reason, next): (String, &[&str]) = if WRITE_VERBS.contains(&word) {
        (
            "not supported: this git is read-only. It inspects the repository and \
             never stages, commits, switches or rewrites anything."
                .to_string(),
            &["'git status' lists what is changed.", "'git diff' shows each change."],
        )
    } else {
        let (_, next) = READ_VERBS.iter().find(|(name, _)| *name == word)?;
        ("not supported by this git.".to_string(), next)
    };
    for sentence in next.iter().filter(|s| runnable(s, config)) {
        reason.push(' ');
        reason.push_str(sentence);
    }
    Some(GitError::UnsupportedVerb {
        tool: tool.to_string(),
        verb: word.to_string(),
        reason,
    })
}

/// Flags git has that a verb here refuses, with the reason and the next step.
///
/// Every entry must name a flag the verb's parser really refuses — a hint for
/// an accepted flag is dead text. `tests::every_hint_names_a_flag_its_verb_refuses`
/// holds that, so adding a spelling to a verb means deleting its hint here.
///
/// Commands are quoted with `'…'`; a revision with `~` or `^` inside one is
/// double-quoted, because the kaish 0.17 parser refuses either character
/// unquoted in a word (`docs/issues.md`, K3).
const HINTS: &[(&str, &[&str], &str)] = &[
    (
        "log",
        &["--graph"],
        "This git draws no history graph. Each commit's parent oids are in \
         --json ('parents'), and --first-parent follows the mainline only",
    ),
    (
        "log",
        &["--decorate"],
        "This git does not label commits with ref names. 'git branch --all' \
         and 'git tag' list each ref beside the oid it points at",
    ),
    (
        "log",
        &["--all", "--branches", "--tags", "--remotes"],
        "This git's log walks from one revision. 'git branch --all' lists \
         every branch tip; name one with 'git log <branch>'",
    ),
    (
        "log",
        &["--pretty", "--format"],
        "This git has two text layouts, the default table and --oneline, and \
         --json carries every field by name",
    ),
    (
        "log",
        &["--grep"],
        "This git does not search commit messages. --author filters by \
         author, and --json carries each commit's 'summary' for a filter",
    ),
    (
        "log",
        &["--name-only", "--name-status"],
        "This git's log names no files. --stat counts each commit's changed \
         files, and 'git diff --name-only --from \"<commit>~1\" --to <commit>' \
         names them",
    ),
    (
        "branch",
        &["-v", "-vv", "--verbose"],
        "This git's listing shows each branch tip's oid but not its subject. \
         --ahead-behind adds the upstream counts -vv shows, and \
         'git log -n 1 <branch>' shows a tip's subject",
    ),
    (
        "branch",
        &[
            "-d", "-D", "--delete", "-m", "-M", "--move", "-c", "-C", "--copy",
            "-u", "--set-upstream-to", "--unset-upstream", "-f", "--force",
        ],
        "This git is read-only: it lists branches and never creates, moves or \
         deletes one",
    ),
    (
        "tag",
        &["-d", "--delete", "-a", "--annotate", "-m", "--message", "-s", "--sign", "-f", "--force"],
        "This git is read-only: it lists tags and never creates or deletes one",
    ),
    (
        "status",
        &["-b", "--branch"],
        "This git's status prints no branch line. 'git info' names the branch \
         HEAD is on",
    ),
    (
        "status",
        &["-u", "--untracked-files"],
        "Write the untracked mode as '--untracked no', '--untracked normal' or \
         '--untracked all'",
    ),
];

/// The usage error for a leaf argv clap refused, rebuilt for an agent.
///
/// An unknown flag is refused as not supported, with [`HINTS`]'s next step
/// when there is one and the verb's own flag list when there is not. Any
/// other clap error keeps clap's first line, which already names the flag and
/// the problem, and drops the `Usage:` and `--help` lines after it.
pub(crate) fn refuse(
    operation: &'static str,
    err: &clap::Error,
    cmd: &clap::Command,
    config: &GitConfig,
) -> GitError {
    let message = match err.kind() {
        ErrorKind::UnknownArgument => {
            let arg = match err.get(ContextKind::InvalidArg) {
                Some(ContextValue::String(arg)) => arg.clone(),
                _ => String::new(),
            };
            // `--pretty=oneline` is the flag `--pretty` with a value.
            let flag = arg.split_once('=').map_or(arg.as_str(), |(flag, _)| flag);
            match hint(operation, flag).filter(|next| runnable(next, config)) {
                Some(next) => format!("{flag} is not supported. {next}"),
                None => format!(
                    "{flag} is not supported. git {operation} accepts {}. See 'help git'",
                    flag_list(cmd)
                ),
            }
        }
        _ => format!("{}. See 'help git'", first_line(err)),
    };
    GitError::Usage { operation, message }
}

/// The next step for `flag` on `operation`, if there is a known one.
fn hint(operation: &str, flag: &str) -> Option<String> {
    if let Some((_, _, next)) = HINTS
        .iter()
        .find(|(op, flags, _)| *op == operation && flags.contains(&flag))
    {
        return Some(format!("{next}."));
    }
    // `git log -5`: git's count shorthand. Every verb that takes a count here
    // spells it `-n`.
    let count = flag.strip_prefix('-')?;
    if !count.is_empty() && count.bytes().all(|b| b.is_ascii_digit()) {
        return Some(format!("Write the count as '-n {count}'."));
    }
    None
}

/// The flags a verb accepts, as an agent would type them: `--limit (-n)`.
fn flag_list(cmd: &clap::Command) -> String {
    let flags: Vec<String> = cmd
        .get_arguments()
        .filter(|arg| !arg.is_hide_set() && !arg.is_positional())
        .filter_map(|arg| {
            let long = arg.get_long()?;
            Some(match arg.get_short() {
                Some(short) => format!("--{long} (-{short})"),
                None => format!("--{long}"),
            })
        })
        .collect();
    flags.join(", ")
}

/// clap's rendered error, cut to its first line and without the `error: `
/// label. The lines after it are `Usage:` and `try '--help'`, neither of which
/// this tool answers.
fn first_line(err: &clap::Error) -> String {
    let rendered = err.render().to_string();
    let line = rendered.lines().next().unwrap_or_default();
    line.strip_prefix("error: ").unwrap_or(line).trim_end_matches('.').to_string()
}

#[cfg(test)]
mod tests {
    use clap::{CommandFactory, Parser};

    use super::*;
    use crate::verbs;

    /// Parse `argv` with the named verb's own parser.
    fn parse(verb: &str, argv: &[&str]) -> Result<(), clap::Error> {
        let argv = std::iter::once(format!("git {verb}")).chain(argv.iter().map(|a| a.to_string()));
        match verb {
            "log" => verbs::log::LogArgs::try_parse_from(argv).map(|_| ()),
            "branch" => verbs::branch::BranchArgs::try_parse_from(argv).map(|_| ()),
            "tag" => verbs::tag::TagArgs::try_parse_from(argv).map(|_| ()),
            "status" => verbs::status::StatusArgs::try_parse_from(argv).map(|_| ()),
            other => panic!("HINTS names verb '{other}', which this test cannot parse for"),
        }
    }

    /// A hint for a flag the verb accepts is dead text: the refusal it
    /// explains can never happen, and the entry misleads whoever reads it. So
    /// each hinted flag must still be one clap refuses as unknown.
    #[test]
    fn every_hint_names_a_flag_its_verb_refuses() {
        for (verb, flags, _) in HINTS {
            for flag in *flags {
                let err = parse(verb, &[flag]).expect_err(&format!(
                    "git {verb} {flag} parses, so its hint can never be shown — \
                     delete it from HINTS"
                ));
                assert_eq!(
                    err.kind(),
                    ErrorKind::UnknownArgument,
                    "git {verb} {flag} is refused for another reason: {err}"
                );
            }
        }
    }

    /// The refusal says "not supported", names the flag, and carries the hint
    /// — and none of clap's lines that would mislead an agent.
    #[test]
    fn an_unknown_flag_is_refused_as_not_supported_with_its_hint() {
        let err = parse("log", &["--graph"]).expect_err("--graph is refused");
        let refused = refuse("log", &err, &verbs::log::LogArgs::command(), &GitConfig::read_only()).to_string();
        assert!(refused.starts_with("git log: --graph is not supported. "), "{refused}");
        assert!(refused.contains("--first-parent"), "the hint is carried: {refused}");
        for misleading in ["-- --graph", "Usage:", "--help", "tip:"] {
            assert!(!refused.contains(misleading), "clap's '{misleading}' leaked: {refused}");
        }
    }

    /// With no hint, the refusal lists what the verb does accept, so the next
    /// call can be chosen from it. The negative control is a flag `log` really
    /// has: it must be in the list, or the list is empty and proves nothing.
    #[test]
    fn an_unhinted_flag_lists_the_verbs_flags() {
        let err = parse("log", &["--frobnicate"]).expect_err("unknown");
        let refused = refuse("log", &err, &verbs::log::LogArgs::command(), &GitConfig::read_only()).to_string();
        assert!(refused.starts_with("git log: --frobnicate is not supported. git log accepts "), "{refused}");
        assert!(refused.contains("--limit (-n)"), "the list names log's real flags: {refused}");
        assert!(refused.contains("--json"), "the global flags are listed too: {refused}");
        assert!(!refused.contains("--operands"), "a hidden sink is not a flag to type: {refused}");
    }

    /// `--pretty=oneline` is refused as `--pretty`, so it finds `--pretty`'s
    /// hint rather than falling through to the generic list.
    #[test]
    fn a_flag_with_an_inline_value_finds_its_hint() {
        let err = parse("log", &["--pretty=oneline"]).expect_err("refused");
        let refused = refuse("log", &err, &verbs::log::LogArgs::command(), &GitConfig::read_only()).to_string();
        assert!(refused.starts_with("git log: --pretty is not supported. "), "{refused}");
        assert!(refused.contains("--oneline"), "{refused}");
    }

    /// `git log -5` is git's count shorthand, and the refusal spells the
    /// accepted form with the caller's own number.
    #[test]
    fn a_numeric_flag_is_refused_with_the_n_spelling() {
        let err = parse("log", &["-5"]).expect_err("refused");
        let refused = refuse("log", &err, &verbs::log::LogArgs::command(), &GitConfig::read_only()).to_string();
        assert!(refused.contains("Write the count as '-n 5'"), "{refused}");
    }

    /// An error that is not an unknown flag keeps clap's own first line — it
    /// names the conflict — and loses the lines after it.
    #[test]
    fn another_clap_error_keeps_its_first_line_only() {
        let err = parse("log", &["--merges", "--no-merges"]).expect_err("conflict");
        let refused = refuse("log", &err, &verbs::log::LogArgs::command(), &GitConfig::read_only()).to_string();
        assert!(refused.contains("--merges"), "{refused}");
        assert!(refused.contains("--no-merges"), "{refused}");
        assert!(refused.ends_with("See 'help git'"), "{refused}");
        assert!(!refused.contains("Usage:"), "{refused}");
    }
}
