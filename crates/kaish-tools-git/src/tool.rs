//! The kaish [`Tool`] implementation: schema tree, argv routing, the
//! `resolve_real_path` bridge, and the blocking seam (architecture.md E).

use std::path::{Path, PathBuf};

use async_trait::async_trait;
use clap::{CommandFactory, Parser};

use kaish_tool_api::{schema_tree_from_clap, Tool, ToolCtx};
use kaish_types::backend::MountInfo;
use kaish_types::{ExecResult, ToolArgs, ToolSchema, Value};

use crate::config::{ConfigError, GitConfig, Verb};
use crate::error::GitError;
use crate::model::{Capabilities, LimitsReport};
use crate::repo::ReadRepo;
use crate::verbs;

/// The registered `git` tool.
///
/// Construct it with [`crate::tool`]; the config it carries is what every
/// schema and every dispatch is derived from, so a verb the embedder
/// subtracted is absent from `tools --json`, from `help git`, and from
/// completion — not merely rejected at execute time.
#[derive(Debug)]
pub struct GitTool {
    config: GitConfig,
}

/// Build a `git` tool from an embedder's config.
///
/// Fails loudly at registration for a config that could never work, rather
/// than at the first invocation, so an embedder finds out while wiring the
/// kernel.
pub fn tool(config: GitConfig) -> Result<GitTool, ConfigError> {
    let name = config.tool_name();
    if name.is_empty() || name.chars().any(char::is_whitespace) {
        return Err(ConfigError::UnusableToolName {
            name: name.to_string(),
        });
    }
    if config.verbs().next().is_none() {
        return Err(ConfigError::NoVerbsEnabled);
    }
    Ok(GitTool { config })
}

impl GitTool {
    /// What this build will let the caller ask for (B.1's `capabilities`).
    fn capabilities(&self) -> Capabilities {
        let limits = self.config.limits();
        Capabilities {
            profiles: self.config.profiles().map(|p| p.as_str().to_string()).collect(),
            verbs: self.config.verbs().map(|v| v.as_str().to_string()).collect(),
            features: crate::enabled_features(),
            limits: LimitsReport {
                max_rows: limits.max_rows,
                max_diff_files: limits.max_diff_files,
                max_blob_bytes: limits.max_blob_bytes,
                max_hunk_bytes_per_file: limits.max_hunk_bytes_per_file,
                submodule_depth: limits.submodule_depth,
            },
        }
    }

    #[tracing::instrument(
        level = "info",
        name = "git.verb",
        skip_all,
        fields(verb = "info", repo)
    )]
    async fn run_info(&self, args: ToolArgs, consumed: usize, ctx: &mut dyn ToolCtx) -> ExecResult {
        const OP: &str = "info";
        if let Err(e) = verb_enabled(&self.config, Verb::Info, OP) {
            return failure(e);
        }

        let parsed = match parse_leaf::<verbs::info::InfoArgs>(&args, consumed, OP) {
            Ok(p) => p,
            Err(result) => return *result,
        };
        parsed.global.apply(ctx);

        if let Err(e) = no_operands(OP, &operands(&args, consumed)) {
            return failure(e);
        }

        let resolved = match resolve_repo_paths(OP, ctx, parsed.repo.as_deref()) {
            Ok(r) => r,
            Err(e) => return failure(e),
        };
        tracing::Span::current().record("repo", tracing::field::display(resolved.real.display()));

        let capabilities = self.capabilities();
        let mount_real = resolved.mount_real.clone();
        let mount_vfs = resolved.mount_vfs.clone();

        // E.3: the repository is opened, read, and dropped inside one
        // closure, and what comes out is an owned model with no gix types in
        // it. Nothing `!Send` exists at any await point in this function.
        let model = block_in_place_compat(move || {
            let repo = ReadRepo::discover(OP, &resolved.real, &resolved.ceiling)?;
            let repo_root_vfs = to_vfs_path(repo.root(), &mount_real, &mount_vfs);
            verbs::info::run(&repo, repo_root_vfs, capabilities)
        });

        let model = match model {
            Ok(m) => m,
            Err(e) => return failure(e),
        };

        let mut result = ExecResult::with_output(crate::render::repo_info(&model));
        // E.4: an embedder's trace can correlate a tool call with a
        // repository state. Egress merge is `.entry().or_insert()`, so
        // tool-emitted entries win and setting these is safe.
        result
            .baggage
            .insert("git.repo".to_string(), model.repo_root_real.clone());
        if let Some(oid) = &model.head.oid {
            result.baggage.insert("git.head_oid".to_string(), oid.clone());
        }
        result
    }

    #[tracing::instrument(
        level = "info",
        name = "git.verb",
        skip_all,
        fields(verb = "status", repo)
    )]
    async fn run_status(
        &self,
        args: ToolArgs,
        consumed: usize,
        ctx: &mut dyn ToolCtx,
    ) -> ExecResult {
        const OP: &str = "status";
        if let Err(e) = verb_enabled(&self.config, Verb::Status, OP) {
            return failure(e);
        }

        let parsed = match parse_leaf::<verbs::status::StatusArgs>(&args, consumed, OP) {
            Ok(p) => p,
            Err(result) => return *result,
        };
        parsed.global.apply(ctx);

        let resolved = match resolve_repo_paths(OP, ctx, parsed.repo.as_deref()) {
            Ok(r) => r,
            Err(e) => return failure(e),
        };
        tracing::Span::current().record("repo", tracing::field::display(resolved.real.display()));

        // The embedder's `max_rows` is a hard cap; `--limit` may only lower it.
        let limit = parsed.limit.min(self.config.limits().max_rows);
        // Not lowerable by an argument: this one caps a read, not an output.
        // Status hashes every tracked file, so it is the only thing standing
        // between a repository and an allocation the repository picked.
        let max_blob_bytes = self.config.limits().max_blob_bytes;
        let untracked = parsed.untracked;
        let ignored = parsed.ignored;
        // `git status [--] <path>...` — every operand is a pathspec on either
        // side of the marker, because status takes no revision. They join the
        // `--path` flags rather than replacing them.
        let mut path_args = parsed.path.clone();
        path_args.extend(operands(&args, consumed).all());

        let outcome = block_in_place_compat(move || {
            let repo = ReadRepo::discover(OP, &resolved.real, &resolved.ceiling)?;
            // `--path` values are relative to the caller's cwd within the repo,
            // exactly as git's pathspecs are. Prefix them with the cwd's
            // repo-relative directory so a filter from a subdirectory means what
            // it says. A pathspec-magic value (leading `:`) is left untouched so
            // the parser can reject it by name rather than hide it behind a
            // prefix.
            let prefix = cwd_prefix(&resolved.real, repo.root());
            let paths = path_args
                .iter()
                .map(|spec| {
                    if prefix.is_empty() || spec.starts_with(':') {
                        spec.clone()
                    } else {
                        format!("{prefix}/{spec}")
                    }
                })
                .collect();
            let opts = verbs::status::StatusOptions {
                untracked,
                ignored,
                paths,
                limit,
                max_blob_bytes,
            };
            let root = repo.root().display().to_string();
            verbs::status::run(&repo, &opts).map(|model| (model, root))
        });

        let (model, repo_root) = match outcome {
            Ok(pair) => pair,
            Err(e) => return failure(e),
        };

        let mut result = ExecResult::with_output(crate::render::status(&model));
        // Truncation is always reported — a stderr note beside the JSON's
        // `truncated: true` (E.5). The exit code stays 0: a truncated status is
        // a successful answer that ran up against `--limit`.
        if model.truncated {
            result.err = format!(
                "git status: output truncated at {} entries (--limit); \
                 'truncated' is true in --json",
                model.entries.len()
            );
        }
        result.baggage.insert("git.repo".to_string(), repo_root);
        if let Some(oid) = &model.head.oid {
            result.baggage.insert("git.head_oid".to_string(), oid.clone());
        }
        result
    }

    #[tracing::instrument(
        level = "info",
        name = "git.verb",
        skip_all,
        fields(verb = "log", repo)
    )]
    async fn run_log(&self, args: ToolArgs, consumed: usize, ctx: &mut dyn ToolCtx) -> ExecResult {
        const OP: &str = "log";
        if let Err(e) = verb_enabled(&self.config, Verb::Log, OP) {
            return failure(e);
        }

        let parsed = match parse_leaf::<verbs::log::LogArgs>(&args, consumed, OP) {
            Ok(p) => p,
            Err(result) => return *result,
        };
        parsed.global.apply(ctx);

        // `--patch` is refused before anything is read. This build assembles no
        // unified-diff text, and answering a `--patch` request with a stat — or
        // with a silently flag-less log — would be a wrong answer rather than a
        // missing one (E.5's precedent).
        if parsed.patch {
            return failure(GitError::PatchNeedsTextdiff { operation: OP });
        }

        let resolved = match resolve_repo_paths(OP, ctx, parsed.repo.as_deref()) {
            Ok(r) => r,
            Err(e) => return failure(e),
        };
        tracing::Span::current().record("repo", tracing::field::display(resolved.real.display()));

        // Dates are parsed here, on the caller's own argument, so a bad one is
        // a usage error before a repository is even opened.
        let since = match parsed.since.as_deref() {
            Some(v) => match verbs::log::parse_date(OP, "--since", v) {
                Ok(t) => Some(t),
                Err(e) => return failure(e),
            },
            None => None,
        };
        let until = match parsed.until.as_deref() {
            Some(v) => match verbs::log::parse_date(OP, "--until", v) {
                Ok(t) => Some(t),
                Err(e) => return failure(e),
            },
            None => None,
        };
        // An inverted window can never match, and answering it with a confident
        // empty log would look like "this history has no such commits" rather
        // than "you asked for an empty range".
        if let (Some(s), Some(u)) = (since, until) {
            if s > u {
                return failure(GitError::Usage {
                    operation: OP,
                    message: format!(
                        "--since '{}' is later than --until '{}', so no commit can \
                         match. Did the two get swapped?",
                        parsed.since.as_deref().unwrap_or_default(),
                        parsed.until.as_deref().unwrap_or_default()
                    ),
                });
            }
        }

        // The embedder's `max_rows` is a hard cap; `--limit` may only lower it.
        let limit = parsed.limit.min(self.config.limits().max_rows);
        // Not lowerable by an argument: this one caps a read, not an output.
        // `--stat` reads blob pairs to count lines, so this is what stands
        // between a repository and an allocation the repository picked.
        let max_blob_bytes = self.config.limits().max_blob_bytes;
        let max_diff_files = self.config.limits().max_diff_files;
        let merges = match (parsed.merges, parsed.no_merges) {
            (true, false) => verbs::log::MergeFilter::Only,
            (false, true) => verbs::log::MergeFilter::Exclude,
            // `(true, true)` is unreachable — clap's `conflicts_with` rejects
            // it — and `(false, false)` is the documented default.
            _ => verbs::log::MergeFilter::Both,
        };
        // `git log [<rev>] [-- <path>...]`, git's own shape.
        //
        // A positional before `--` is a **revision**, always. Git would guess:
        // it tries the string as a rev, then as a path, and errors only when
        // the string is both or neither. We do not guess — the same reason the
        // revision grammar is small and approxidate is refused. One rule, and
        // a failure that names the other spelling, beats a heuristic that
        // silently answers about a path when the caller meant a branch.
        let ops = operands(&args, consumed);
        if ops.before.len() > 1 {
            return failure(GitError::Usage {
                operation: OP,
                message: format!(
                    "takes at most one revision, but got '{}'. A range is \
                     '--rev A' plus '--rev B' in two calls, not 'A B'; paths \
                     go after '--'",
                    ops.before.join("' '")
                ),
            });
        }
        let rev = match ops.before.first() {
            Some(positional) if parsed.rev != verbs::log::DEFAULT_REV => {
                // Both spellings, disagreeing. Picking one silently would
                // answer about a revision the caller did not choose.
                return failure(GitError::Usage {
                    operation: OP,
                    message: format!(
                        "got a revision twice — '{positional}' and '--rev \
                         {}'. Give it once",
                        parsed.rev
                    ),
                });
            }
            Some(positional) => positional.clone(),
            None => parsed.rev.clone(),
        };
        let author = parsed.author.clone();
        let first_parent = parsed.first_parent;
        let body = parsed.body;
        let stat = parsed.stat;
        // Operands after `--` are pathspecs, joining the `--path` flags.
        let mut path_args = parsed.path.clone();
        path_args.extend(ops.after.iter().cloned());

        let outcome = block_in_place_compat(move || {
            let repo = ReadRepo::discover(OP, &resolved.real, &resolved.ceiling)?;
            // `--path` values are relative to the caller's cwd within the repo,
            // exactly as git's pathspecs are — the same prefixing `status` does.
            let prefix = cwd_prefix(&resolved.real, repo.root());
            let paths = path_args
                .iter()
                .map(|spec| {
                    if prefix.is_empty() || spec.starts_with(':') {
                        spec.clone()
                    } else {
                        format!("{prefix}/{spec}")
                    }
                })
                .collect();
            let opts = verbs::log::LogOptions {
                rev,
                limit,
                paths,
                since,
                until,
                author,
                merges,
                first_parent,
                body,
                stat,
                max_blob_bytes,
                max_diff_files,
            };
            let root = repo.root().display().to_string();
            verbs::log::run(&repo, &opts).map(|model| (model, root))
        });

        let (model, repo_root) = match outcome {
            Ok(pair) => pair,
            Err(e) => return failure(e),
        };

        let mut result = ExecResult::with_output(crate::render::log(&model));
        // Truncation is always reported — a stderr note beside the JSON's
        // `truncated: true` (E.5). The exit code stays 0: a truncated log is a
        // successful answer that ran up against `--limit`.
        if model.truncated {
            // Naming `--limit` unconditionally would be a small lie whenever
            // the walk budget stopped us instead: an agent would lower a limit
            // that was never the constraint. A report short of the limit can
            // only have come from the budget.
            result.err = if model.commits.len() >= limit {
                format!(
                    "git log: output truncated at {} commits (--limit); \
                     'truncated' is true in --json",
                    model.commits.len()
                )
            } else {
                format!(
                    "git log: stopped after examining the maximum number of \
                     commits without filling --limit ({} matched); \
                     'truncated' is true in --json. Narrow the search with \
                     --rev or a date window",
                    model.commits.len()
                )
            };
        }
        result.baggage.insert("git.repo".to_string(), repo_root);
        if let Some(first) = model.commits.first() {
            result
                .baggage
                .insert("git.log_tip_oid".to_string(), first.oid.clone());
        }
        result
    }

    #[tracing::instrument(
        level = "info",
        name = "git.verb",
        skip_all,
        fields(verb = "ls", repo)
    )]
    async fn run_ls(&self, args: ToolArgs, consumed: usize, ctx: &mut dyn ToolCtx) -> ExecResult {
        const OP: &str = "ls";
        if let Err(e) = verb_enabled(&self.config, Verb::Ls, OP) {
            return failure(e);
        }

        let parsed = match parse_leaf::<verbs::ls::LsArgs>(&args, consumed, OP) {
            Ok(p) => p,
            Err(result) => return *result,
        };
        parsed.global.apply(ctx);

        let resolved = match resolve_repo_paths(OP, ctx, parsed.repo.as_deref()) {
            Ok(r) => r,
            Err(e) => return failure(e),
        };
        tracing::Span::current().record("repo", tracing::field::display(resolved.real.display()));

        // Position, not content, decides the two operands: the first is
        // always the revision, the second the path — `git ls-tree <rev>
        // [<path>]`'s own grammar, which needs no `--` to tell them apart.
        let ops = operands(&args, consumed).all();
        if ops.len() > 2 {
            return failure(GitError::Usage {
                operation: OP,
                message: format!(
                    "takes at most a revision and a path, but got {} operands: \
                     '{}'",
                    ops.len(),
                    ops.join("' '")
                ),
            });
        }
        let rev = ops
            .first()
            .cloned()
            .unwrap_or_else(|| verbs::ls::DEFAULT_REV.to_string());
        let path = ops.get(1).cloned().unwrap_or_default();

        // The embedder's `max_rows` is a hard cap; `--limit` may only lower it.
        let limit = parsed.limit.min(self.config.limits().max_rows);
        let recursive = parsed.recursive;

        let outcome = block_in_place_compat(move || {
            let repo = ReadRepo::discover(OP, &resolved.real, &resolved.ceiling)?;
            let opts = verbs::ls::LsOptions {
                rev,
                path,
                recursive,
                limit,
            };
            let root = repo.root().display().to_string();
            verbs::ls::run(&repo, &opts).map(|model| (model, root))
        });

        let (model, repo_root) = match outcome {
            Ok(pair) => pair,
            Err(e) => return failure(e),
        };

        let mut result = ExecResult::with_output(crate::render::ls(&model));
        if model.truncated {
            result.err = format!(
                "git ls: output truncated at {} entries (--limit); 'truncated' \
                 is true in --json",
                model.entries.len()
            );
        }
        result.baggage.insert("git.repo".to_string(), repo_root);
        result
    }

    #[tracing::instrument(
        level = "info",
        name = "git.verb",
        skip_all,
        fields(verb = "show", repo)
    )]
    async fn run_show(&self, args: ToolArgs, consumed: usize, ctx: &mut dyn ToolCtx) -> ExecResult {
        const OP: &str = "show";
        if let Err(e) = verb_enabled(&self.config, Verb::Show, OP) {
            return failure(e);
        }

        let parsed = match parse_leaf::<verbs::show::ShowArgs>(&args, consumed, OP) {
            Ok(p) => p,
            Err(result) => return *result,
        };
        parsed.global.apply(ctx);

        let resolved = match resolve_repo_paths(OP, ctx, parsed.repo.as_deref()) {
            Ok(r) => r,
            Err(e) => return failure(e),
        };
        tracing::Span::current().record("repo", tracing::field::display(resolved.real.display()));

        // A single operand: the whole flagship spelling, colon path
        // included (`show HEAD:src/lib.rs` is one operand, not two).
        let ops = operands(&args, consumed).all();
        if ops.len() > 1 {
            return failure(GitError::Usage {
                operation: OP,
                message: format!(
                    "takes exactly one revision, but got {} operands: '{}'. A \
                     path is part of the revision operand ('show \
                     HEAD:src/lib.rs'), not a second one",
                    ops.len(),
                    ops.join("' '")
                ),
            });
        }
        let rev = ops
            .into_iter()
            .next()
            .unwrap_or_else(|| verbs::show::DEFAULT_REV.to_string());

        let limit = parsed.limit.min(self.config.limits().max_rows);
        let max_blob_bytes = self.config.limits().max_blob_bytes;

        let outcome = block_in_place_compat(move || {
            let repo = ReadRepo::discover(OP, &resolved.real, &resolved.ceiling)?;
            let opts = verbs::show::ShowOptions {
                rev,
                limit,
                max_blob_bytes,
            };
            let root = repo.root().display().to_string();
            verbs::show::run(&repo, &opts).map(|outcome| (outcome, root))
        });

        let (outcome, repo_root) = match outcome {
            Ok(pair) => pair,
            Err(e) => return failure(e),
        };

        // D5: the type is always stated in the output — `git.show_kind`
        // baggage names it regardless of which arm below produced it, on top
        // of each structured shape's own `kind` field (render.rs's
        // `tag_kind`) or, for the blob form, the fact that there is no
        // structure to tag at all.
        let (mut result, kind) = match outcome {
            verbs::show::ShowOutcome::Commit(commit) => {
                (ExecResult::with_output(crate::render::show_commit(&commit)), "commit")
            }
            verbs::show::ShowOutcome::Tag(tag) => {
                (ExecResult::with_output(crate::render::show_tag(&tag)), "tag")
            }
            verbs::show::ShowOutcome::Tree(report) => {
                let mut r = ExecResult::with_output(crate::render::show_tree(&report));
                if report.truncated {
                    r.err = format!(
                        "git show: output truncated at {} entries (--limit); \
                         'truncated' is true in --json",
                        report.entries.len()
                    );
                }
                (r, "tree")
            }
            verbs::show::ShowOutcome::Gitlink(row) => {
                (ExecResult::with_output(crate::render::show_gitlink(&row)), "commit")
            }
            verbs::show::ShowOutcome::Blob {
                oid,
                size,
                truncated,
                bytes,
            } => {
                let mut r = ExecResult::success_text_or_bytes(bytes);
                if truncated {
                    r.err = format!(
                        "git show: blob '{oid}' is {size} bytes, over this \
                         build's {max_blob_bytes}-byte cap (GitConfig limits, \
                         max_blob_bytes) — content was not read. Raise the cap \
                         to read it"
                    );
                }
                r.baggage.insert("git.oid".to_string(), oid);
                r.baggage.insert("git.size".to_string(), size.to_string());
                (r, "blob")
            }
        };
        result.baggage.insert("git.repo".to_string(), repo_root);
        result.baggage.insert("git.show_kind".to_string(), kind.to_string());
        result
    }
}

/// The repo-relative, slash-separated directory of `real` within `root`, or
/// empty when `real` is the root itself or cannot be placed under it.
fn cwd_prefix(real: &Path, root: &Path) -> String {
    let canonical = std::fs::canonicalize(real).unwrap_or_else(|_| real.to_path_buf());
    let Ok(rest) = canonical.strip_prefix(root) else {
        return String::new();
    };
    rest.components()
        .map(|c| c.as_os_str().to_string_lossy())
        .collect::<Vec<_>>()
        .join("/")
}

#[async_trait]
impl Tool for GitTool {
    fn name(&self) -> &str {
        self.config.tool_name()
    }

    fn schema(&self) -> ToolSchema {
        // E.1: the clap tree exists for schema reflection, and it is built
        // from ONLY the verbs this config enables — so a disabled verb is not
        // merely rejected, it is unroutable because `select_leaf` has nothing
        // to route to.
        let mut cmd = clap::Command::new(self.config.tool_name().to_string()).about(DESCRIPTION);
        if self.config.has(Verb::Info) {
            cmd = cmd.subcommand(verbs::info::InfoArgs::command().name("info"));
        }
        if self.config.has(Verb::Status) {
            cmd = cmd.subcommand(verbs::status::StatusArgs::command().name("status"));
        }
        if self.config.has(Verb::Log) {
            cmd = cmd.subcommand(verbs::log::LogArgs::command().name("log"));
        }
        if self.config.has(Verb::Ls) {
            cmd = cmd.subcommand(verbs::ls::LsArgs::command().name("ls"));
        }
        if self.config.has(Verb::Show) {
            cmd = cmd.subcommand(verbs::show::ShowArgs::command().name("show"));
        }
        schema_tree_from_clap(
            &cmd,
            self.config.tool_name(),
            DESCRIPTION,
            examples_for(&self.config),
        )
    }

    async fn execute(&self, mut args: ToolArgs, ctx: &mut dyn ToolCtx) -> ExecResult {
        let schema = self.schema();
        args.flagify_bool_named(&schema);

        let (verb, consumed) = match route(&schema, &args.positional) {
            Ok(r) => r,
            Err(e) => return failure(e),
        };

        match verb.as_str() {
            "info" => self.run_info(args, consumed, ctx).await,
            "status" => self.run_status(args, consumed, ctx).await,
            "log" => self.run_log(args, consumed, ctx).await,
            "ls" => self.run_ls(args, consumed, ctx).await,
            "show" => self.run_show(args, consumed, ctx).await,
            // Unreachable: `route` only returns names it found in the schema,
            // and the schema is built from the verbs this file dispatches.
            // Reached anyway means a verb was added to `schema()` without a
            // dispatch arm, which is a bug in this file, not in the caller.
            other => ExecResult::failure(
                2,
                format!(
                    "git: '{other}' is in this build's schema but has no implementation — \
                     this is a bug in kaish-tools-git, not in your command line"
                ),
            ),
        }
    }
}

/// The tool's own description, shared by the clap tree and the schema.
const DESCRIPTION: &str =
    "Read a git repository — shallow, safety-first, and read-only by construction";

/// Examples the schema carries into `help git` and completion.
///
/// Every example's command starts `git <verb>`, and [`examples_for`] filters
/// on that word before the schema is built — an example for a verb this
/// config subtracted is exactly the kind of thing E.1's gate exists to keep
/// out of `help git`: a disabled verb must be absent from what an agent is
/// told exists, not merely refused once it tries the example. There is one
/// entry per implemented [`Verb`] — `help <tool>`'s renderer
/// (`kaish-help`'s `tool_help`) shows only params and examples, never a bare
/// subcommand list, so a verb with no example here would never be named in
/// `help git` at all, enabled or not.
const EXAMPLES: [(&str, &str); 7] = [
    ("What repository is this", "git info"),
    ("Inspect a specific repository", "git info --repo /mnt/repos/kaish"),
    ("Structured, for a script", "git info --json"),
    ("What changed in the working tree", "git status"),
    ("Recent commit history", "git log"),
    ("Read a file as of the last release", "git show v0.1.0:src/lib.rs"),
    ("List a directory as of HEAD", "git ls HEAD src"),
];

/// [`EXAMPLES`], narrowed to the ones whose verb this config still enables.
///
/// Driven by `Verb::ALL`/[`GitConfig::has`] rather than a hand-matched list
/// of example strings, so a verb subtracted by an embedder — or added by a
/// later phasing PR, per the coordination rule with the sibling `git diff`
/// PR — needs no change here to stay correct.
fn examples_for(config: &GitConfig) -> impl Iterator<Item = (&'static str, &'static str)> + '_ {
    EXAMPLES.iter().copied().filter(move |(_, code)| {
        // Every example is `git <verb> ...`; the second whitespace-separated
        // word is the verb it demonstrates.
        code.split_whitespace()
            .nth(1)
            .and_then(|word| Verb::ALL.iter().find(|v| v.as_str() == word))
            .is_some_and(|verb| config.has(*verb))
    })
}

/// Wrap a [`GitError`] as an [`ExecResult`] carrying its taxonomy code.
fn failure(err: GitError) -> ExecResult {
    ExecResult::failure(err.exit_code(), err.to_string())
}

/// Belt-and-braces profile check (E.5, exit 5).
///
/// Unreachable through normal dispatch — a disabled verb is absent from the
/// schema, so routing never produces its name. It is here because "the
/// schema is the gate" is a property of two files agreeing, and this is the
/// one that fails closed if they ever stop.
fn verb_enabled(config: &GitConfig, verb: Verb, operation: &'static str) -> Result<(), GitError> {
    if config.has(verb) {
        Ok(())
    } else {
        Err(GitError::VerbNotEnabled { operation })
    }
}

/// Route the verb words off the typed positionals, mirroring the kernel's
/// `select_leaf` (E.1).
///
/// Returns the selected verb path joined by spaces, and how many positionals
/// it consumed. Descent stops at the first positional that is not a child
/// name, exactly as `select_leaf` does, so the leaf's own positionals are
/// left alone.
fn route(schema: &ToolSchema, positional: &[Value]) -> Result<(String, usize), GitError> {
    let mut node = schema;
    let mut path: Vec<String> = Vec::new();
    let mut consumed = 0usize;

    for value in positional {
        if node.subcommands.is_empty() {
            break;
        }
        let Value::String(word) = value else {
            break;
        };
        let Some(child) = node.subcommands.iter().find(|c| c.matches_command(word)) else {
            break;
        };
        path.push(child.name.clone());
        node = child;
        consumed += 1;
    }

    if path.is_empty() {
        let available: Vec<&str> = schema.subcommands.iter().map(|s| s.name.as_str()).collect();
        let got = match positional.first() {
            Some(Value::String(s)) => format!("'{s}' is not one of them"),
            Some(_) => "the first argument is not a verb name".to_string(),
            None => "none was given".to_string(),
        };
        return Err(GitError::NoVerb {
            tool: schema.name.clone(),
            got,
            // An embedder can subtract every verb. The tool constructor
            // refuses that config, so this is not reachable through
            // registration — but if it ever were, "offers: " followed by
            // nothing reads like a truncated message rather than an answer.
            available: if available.is_empty() {
                "no verbs at all — every verb was subtracted from this build's \
                 config"
                    .to_string()
            } else {
                available.join(", ")
            },
        });
    }

    Ok((path.join(" "), consumed))
}

/// Re-parse the leaf's own argv with its flat clap `Parser` (E.1).
///
/// Feeding the whole `to_argv()` to a clap *tree* would break: `to_argv()`
/// emits a `--` before positionals, and a `--` ahead of a subcommand name
/// defeats clap's subcommand parsing. So the verb words come off the typed
/// positionals first, and what is left is parsed flat.
fn parse_leaf<P: Parser>(
    args: &ToolArgs,
    consumed: usize,
    operation: &'static str,
) -> Result<P, Box<ExecResult>> {
    // Boxed: `ExecResult` is a wide type, and an unboxed `Err` variant makes
    // every `Ok` return pay for it (clippy::result_large_err).
    let mut leaf = args.clone();
    leaf.positional = args.positional.iter().skip(consumed).cloned().collect();
    let argv = leaf
        .to_argv()
        .map_err(|e| Box::new(ExecResult::failure(2, format!("git {operation}: {e}"))))?;
    P::try_parse_from(std::iter::once(format!("git {operation}")).chain(argv))
        .map_err(|e| Box::new(ExecResult::failure(2, format!("git {operation}: {e}"))))
}

/// A verb's operands, split at git's `--` end-of-options marker.
///
/// The kernel hands a literal `--` through as a positional `Value::String`, so
/// the split git users rely on survives all the way here. Reading operands off
/// `args.positional` rather than off the clap-parsed struct is the kernel's own
/// convention — every builtin does it, because `ToolArgs::to_argv()` always
/// emits `--` before positionals and clap therefore cannot tell a caller's
/// marker from the binder's.
struct Operands {
    /// Positionals before `--`. A revision, for a verb that takes one.
    before: Vec<String>,
    /// Positionals after `--`. Always pathspecs, for every verb.
    after: Vec<String>,
}

impl Operands {
    /// Every operand, in order — for verbs where both halves mean the same
    /// thing (`status` takes only pathspecs, with or without the marker).
    fn all(&self) -> Vec<String> {
        let mut out = self.before.clone();
        out.extend(self.after.iter().cloned());
        out
    }

    fn is_empty(&self) -> bool {
        self.before.is_empty() && self.after.is_empty()
    }
}

/// Split the operands a verb was given at the `--` marker.
fn operands(args: &ToolArgs, consumed: usize) -> Operands {
    let mut before = Vec::new();
    let mut after = Vec::new();
    let mut seen_marker = false;
    for value in args.positional.iter().skip(consumed) {
        // Only a string operand can be a revision or a pathspec. A typed
        // value here (an int from a pipeline, say) is not one, and coercing it
        // to text would invent an argument the caller did not write.
        let Value::String(text) = value else {
            continue;
        };
        let text = text.clone();
        if !seen_marker && text == "--" {
            seen_marker = true;
            continue;
        }
        if seen_marker {
            after.push(text);
        } else {
            before.push(text);
        }
    }
    Operands { before, after }
}

/// Refuse operands for a verb that takes none, naming what was given.
///
/// Silence here is what made this a bug: the operands used to land in a hidden
/// clap sink and vanish, so `git info /some/path` answered about the cwd with
/// exit 0 — a different question than the one asked, and nothing in the output
/// said so.
fn no_operands(operation: &'static str, ops: &Operands) -> Result<(), GitError> {
    if ops.is_empty() {
        return Ok(());
    }
    Err(GitError::Usage {
        operation,
        message: format!(
            "takes no operands, but got '{}'. Name the repository with --repo",
            ops.all().join("' '")
        ),
    })
}

/// Everything the E.2 bridge produces for one invocation.
struct ResolvedPaths {
    /// The path the caller named, on the host filesystem.
    real: PathBuf,
    /// The real root of the containing mount — the discovery ceiling.
    ceiling: PathBuf,
    /// The containing mount's VFS path.
    mount_vfs: PathBuf,
    /// The containing mount's real root.
    mount_real: PathBuf,
}

/// The `resolve_real_path` bridge (architecture.md E.2).
///
/// VFS path in, real path plus a discovery ceiling out. The ceiling is the
/// real root of the mount that contains the path, so `git info` inside a
/// mount can never discover the host's repository two directories above it.
fn resolve_repo_paths(
    operation: &'static str,
    ctx: &dyn ToolCtx,
    repo_arg: Option<&str>,
) -> Result<ResolvedPaths, GitError> {
    let vfs = ctx.resolve_path(repo_arg.unwrap_or("."));
    let backend = ctx.backend();

    let real = backend
        .resolve_real_path(&vfs)
        .ok_or_else(|| GitError::NotRealPath {
            operation,
            vfs_path: vfs.clone(),
        })?;

    let mounts = backend.mounts();
    let mount = longest_prefix_mount(&mounts, &vfs).ok_or_else(|| GitError::NoContainingMount {
        operation,
        vfs_path: vfs.clone(),
    })?;
    let ceiling = backend
        .resolve_real_path(&mount.path)
        .ok_or_else(|| GitError::NotRealPath {
            operation,
            vfs_path: mount.path.clone(),
        })?;

    Ok(ResolvedPaths {
        real,
        mount_vfs: mount.path.clone(),
        mount_real: ceiling.clone(),
        ceiling,
    })
}

/// The mount whose VFS path is the longest prefix of `path`.
fn longest_prefix_mount<'a>(mounts: &'a [MountInfo], path: &Path) -> Option<&'a MountInfo> {
    mounts
        .iter()
        .filter(|m| path.starts_with(&m.path))
        .max_by_key(|m| m.path.components().count())
}

/// Map a real path back into the VFS through the mount it was reached by.
///
/// `None` when it does not fall inside the mount — an agent should be told
/// when it cannot name a path it can see, rather than shown a VFS path that
/// resolves to something else.
fn to_vfs_path(real: &Path, mount_real: &Path, mount_vfs: &Path) -> Option<String> {
    let rest = real.strip_prefix(mount_real).ok()?;
    Some(mount_vfs.join(rest).display().to_string())
}

/// Run blocking gix work without stalling the runtime, on either flavor.
///
/// `tokio::task::block_in_place` is the right call on a multi-thread runtime
/// and *panics* on a current-thread one, which an embedder may well be using
/// (kaish-web's browser build is exactly that). Same work either way — this
/// picks a scheduling strategy, it is not a semantic fallback, and the
/// breadcrumb says which path ran so a surprise is visible in a trace rather
/// than inferred from a stall.
pub(crate) fn block_in_place_compat<T>(f: impl FnOnce() -> T) -> T {
    match tokio::runtime::Handle::try_current() {
        Ok(handle) if handle.runtime_flavor() == tokio::runtime::RuntimeFlavor::MultiThread => {
            tracing::debug!(strategy = "block_in_place", "git: entering blocking gix work");
            tokio::task::block_in_place(f)
        }
        Ok(_) => {
            tracing::debug!(
                strategy = "direct",
                "git: entering blocking gix work on a current-thread runtime"
            );
            f()
        }
        Err(_) => {
            tracing::debug!(strategy = "no-runtime", "git: entering blocking gix work");
            f()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Limits;

    fn word(s: &str) -> Value {
        Value::String(s.to_string())
    }

    #[test]
    fn tool_rejects_an_unusable_name() {
        let err = tool(GitConfig::read_only().with_tool_name("")).expect_err("empty name");
        assert!(matches!(err, ConfigError::UnusableToolName { .. }));
        let err = tool(GitConfig::read_only().with_tool_name("my git")).expect_err("spaced name");
        assert!(matches!(err, ConfigError::UnusableToolName { .. }));
    }

    #[test]
    fn tool_rejects_a_config_with_no_verbs() {
        let err = tool(
            GitConfig::read_only()
                .without_verb(Verb::Info)
                .without_verb(Verb::Status)
                .without_verb(Verb::Log)
                .without_verb(Verb::Ls)
                .without_verb(Verb::Show),
        )
        .expect_err("a tool with no verbs cannot run anything");
        assert_eq!(err, ConfigError::NoVerbsEnabled);
    }

    #[test]
    fn schema_carries_only_the_enabled_verbs() {
        let full = tool(GitConfig::read_only()).expect("read-only config").schema();
        let names: Vec<&str> = full.subcommands.iter().map(|s| s.name.as_str()).collect();
        assert_eq!(names, ["info", "status", "log", "ls", "show"]);

        // Subtract one, and only the others survive — the schema is built from
        // the config, so a disabled verb is absent, not merely rejected.
        let narrowed = tool(GitConfig::read_only().without_verb(Verb::Status))
            .expect("the rest is a valid config")
            .schema();
        let names: Vec<&str> = narrowed.subcommands.iter().map(|s| s.name.as_str()).collect();
        assert_eq!(names, ["info", "log", "ls", "show"]);

        // Subtract a different one, to prove the removal tracks the config
        // rather than the last verb in the list.
        let narrowed = tool(GitConfig::read_only().without_verb(Verb::Show))
            .expect("the rest is a valid config")
            .schema();
        let names: Vec<&str> = narrowed.subcommands.iter().map(|s| s.name.as_str()).collect();
        assert_eq!(names, ["info", "status", "log", "ls"]);
    }

    /// The disabled verb must vanish from the schema, because that is what
    /// makes it unroutable rather than merely rejected (E.1).
    #[test]
    fn a_subtracted_verb_is_absent_from_the_schema_and_unroutable() {
        // `tool()` refuses an empty verb set, so exercise the schema builder
        // directly with a config that has every verb subtracted.
        let git = GitTool {
            config: GitConfig::read_only()
                .without_verb(Verb::Info)
                .without_verb(Verb::Status)
                .without_verb(Verb::Log)
                .without_verb(Verb::Ls)
                .without_verb(Verb::Show),
        };
        let schema = git.schema();
        assert!(
            schema.subcommands.is_empty(),
            "a subtracted verb must not appear in the schema"
        );
        let err = route(&schema, &[word("info")]).expect_err("nothing to route to");
        assert_eq!(err.exit_code(), 2);
    }

    #[test]
    fn verb_enabled_refuses_with_exit_five() {
        let cfg = GitConfig::read_only().without_verb(Verb::Info);
        let err = verb_enabled(&cfg, Verb::Info, "info").expect_err("verb is subtracted");
        assert_eq!(err.exit_code(), 5, "profile refusal is exit 5");
        verb_enabled(&GitConfig::read_only(), Verb::Info, "info").expect("verb is enabled");
    }

    #[test]
    fn route_selects_the_verb_and_reports_what_it_consumed() {
        let schema = tool(GitConfig::read_only()).expect("config").schema();
        let (verb, consumed) = route(&schema, &[word("info")]).expect("info routes");
        assert_eq!(verb, "info");
        assert_eq!(consumed, 1);
    }

    /// Extra positionals belong to the leaf, not to routing — the same rule
    /// `select_leaf` follows when it stops descending.
    #[test]
    fn route_stops_at_the_first_non_verb_word() {
        let schema = tool(GitConfig::read_only()).expect("config").schema();
        let (verb, consumed) =
            route(&schema, &[word("info"), word("extra")]).expect("info still routes");
        assert_eq!(verb, "info");
        assert_eq!(consumed, 1, "'extra' is the leaf's argument, not a verb");
    }

    #[test]
    fn route_without_a_verb_is_a_usage_error_naming_the_options() {
        let schema = tool(GitConfig::read_only()).expect("config").schema();
        let err = route(&schema, &[]).expect_err("a verb is required");
        assert_eq!(err.exit_code(), 2);
        assert!(err.to_string().contains("info"), "{err}");

        let err = route(&schema, &[word("nonesuch")]).expect_err("unknown verb");
        assert_eq!(err.exit_code(), 2);
        assert!(err.to_string().contains("nonesuch"), "{err}");
    }

    #[test]
    fn longest_prefix_mount_picks_the_innermost() {
        let mounts = vec![
            MountInfo {
                path: PathBuf::from("/"),
                read_only: false,
                resident_bytes: None,
            },
            MountInfo {
                path: PathBuf::from("/mnt/repos"),
                read_only: false,
                resident_bytes: None,
            },
        ];
        let m = longest_prefix_mount(&mounts, Path::new("/mnt/repos/kaish/src"))
            .expect("a mount contains it");
        assert_eq!(m.path, PathBuf::from("/mnt/repos"));

        let m = longest_prefix_mount(&mounts, Path::new("/elsewhere")).expect("root contains it");
        assert_eq!(m.path, PathBuf::from("/"));
    }

    #[test]
    fn to_vfs_path_maps_back_through_the_mount() {
        assert_eq!(
            to_vfs_path(
                Path::new("/srv/repos/kaish"),
                Path::new("/srv/repos"),
                Path::new("/mnt")
            ),
            Some("/mnt/kaish".to_string())
        );
        assert_eq!(
            to_vfs_path(
                Path::new("/elsewhere/kaish"),
                Path::new("/srv/repos"),
                Path::new("/mnt")
            ),
            None,
            "a path outside the mount has no VFS name"
        );
    }

    /// Both scheduling paths must run the closure exactly once and return its
    /// value. The current-thread case is the one that would panic if we
    /// reached for `block_in_place` unconditionally.
    #[test]
    fn block_in_place_compat_runs_without_a_runtime() {
        assert_eq!(block_in_place_compat(|| 42), 42);
    }

    #[tokio::test]
    async fn block_in_place_compat_runs_on_a_current_thread_runtime() {
        assert_eq!(block_in_place_compat(|| 42), 42);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn block_in_place_compat_runs_on_a_multi_thread_runtime() {
        assert_eq!(block_in_place_compat(|| 42), 42);
    }

    /// Every implemented verb has at least one example, and `examples_for`
    /// drops exactly the ones belonging to a subtracted verb — driven by
    /// `Verb::ALL`, so a verb added later needs an `EXAMPLES` entry to be
    /// visible in `help git` at all, but no change here to stay filtered
    /// correctly once it has one.
    #[test]
    fn examples_are_filtered_by_the_config_and_every_verb_has_one() {
        let full = GitConfig::read_only();
        let all: Vec<&str> = examples_for(&full).map(|(_, code)| code).collect();
        for verb in Verb::ALL {
            assert!(
                all.iter().any(|code| code.split_whitespace().nth(1) == Some(verb.as_str())),
                "{verb:?} has no EXAMPLES entry, so help git would never name \
                 it even when enabled"
            );
        }

        for verb in Verb::ALL {
            let narrowed = GitConfig::read_only().without_verb(*verb);
            let remaining: Vec<&str> = examples_for(&narrowed).map(|(_, code)| code).collect();
            assert!(
                !remaining
                    .iter()
                    .any(|code| code.split_whitespace().nth(1) == Some(verb.as_str())),
                "an example for disabled verb {verb:?} survived filtering: {remaining:?}"
            );
            // Negative control: every other verb's examples survive.
            for other in Verb::ALL {
                if other == verb {
                    continue;
                }
                assert!(
                    remaining
                        .iter()
                        .any(|code| code.split_whitespace().nth(1) == Some(other.as_str())),
                    "disabling {verb:?} dropped {other:?}'s example too: {remaining:?}"
                );
            }
        }
    }

    #[test]
    fn capabilities_report_the_configured_limits() {
        let limits = Limits {
            max_rows: 7,
            ..Limits::default()
        };
        let git = tool(GitConfig::read_only().with_limits(limits)).expect("config");
        let caps = git.capabilities();
        assert_eq!(caps.limits.max_rows, 7);
        assert_eq!(caps.profiles, ["read"]);
        assert_eq!(caps.verbs.len(), Verb::ALL.len());
        for verb in Verb::ALL {
            assert!(
                caps.verbs.contains(&verb.as_str().to_string()),
                "{verb:?} missing from capabilities.verbs: {:?}",
                caps.verbs
            );
        }
    }

    // E.1's router drift test — comparing this file's `route()` against the
    // kernel's own dispatch — lives in `tests/router_kernel_drift.rs`, not
    // here. The kernel's `select_leaf` is `pub(crate)` to `kaish-kernel`
    // (unreachable even as a dev-dependency), so that test drives a real
    // `kaish_kernel::Kernel` through `Kernel::execute` instead of calling
    // kernel internals directly — exercising the kernel's actual dispatch
    // path rather than a belief about it.
}
