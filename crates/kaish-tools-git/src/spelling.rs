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
/// double-quoted. kaish 0.17 refused either character unquoted in a word;
/// 0.18 accepts it, and the quoted form parses on both.
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
        &["--follow"],
        "This git does not follow renames: history stops where a file was \
         renamed. 'git log -- FILE' lists the commits that touched FILE under \
         its current name only",
    ),
    (
        "log",
        &["--date"],
        "This git takes no date format. The default table prints each commit's \
         date as YYYY-MM-DD, and --json carries the full author time in \
         'author.time'",
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
    argv: &[String],
) -> GitError {
    let message = match err.kind() {
        ErrorKind::UnknownArgument => {
            let arg = match err.get(ContextKind::InvalidArg) {
                Some(ContextValue::String(arg)) => arg.clone(),
                _ => String::new(),
            };
            // `--pretty=oneline` is the flag `--pretty` with a value.
            let first = arg.split_once('=').map_or(arg.as_str(), |(flag, _)| flag);
            let first = whole_word(first, argv);
            // clap stops at the first unknown flag. Reporting it alone costs a
            // retry per flag, so the rest of the argv is scanned for more.
            let mut flags = vec![first];
            for flag in other_unknown_flags(cmd, argv) {
                if !flags.contains(&flag) {
                    flags.push(flag);
                }
            }
            let mut parts = Vec::new();
            let mut lists_flags = false;
            for flag in &flags {
                match hint(operation, flag, cmd).filter(|next| runnable(next, config)) {
                    Some(next) => parts.push(format!("{flag} is not supported. {next}")),
                    None => {
                        parts.push(format!("{flag} is not supported."));
                        lists_flags = true;
                    }
                }
            }
            if lists_flags {
                parts.push(format!("git {operation} accepts {}. See 'help git'", flag_list(cmd)));
            }
            parts.join(" ")
        }
        _ => format!("{}. See 'help git'", first_line(err)),
    };
    GitError::Usage { operation, message }
}

/// clap names `-5` for `-5p` and for `-p5`, the part it could not place. The
/// caller typed one cluster, which git also refuses (`'5p' not an integer`),
/// so the refusal names the word as typed.
fn whole_word(flag: &str, argv: &[String]) -> String {
    if flag.len() > 1 && flag[1..].bytes().all(|b| b.is_ascii_digit()) {
        let longer = argv
            .iter()
            .take_while(|w| w.as_str() != "--")
            .find(|w| {
                !w.starts_with("--")
                    && w.len() > flag.len()
                    && (w.starts_with(flag) || w.ends_with(&flag[1..]) && w.starts_with('-'))
            });
        if let Some(word) = longer {
            return word.clone();
        }
    }
    flag.to_string()
}

/// The long flags and `-N` counts in `argv`, before any `--`, that `cmd` does
/// not accept. Short clusters are left to clap, which names the first.
fn other_unknown_flags(cmd: &clap::Command, argv: &[String]) -> Vec<String> {
    let mut found = Vec::new();
    for word in argv.iter().take_while(|w| w.as_str() != "--") {
        let Some(rest) = word.strip_prefix("--") else {
            if word.len() > 1 && word[1..].bytes().all(|b| b.is_ascii_digit()) {
                found.push(word.clone());
            }
            continue;
        };
        let name = rest.split_once('=').map_or(rest, |(name, _)| name);
        let known = name == "help"
            || cmd.get_arguments().any(|a| {
                a.get_long() == Some(name) || a.get_all_aliases().is_some_and(|al| al.contains(&name))
            });
        if !name.is_empty() && !known {
            found.push(format!("--{name}"));
        }
    }
    found
}

/// `git log -5` is git's count shorthand for `-n 5`: rewrite each `-N` word
/// before `--` into the two words clap parses. Only `log` has the shorthand
/// (`git show -2` means something else in git, and `branch` and `tag` have no
/// such flag).
pub(crate) fn expand_counts(operation: &str, argv: &[String]) -> Vec<String> {
    if operation != "log" {
        return argv.to_vec();
    }
    let mut out = Vec::with_capacity(argv.len());
    let mut before_marker = true;
    let mut takes_value = false;
    for word in argv {
        let is_count = before_marker
            && !takes_value
            && word.len() > 1
            && word.starts_with('-')
            && word[1..].bytes().all(|b| b.is_ascii_digit());
        if is_count {
            out.push("-n".to_string());
            out.push(word[1..].to_string());
        } else {
            out.push(word.clone());
        }
        if word == "--" {
            before_marker = false;
        }
        // The word after a bare value-taking flag is that flag's value.
        takes_value = !takes_value && matches!(word.as_str(), "-n" | "--limit" | "--rev" | "--author" | "--since" | "--until" | "--path" | "--repo");
    }
    out
}

/// The next step for `flag` on `operation`, if there is a known one.
fn hint(operation: &str, flag: &str, cmd: &clap::Command) -> Option<String> {
    if let Some((_, _, next)) = HINTS
        .iter()
        .find(|(op, flags, _)| *op == operation && flags.contains(&flag))
    {
        return Some(format!("{next}."));
    }
    // `git log -5`: git's count shorthand. The spelling named is one the verb
    // takes: `-n` where it has one, else `--limit`, else none.
    let count = flag.strip_prefix('-')?;
    if count.is_empty() || !count.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    let limit = cmd.get_arguments().find(|a| a.get_long() == Some("limit"))?;
    Some(match limit.get_short() {
        Some(short) => format!("Write the count as '-{short} {count}'."),
        None => format!("Write the count as '--limit {count}'."),
    })
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
        let refused = refuse("log", &err, &verbs::log::LogArgs::command(), &GitConfig::read_only(), &[]).to_string();
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
        let refused = refuse("log", &err, &verbs::log::LogArgs::command(), &GitConfig::read_only(), &[]).to_string();
        assert!(refused.starts_with("git log: --frobnicate is not supported. git log accepts "), "{refused}");
        assert!(refused.contains("--limit (-n)"), "the list names log's real flags: {refused}");
        assert!(refused.contains("--json"), "the global flags are listed too: {refused}");
        assert!(!refused.contains("--operands"), "a hidden sink is not a flag to type: {refused}");
    }

    /// `log -5` is `log -n 5`; a count after the marker is a path, and the
    /// word after a value-taking flag is that flag's value.
    #[test]
    fn counts_expand_before_the_marker_only() {
        let v = |w: &[&str]| w.iter().map(|s| s.to_string()).collect::<Vec<_>>();
        assert_eq!(expand_counts("log", &v(&["-12", "--oneline"])), v(&["-n", "12", "--oneline"]));
        assert_eq!(expand_counts("log", &v(&["--oneline", "--", "-2"])), v(&["--oneline", "--", "-2"]));
        assert_eq!(expand_counts("log", &v(&["--author", "-3"])), v(&["--author", "-3"]));
        assert_eq!(expand_counts("log", &v(&["--repo", "-3", "-2"])), v(&["--repo", "-3", "-n", "2"]));
        assert_eq!(expand_counts("show", &v(&["-2"])), v(&["-2"]));
    }

    /// `--pretty=oneline` is refused as `--pretty`, so it finds `--pretty`'s
    /// hint rather than falling through to the generic list.
    #[test]
    fn a_flag_with_an_inline_value_finds_its_hint() {
        let err = parse("log", &["--pretty=oneline"]).expect_err("refused");
        let refused = refuse("log", &err, &verbs::log::LogArgs::command(), &GitConfig::read_only(), &[]).to_string();
        assert!(refused.starts_with("git log: --pretty is not supported. "), "{refused}");
        assert!(refused.contains("--oneline"), "{refused}");
    }

    /// `-5` is accepted by `log` (see `expand_counts`) and by no other verb;
    /// `branch` refuses it and spells the accepted form with the caller's own
    /// number.
    #[test]
    fn a_numeric_flag_is_refused_with_the_n_spelling() {
        let err = parse("branch", &["-5"]).expect_err("refused");
        let refused = refuse("branch", &err, &verbs::branch::BranchArgs::command(), &GitConfig::read_only(), &[]).to_string();
        assert!(refused.contains("Write the count as '-n 5'"), "{refused}");
    }

    /// An error that is not an unknown flag keeps clap's own first line — it
    /// names the conflict — and loses the lines after it.
    #[test]
    fn another_clap_error_keeps_its_first_line_only() {
        let err = parse("log", &["--merges", "--no-merges"]).expect_err("conflict");
        let refused = refuse("log", &err, &verbs::log::LogArgs::command(), &GitConfig::read_only(), &[]).to_string();
        assert!(refused.contains("--merges"), "{refused}");
        assert!(refused.contains("--no-merges"), "{refused}");
        assert!(refused.ends_with("See 'help git'"), "{refused}");
        assert!(!refused.contains("Usage:"), "{refused}");
    }
}
