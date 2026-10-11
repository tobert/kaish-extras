//! The kaish [`Tool`] implementation: schema tree, argv routing, the
//! `resolve_real_path` bridge, and the blocking seam (architecture.md E).

use std::path::{Path, PathBuf};

use async_trait::async_trait;
use clap::{CommandFactory, Parser};

use kaish_tool_api::{schema_tree_from_clap, Tool, ToolCtx};
use kaish_types::backend::MountInfo;
use kaish_types::{ExecResult, OutputData, ToolArgs, ToolSchema, Value};

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
                max_patch_content_bytes: limits.max_patch_content_bytes,
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
    async fn run_info(&self, argv: &[String], ctx: &mut dyn ToolCtx) -> ExecResult {
        const OP: &str = "info";
        if let Err(e) = verb_enabled(&self.config, Verb::Info, OP) {
            return failure(e);
        }

        let parsed = match parse_leaf::<verbs::info::InfoArgs>(argv, OP) {
            Ok(p) => p,
            Err(result) => return *result,
        };
        parsed.global.apply(ctx);

        if let Err(e) = no_operands(OP, &operands(&parsed.operands, argv)) {
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
    async fn run_status(&self, argv: &[String], ctx: &mut dyn ToolCtx) -> ExecResult {
        const OP: &str = "status";
        if let Err(e) = verb_enabled(&self.config, Verb::Status, OP) {
            return failure(e);
        }

        let parsed = match parse_leaf::<verbs::status::StatusArgs>(argv, OP) {
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
        path_args.extend(operands(&parsed.operands, argv).all());

        let outcome = block_in_place_compat(move || {
            let repo = ReadRepo::discover(OP, &resolved.real, &resolved.ceiling)?;
            // `--path` values are relative to the caller's cwd within the repo,
            // exactly as git's pathspecs are. Prefix them with the cwd's
            // repo-relative directory so a filter from a subdirectory means what
            // it says. A pathspec-magic value (leading `:`) is left untouched so
            // the parser can reject it by name rather than hide it behind a
            // prefix.
            let base = PathBase::new(&resolved, &repo);
            let paths = base.repo_paths(OP, &path_args)?;
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
        // A path that was not compared is reported the same way truncation is:
        // in the model, and again on stderr (E.5). The exit code stays 0 — the
        // report is a successful answer about every other path.
        if model.totals.blob_capped > 0 {
            if !result.err.is_empty() {
                result.err.push('\n');
            }
            let n = model.totals.blob_capped;
            result.err.push_str(&format!(
                "git status: {n} tracked {} not compared — over this build's \
                 {max_blob_bytes}-byte cap (GitConfig limits, max_blob_bytes). \
                 Each is marked 'blob_capped' in --json, with a null worktree \
                 column, and '~' in the text surface. Raise the cap to compare \
                 them",
                if n == 1 { "file was" } else { "files were" }
            ));
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
    async fn run_log(&self, argv: &[String], ctx: &mut dyn ToolCtx) -> ExecResult {
        const OP: &str = "log";
        if let Err(e) = verb_enabled(&self.config, Verb::Log, OP) {
            return failure(e);
        }

        let parsed = match parse_leaf::<verbs::log::LogArgs>(argv, OP) {
            Ok(p) => p,
            Err(result) => return *result,
        };
        parsed.global.apply(ctx);

        // `--patch` is refused before anything is read, and the refusal has to
        // say the true thing in both builds: without `textdiff` there is no
        // patch machinery at all, and with it `git log` still assembles none.
        // Answering a `--patch` request with a stat — or with a silently
        // flag-less log — would be a wrong answer rather than a missing one
        // (E.5's precedent).
        if parsed.patch {
            #[cfg(not(feature = "textdiff"))]
            return failure(GitError::PatchNeedsTextdiff {
                operation: OP,
                flag: "--patch",
                instead: "Use --stat for the changed-file and line counts this \
                          build does compute.",
            });
            #[cfg(feature = "textdiff")]
            return failure(GitError::PatchNotInThisVerb {
                operation: OP,
                flag: "--patch",
                instead: "One commit's patch is 'git diff --patch \
                          \"<commit>~1\" <commit>'; --stat gives every \
                          commit's changed-file and line counts.",
            });
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
        // `git log [<rev>] [<path>...] [-- <path>...]`, git's own shape. Each
        // word before `--` is sorted the way `git diff` sorts it
        // (`classify_operands`): a revision when it resolves as one, a path
        // when it matches a tracked path, refused when it is both or neither.
        // That needs the repository, so it happens below.
        let ops = operands(&parsed.operands, argv);
        let rev_flag = (parsed.rev != verbs::log::DEFAULT_REV).then(|| parsed.rev.clone());
        let author = parsed.author.clone();
        let first_parent = parsed.first_parent;
        let body = parsed.body;
        let stat = parsed.stat;
        let oneline = parsed.oneline;
        // Operands after `--` are pathspecs, joining the `--path` flags.
        let mut path_args = parsed.path.clone();
        path_args.extend(ops.after.iter().cloned());
        let words = ops.before.clone();
        let marked = ops.marked;

        let outcome = block_in_place_compat(move || {
            let repo = ReadRepo::discover(OP, &resolved.real, &resolved.ceiling)?;
            // Pathspecs are relative to the caller's cwd within the repo,
            // exactly as git's are — the same rule `status` and `diff` use.
            let base = PathBase::new(&resolved, &repo);
            let sorted = verbs::diff::classify_operands(OP, &repo, &words, marked, &|w| base.repo_path(OP, w))?;
            let mut paths = base.repo_paths(OP, &path_args)?;
            paths.extend(sorted.paths);
            let rev = match (sorted.revs.as_slice(), rev_flag) {
                ([], None) => verbs::log::DEFAULT_REV.to_string(),
                ([], Some(flag)) => flag,
                ([one], None) => one.clone(),
                // Both spellings, disagreeing. Picking one silently would
                // answer about a revision the caller did not choose.
                ([one], Some(flag)) => {
                    return Err(GitError::Usage {
                        operation: OP,
                        message: format!("got a revision twice — '{one}' and '--rev {flag}'. Give it once"),
                    })
                }
                (many, _) => {
                    return Err(GitError::Usage {
                        operation: OP,
                        message: format!(
                            "takes at most one revision, but got '{}'. A range is \
                             not supported here; paths go after '--'",
                            many.join("' '")
                        ),
                    })
                }
            };
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

        let mut result = ExecResult::with_output(crate::render::log(&model, oneline));
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
    async fn run_ls(&self, argv: &[String], ctx: &mut dyn ToolCtx) -> ExecResult {
        const OP: &str = "ls";
        if let Err(e) = verb_enabled(&self.config, Verb::Ls, OP) {
            return failure(e);
        }

        let parsed = match parse_leaf::<verbs::ls::LsArgs>(argv, OP) {
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
        let ops = operands(&parsed.operands, argv).all();
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
        let path_arg = ops.get(1).cloned();

        // The embedder's `max_rows` is a hard cap; `--limit` may only lower it.
        let limit = parsed.limit.min(self.config.limits().max_rows);
        let recursive = parsed.recursive;

        let outcome = block_in_place_compat(move || {
            let repo = ReadRepo::discover(OP, &resolved.real, &resolved.ceiling)?;
            // A path is read from the cwd, and with none the cwd itself is
            // listed — git's `ls-tree` from a subdirectory. A `<rev>:<path>`
            // revision names its path from the root, as git's does, so the
            // cwd does not apply to it.
            let path = match (&path_arg, rev.contains(':')) {
                (None, true) => String::new(),
                (arg, _) => PathBase::new(&resolved, &repo)
                    .repo_path(OP, arg.as_deref().unwrap_or(""))?,
            };
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
    async fn run_show(&self, argv: &[String], ctx: &mut dyn ToolCtx) -> ExecResult {
        const OP: &str = "show";
        if let Err(e) = verb_enabled(&self.config, Verb::Show, OP) {
            return failure(e);
        }

        let parsed = match parse_leaf::<verbs::show::ShowArgs>(argv, OP) {
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
        let ops = operands(&parsed.operands, argv).all();
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
            verbs::show::ShowOutcome::Commit(show) => {
                let mut r = ExecResult::with_output(crate::render::show_commit(&show));
                if show.files.is_none() {
                    r.err = format!(
                        "git show: {} is a merge, so its files are not listed. \
                         'git diff --from \"{}^1\" --to {}' lists what it changed \
                         against its first parent",
                        show.commit.short_oid, show.commit.short_oid, show.commit.short_oid
                    );
                } else if show.files_truncated {
                    r.err = format!(
                        "git show: file list truncated at {} files (--limit); \
                         'files_truncated' is true in --json",
                        show.files.as_ref().map_or(0, Vec::len)
                    );
                }
                (r, "commit")
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

    #[tracing::instrument(
        level = "info",
        name = "git.verb",
        skip_all,
        fields(verb = "diff", repo)
    )]
    async fn run_diff(&self, argv: &[String], json: bool, ctx: &mut dyn ToolCtx) -> ExecResult {
        const OP: &str = "diff";
        if let Err(e) = verb_enabled(&self.config, Verb::Diff, OP) {
            return failure(e);
        }

        let parsed = match parse_leaf::<verbs::diff::DiffArgs>(argv, OP) {
            Ok(p) => p,
            Err(result) => return *result,
        };
        parsed.global.apply(ctx);

        // Without `textdiff` both flags are refused before anything is read:
        // this build assembles no unified-diff text, and answering either with
        // the default table would be a wrong answer rather than a missing one
        // (E.5).
        #[cfg(not(feature = "textdiff"))]
        if parsed.patch {
            return failure(GitError::PatchNeedsTextdiff {
                operation: OP,
                flag: "--patch",
                instead: "The default output already reports every changed \
                          file with its added and deleted line counts; \
                          --name-only reports the paths alone.",
            });
        }
        #[cfg(not(feature = "textdiff"))]
        if parsed.context.is_some() {
            return failure(GitError::PatchNeedsTextdiff {
                operation: OP,
                flag: "--context",
                instead: "Only --patch output has hunks for --context to \
                          size, and this build produces none.",
            });
        }
        // With it, `--context` still needs something to size.
        #[cfg(feature = "textdiff")]
        if parsed.context.is_some() && !parsed.patch {
            return failure(GitError::Usage {
                operation: OP,
                message: "--context sizes the context around each --patch \
                          hunk, and without --patch there are no hunks to \
                          size. Add --patch, or drop --context"
                    .to_string(),
            });
        }

        let resolved = match resolve_repo_paths(OP, ctx, parsed.repo.as_deref()) {
            Ok(r) => r,
            Err(e) => return failure(e),
        };
        tracing::Span::current().record("repo", tracing::field::display(resolved.real.display()));

        // git's spelling: `git diff A B -- <path>...`. The words before the
        // marker are sorted into revisions and paths once the repository is
        // open, because only the repository can say which a word is.
        let ops = operands(&parsed.operands, argv);
        let mut paths = parsed.path.clone();
        paths.extend(ops.after.iter().cloned());

        let flag_endpoints = match (parsed.staged, parsed.from.clone(), parsed.to.clone()) {
            // clap's `conflicts_with_all` refuses --staged beside --from/--to,
            // so the revisions are None here by construction.
            (true, _, _) => verbs::diff::Endpoints::HeadToIndex,
            (false, None, None) => verbs::diff::Endpoints::IndexToWorktree,
            (false, Some(from), None) => verbs::diff::Endpoints::RevToWorktree { from },
            (false, from, Some(to)) => verbs::diff::Endpoints::RevToRev {
                from: from.unwrap_or_else(|| verbs::diff::DEFAULT_FROM_REV.to_string()),
                to,
            },
        };
        let named_by_flag = if parsed.staged {
            Some("--staged")
        } else if parsed.from.is_some() || parsed.to.is_some() {
            Some("--from/--to")
        } else {
            None
        };
        let words = ops.before.clone();
        let marked = ops.marked;

        // The embedder's `max_diff_files` is a hard cap; `--limit` may only
        // lower it. Not `max_rows`: a diff's rows are files, and C.1 gives
        // them their own cap.
        let limit = parsed.limit.min(self.config.limits().max_diff_files);
        // Not lowerable by an argument: this one caps a read, not an output.
        let max_blob_bytes = self.config.limits().max_blob_bytes;
        // On by default, matching git since 2.9. `--no-find-renames` is the
        // only way to turn it off; `--find-renames` is accepted so a caller
        // can be explicit, and clap refuses the pair.
        let find_renames = !parsed.no_find_renames;
        let name_only = parsed.name_only;
        // Not lowerable by an argument either: `--context` decides how much
        // context a hunk carries, and this decides how much hunk text one file
        // may cost whatever the caller asked for.
        #[cfg(feature = "textdiff")]
        let patch_wanted = parsed.patch;
        // An embedder's renderer replaces the patch text and nothing else:
        // `--json` reports this build's own model and hunks, so it is not
        // asked there, and without `--patch` there is no patch to render.
        #[cfg(feature = "textdiff")]
        let renderer = if patch_wanted && !(json || parsed.global.json) {
            self.config.patch_renderer().cloned()
        } else {
            None
        };
        #[cfg(not(feature = "textdiff"))]
        let _ = json;
        #[cfg(feature = "textdiff")]
        let context = parsed.context.unwrap_or(verbs::diff::DEFAULT_CONTEXT);
        // No hunks of ours when a renderer will make its own.
        #[cfg(feature = "textdiff")]
        let hunks = (patch_wanted && renderer.is_none()).then(|| crate::diffcore::HunkOptions {
            context: u32::try_from(context).unwrap_or(u32::MAX),
            max_bytes: self.config.limits().max_hunk_bytes_per_file,
        });
        #[cfg(feature = "textdiff")]
        let max_patch_content_bytes = self.config.limits().max_patch_content_bytes;
        #[cfg(feature = "textdiff")]
        let patch_opts = crate::renderer::PatchOptions {
            context,
            max_hunk_bytes_per_file: self.config.limits().max_hunk_bytes_per_file,
            word_diff: false,
        };

        let outcome = block_in_place_compat(move || {
            let repo = ReadRepo::discover(OP, &resolved.real, &resolved.ceiling)?;
            // Pathspecs are relative to the caller's cwd, as in git and as in
            // `status` and `log`.
            let base = PathBase::new(&resolved, &repo);
            let sorted = verbs::diff::classify_operands(OP, &repo, &words, marked, &|w| base.repo_path(OP, w))?;
            let mut paths = base.repo_paths(OP, &paths)?;
            paths.extend(sorted.paths);
            let endpoints = match (verbs::diff::endpoints_from_revs(&sorted.revs)?, named_by_flag) {
                (None, _) => flag_endpoints,
                (Some(endpoints), None) => endpoints,
                // Two ways of naming the ends, both used. Picking one would
                // answer a question the caller did not finish asking.
                (Some(_), Some("--staged")) => {
                    return Err(GitError::Usage {
                        operation: OP,
                        message: format!(
                            "comparing a revision with the index is not \
                             supported, and '{}' beside --staged asks for \
                             that. 'git diff --staged' compares HEAD with the \
                             index; 'git diff {}' compares {} with the \
                             working tree",
                            sorted.revs.join(" "),
                            verbs::diff::kaish_word(&sorted.revs[0]),
                            sorted.revs[0]
                        ),
                    });
                }
                (Some(_), Some(_)) => {
                    return Err(GitError::Usage {
                        operation: OP,
                        message: format!(
                            "got revisions twice, as '{}' and as --from/--to. \
                             Name them one way: 'git diff A B' or \
                             'git diff --from A --to B'",
                            sorted.revs.join(" ")
                        ),
                    });
                }
            };
            let opts = verbs::diff::DiffOptions {
                endpoints,
                paths,
                name_only,
                find_renames,
                limit,
                max_blob_bytes,
                #[cfg(feature = "textdiff")]
                hunks,
            };
            let root = repo.root().display().to_string();
            // The renderer runs here, inside the blocking section, beside the
            // content it is handed — which never leaves this closure.
            #[cfg(feature = "textdiff")]
            if let Some(renderer) = renderer {
                let (model, contents) =
                    verbs::diff::run_with_content(&repo, &opts, max_patch_content_bytes)?;
                let files = verbs::diff::changed_files(&model, &contents);
                let patch = renderer.render(&files, &patch_opts).map_err(|e| {
                    GitError::PatchRendererFailed {
                        operation: OP,
                        message: error_chain(&*e),
                    }
                })?;
                let count = |why| contents.iter().filter(|c| c.withheld == Some(why)).count();
                let over_blob = count(crate::renderer::Withheld::OverMaxBlobBytes);
                let over_budget = count(crate::renderer::Withheld::OverMaxPatchContentBytes);
                return Ok((model, root, Some(Rendered { patch, over_blob, over_budget })));
            }
            let rendered: MaybeRendered = None;
            verbs::diff::run(&repo, &opts).map(|model| (model, root, rendered))
        });

        let (model, repo_root, rendered) = match outcome {
            Ok(triple) => triple,
            Err(e) => return failure(e),
        };

        // How many files the renderer got sizes and no content for, when
        // there was one.
        #[cfg(feature = "textdiff")]
        let withheld = rendered.as_ref().map(|r| (r.over_blob, r.over_budget));
        #[cfg(feature = "textdiff")]
        let (data, text, sideband) = match rendered {
            Some(Rendered { patch, .. }) => {
                let (data, text) = crate::render::diff_rendered(&model, patch.text);
                (data, text, patch.data)
            }
            None if patch_wanted => {
                let (data, text) = crate::render::diff_patch(&model);
                (data, text, None)
            }
            None => {
                let (data, text) = crate::render::diff(&model);
                (data, text, None)
            }
        };
        #[cfg(not(feature = "textdiff"))]
        let _ = rendered;
        #[cfg(not(feature = "textdiff"))]
        let (data, text) = crate::render::diff(&model);

        let mut result = ExecResult::with_output_and_text(data, text);
        // The renderer's structured data is the pipeline's structured input
        // to the next stage. It does not change what `x=$(git diff --patch)`
        // binds: the schema leaves `typed_substitution` off, so a
        // substitution binds the text (AGENTS.md, "Command substitution
        // binds text"; pinned by `tests/patch_renderer.rs`).
        #[cfg(feature = "textdiff")]
        if let Some(value) = sideband {
            result.data = Some(kaish_types::json_to_value_no_envelope(value));
        }
        let mut notes: Vec<String> = Vec::new();
        // The patch payload is a patch and nothing else — `git diff --patch |
        // git apply` must not have to skip a preamble — so the endpoints B.4
        // asks every result to state move to stderr, beside `--json`.
        #[cfg(feature = "textdiff")]
        if patch_wanted {
            notes.push(format!(
                "{} → {}",
                crate::render::endpoint_text(&model.from),
                crate::render::endpoint_text(&model.to)
            ));
            // Counted from what the renderer was handed, not from
            // `lines_capped`: a renamed file over the cap is withheld from
            // the renderer and still `0/0`, uncapped, in the model.
            if let Some((over_blob, over_budget)) = withheld {
                if over_blob > 0 {
                    notes.push(format!(
                        "{over_blob} file(s) have a side over max_blob_bytes, \
                         so the patch renderer got their sizes and no content"
                    ));
                }
                if over_budget > 0 {
                    notes.push(format!(
                        "{over_budget} file(s) came after the call's \
                         {max_patch_content_bytes}-byte content budget was \
                         spent (max_patch_content_bytes), so the patch \
                         renderer got their sizes and no content; narrow the \
                         diff with a path to see them"
                    ));
                }
            } else if model.totals.lines_capped > 0 {
                notes.push(format!(
                    "{} file(s) were not read: a side was over \
                     max_blob_bytes, so their counts and hunks are absent; \
                     'lines_capped' says which file",
                    model.totals.lines_capped
                ));
            }
            if model.totals.hunks_capped > 0 {
                notes.push(format!(
                    "{} file(s) have their hunks cut short at \
                     max_hunk_bytes_per_file; their counts are exact and \
                     'hunks_capped' says which file",
                    model.totals.hunks_capped
                ));
            }
        }
        if model.truncated {
            notes.push(format!(
                "output truncated at {} files (--limit); 'truncated' is true \
                 in --json",
                model.files.len()
            ));
        }
        if model.unmerged > 0 {
            notes.push(format!(
                "{} unmerged path(s) have no stage 0 to compare and are not \
                 in this diff; 'unmerged' says so in --json, and `git status` \
                 reports their state",
                model.unmerged
            ));
        }
        if !notes.is_empty() {
            result.err = format!("git diff: {}", notes.join("; "));
        }
        result.baggage.insert("git.repo".to_string(), repo_root);
        result
    }

    #[tracing::instrument(
        level = "info",
        name = "git.verb",
        skip_all,
        fields(verb = "branch", repo)
    )]
    async fn run_branch(&self, argv: &[String], ctx: &mut dyn ToolCtx) -> ExecResult {
        const OP: &str = "branch";
        if let Err(e) = verb_enabled(&self.config, Verb::Branch, OP) {
            return failure(e);
        }

        let parsed = match parse_leaf::<verbs::branch::BranchArgs>(argv, OP) {
            Ok(p) => p,
            Err(result) => return *result,
        };
        parsed.global.apply(ctx);

        // After `--list`, operands are name patterns; otherwise there are none.
        let patterns = operands(&parsed.operands, argv).all();
        if !parsed.list {
            if let Err(e) = no_operands(OP, &operands(&parsed.operands, argv)) {
                return failure(e);
            }
        }

        let resolved = match resolve_repo_paths(OP, ctx, parsed.repo.as_deref()) {
            Ok(r) => r,
            Err(e) => return failure(e),
        };
        tracing::Span::current().record("repo", tracing::field::display(resolved.real.display()));

        if parsed.show_current {
            let head = block_in_place_compat(move || {
                let repo = ReadRepo::discover(OP, &resolved.real, &resolved.ceiling)?;
                repo.head()
            });
            return match head {
                // git prints the name alone, and nothing for a detached HEAD.
                Ok(head) => {
                    let text = head.branch.as_ref().map(|b| format!("{b}\n")).unwrap_or_default();
                    ExecResult::with_output(
                        OutputData::text(text).with_rich_json(serde_json::json!({ "branch": head.branch })),
                    )
                }
                Err(e) => failure(e),
            };
        }

        // clap's `conflicts_with` refuses `--all --remote`, so the pair is
        // unreachable and the default is the local namespace alone.
        let scope = match (parsed.all, parsed.remote) {
            (true, _) => verbs::branch::Scope::All,
            (false, true) => verbs::branch::Scope::Remote,
            (false, false) => verbs::branch::Scope::Local,
        };
        // The embedder's `max_rows` is a hard cap; `--limit` may only lower it.
        let limit = parsed.limit.min(self.config.limits().max_rows);
        let contains = parsed.contains.clone();
        let merged = parsed.merged.clone();
        let ahead_behind = parsed.ahead_behind;

        let outcome = block_in_place_compat(move || {
            let repo = ReadRepo::discover(OP, &resolved.real, &resolved.ceiling)?;
            let opts = verbs::branch::BranchOptions {
                scope,
                contains,
                merged,
                ahead_behind,
                limit,
                patterns,
            };
            let root = repo.root().display().to_string();
            verbs::branch::run(&repo, &opts).map(|model| (model, root))
        });

        let (model, repo_root) = match outcome {
            Ok(pair) => pair,
            Err(e) => return failure(e),
        };

        let mut result = ExecResult::with_output(crate::render::branch(&model));
        if model.truncated {
            result.err = format!(
                "git branch: output truncated at {} branches (--limit); \
                 'truncated' is true in --json",
                model.branches.len()
            );
        }
        result.baggage.insert("git.repo".to_string(), repo_root);
        result
    }

    #[tracing::instrument(
        level = "info",
        name = "git.verb",
        skip_all,
        fields(verb = "tag", repo)
    )]
    async fn run_tag(&self, argv: &[String], ctx: &mut dyn ToolCtx) -> ExecResult {
        const OP: &str = "tag";
        if let Err(e) = verb_enabled(&self.config, Verb::Tag, OP) {
            return failure(e);
        }

        let parsed = match parse_leaf::<verbs::tag::TagArgs>(argv, OP) {
            Ok(p) => p,
            Err(result) => return *result,
        };
        parsed.global.apply(ctx);

        // After `--list`, operands are name patterns; otherwise there are none.
        let patterns = operands(&parsed.operands, argv).all();
        if !parsed.list {
            if let Err(e) = no_operands(OP, &operands(&parsed.operands, argv)) {
                return failure(e);
            }
        }

        let resolved = match resolve_repo_paths(OP, ctx, parsed.repo.as_deref()) {
            Ok(r) => r,
            Err(e) => return failure(e),
        };
        tracing::Span::current().record("repo", tracing::field::display(resolved.real.display()));

        let limit = parsed.limit.min(self.config.limits().max_rows);
        let contains = parsed.contains.clone();

        let outcome = block_in_place_compat(move || {
            let repo = ReadRepo::discover(OP, &resolved.real, &resolved.ceiling)?;
            let opts = verbs::tag::TagOptions {
                contains,
                limit,
                patterns,
            };
            let root = repo.root().display().to_string();
            verbs::tag::run(&repo, &opts).map(|model| (model, root))
        });

        let (model, repo_root) = match outcome {
            Ok(pair) => pair,
            Err(e) => return failure(e),
        };

        let mut result = ExecResult::with_output(crate::render::tag(&model));
        if model.truncated {
            result.err = format!(
                "git tag: output truncated at {} tags (--limit); 'truncated' \
                 is true in --json",
                model.tags.len()
            );
        }
        result.baggage.insert("git.repo".to_string(), repo_root);
        result
    }

    #[tracing::instrument(
        level = "info",
        name = "git.verb",
        skip_all,
        fields(verb = "worktree list", repo)
    )]
    async fn run_worktree_list(&self, argv: &[String], ctx: &mut dyn ToolCtx) -> ExecResult {
        const OP: &str = "worktree list";
        if let Err(e) = verb_enabled(&self.config, Verb::WorktreeList, OP) {
            return failure(e);
        }

        let parsed = match parse_leaf::<verbs::worktree::WorktreeListArgs>(argv, OP) {
            Ok(p) => p,
            Err(result) => return *result,
        };
        parsed.global.apply(ctx);

        if let Err(e) = no_operands(OP, &operands(&parsed.operands, argv)) {
            return failure(e);
        }

        let resolved = match resolve_repo_paths(OP, ctx, parsed.repo.as_deref()) {
            Ok(r) => r,
            Err(e) => return failure(e),
        };
        tracing::Span::current().record("repo", tracing::field::display(resolved.real.display()));

        let limit = parsed.limit.min(self.config.limits().max_rows);
        // B.9's `path_vfs`: each working tree is mapped back through the mount
        // the caller reached this repository by, and one that falls outside it
        // is reported with a null rather than a VFS path that resolves
        // somewhere else.
        let mount_real = resolved.mount_real.clone();
        let mount_vfs = resolved.mount_vfs.clone();

        let outcome = block_in_place_compat(move || {
            let repo = ReadRepo::discover(OP, &resolved.real, &resolved.ceiling)?;
            let opts = verbs::worktree::WorktreeListOptions {
                limit,
                mount_real,
                mount_vfs,
            };
            let root = repo.root().display().to_string();
            verbs::worktree::run(&repo, &opts).map(|model| (model, root))
        });

        let (model, repo_root) = match outcome {
            Ok(pair) => pair,
            Err(e) => return failure(e),
        };

        let mut result = ExecResult::with_output(crate::render::worktree_list(&model));
        if model.truncated {
            result.err = format!(
                "git worktree list: output truncated at {} working trees \
                 (--limit); 'truncated' is true in --json",
                model.worktrees.len()
            );
        }
        result.baggage.insert("git.repo".to_string(), repo_root);
        result
    }
}

/// How a caller's pathspec becomes repo-relative, the way git reads one:
/// relative to the cwd (from `src/`, `lib.rs` is `src/lib.rs`), with `.` and
/// `..` resolved, and an absolute path accepted when it names something
/// inside the repository. A pathspec-magic value (leading `:`) is left
/// untouched so the parser can reject it by name.
///
/// Lexical on purpose: the spec names a path in the index or a tree, which
/// may not exist on disk, and resolving it against the filesystem would
/// touch the host to answer a question about the repository.
struct PathBase {
    /// The cwd's repo-relative segments.
    prefix: Vec<String>,
    /// The repository root's VFS path, for absolute specs; `None` when the
    /// root cannot be named in the VFS.
    root_vfs: Option<String>,
}

impl PathBase {
    fn new(resolved: &ResolvedPaths, repo: &ReadRepo) -> Self {
        let prefix = cwd_prefix(&resolved.real, repo.root());
        PathBase {
            prefix: prefix.split('/').filter(|s| !s.is_empty()).map(str::to_string).collect(),
            root_vfs: to_vfs_path(repo.root(), &resolved.mount_real, &resolved.mount_vfs),
        }
    }

    fn repo_paths(&self, operation: &'static str, specs: &[String]) -> Result<Vec<String>, GitError> {
        specs.iter().map(|spec| self.repo_path(operation, spec)).collect()
    }

    fn repo_path(&self, operation: &'static str, spec: &str) -> Result<String, GitError> {
        if spec.starts_with(':') {
            return Ok(spec.to_string());
        }
        let outside = || GitError::Usage {
            operation,
            message: format!(
                "path '{spec}' is outside the repository{}. A path is read from the \
                 current directory and must stay inside the repository",
                match &self.root_vfs {
                    Some(root) => format!(" at {root}"),
                    None => String::new(),
                }
            ),
        };
        let (mut segments, rest) = match spec.strip_prefix('/') {
            Some(_) => {
                let root = self.root_vfs.as_deref().ok_or_else(outside)?;
                let rest = if spec == root {
                    ""
                } else {
                    spec.strip_prefix(root)
                        .and_then(|r| r.strip_prefix('/'))
                        .ok_or_else(outside)?
                };
                (Vec::new(), rest)
            }
            None => (self.prefix.clone(), spec),
        };
        for segment in rest.split('/') {
            match segment {
                "" | "." => {}
                ".." => {
                    segments.pop().ok_or_else(outside)?;
                }
                other => segments.push(other.to_string()),
            }
        }
        Ok(segments.join("/"))
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
        if self.config.has(Verb::Diff) {
            let diff = verbs::diff::DiffArgs::command().name("diff");
            // The default `--patch` text promises `git apply` takes the
            // output. With an embedder's renderer that is the renderer's
            // promise to make, not this build's, so it is not published.
            #[cfg(feature = "textdiff")]
            let diff = if self.config.patch_renderer().is_some() {
                diff.mut_arg("patch", |arg| arg.help(RENDERED_PATCH_HELP))
            } else {
                diff
            };
            cmd = cmd.subcommand(diff);
        }
        if self.config.has(Verb::Branch) {
            cmd = cmd.subcommand(verbs::branch::BranchArgs::command().name("branch"));
        }
        if self.config.has(Verb::Tag) {
            cmd = cmd.subcommand(verbs::tag::TagArgs::command().name("tag"));
        }
        // `worktree` is a node, not a leaf: B.11's write profiles add
        // `add`/`remove`/`lock`/`prune` beside `list` under the same word.
        // The node exists only while it has a child — a bare `git worktree`
        // advertising nothing it can run would be a promise this build cannot
        // keep, which is the same rule `without_verb` follows for a leaf.
        if self.config.has(Verb::WorktreeList) {
            cmd = cmd.subcommand(
                clap::Command::new("worktree")
                    .about(WORKTREE_DESCRIPTION)
                    .subcommand(verbs::worktree::WorktreeListArgs::command().name("list")),
            );
        }
        schema_tree_from_clap(
            &cmd,
            self.config.tool_name(),
            DESCRIPTION,
            examples_for(&self.config),
        )
        // The true argv, `--` included; see `execute`.
        .with_verbatim_argv()
    }

    async fn execute(&self, args: ToolArgs, ctx: &mut dyn ToolCtx) -> ExecResult {
        let schema = self.schema();

        // The schema declares `with_verbatim_argv`, so a kernel hands over
        // every word after `git` in source order — the caller's own `--`
        // included — and lifts only `--json`, which it applies itself. The
        // typed binder this replaced dropped `--`, so `git log main -- src`
        // read as two revisions (2026-10-01). No words means a caller built
        // `ToolArgs` without a kernel's binder; guessing its argv would be
        // guessing at the question.
        let Some(words) = &args.words else {
            return ExecResult::failure(
                2,
                "git: this call carried no `words`. kaish-tools-git binds its \
                 arguments verbatim (ToolSchema::with_verbatim_argv), so a \
                 kaish kernel supplies them; a direct caller must set \
                 ToolArgs::words to the argv after `git`"
                    .to_string(),
            );
        };
        // A binary word renders as a placeholder token, which would then be
        // read as a revision or a path nobody wrote.
        if let Some(index) = words.iter().position(|w| matches!(w, Value::Bytes(_))) {
            return ExecResult::failure(
                2,
                format!(
                    "git: argument {} is binary data. git takes text \
                     arguments: a revision, a path or a flag",
                    index + 1
                ),
            );
        }
        let argv = args.words_argv();

        let (verb, consumed) = match route(&schema, &argv) {
            Ok(r) => r,
            Err(e) => {
                // A git command this build lacks gets its own refusal: why,
                // and what to type instead.
                let known = argv.first().and_then(|word| {
                    crate::spelling::unsupported_verb(&schema.name, word, |v| self.config.has(v))
                });
                return failure(known.unwrap_or(e));
            }
        };
        let leaf = &argv[consumed..];

        // `git <verb> --help` answers from the schema, before the verb's own
        // parser sees the flag. clap's help would print the argument struct's
        // design notes, and `help git` is the published text.
        let options = leaf.iter().take_while(|w| *w != "--");
        if options.into_iter().any(|w| w == "--help" || w == "-h") {
            return verb_help(&schema, &verb);
        }

        match verb.as_str() {
            "info" => self.run_info(leaf, ctx).await,
            "status" => self.run_status(leaf, ctx).await,
            "log" => self.run_log(leaf, ctx).await,
            "ls" => self.run_ls(leaf, ctx).await,
            "show" => self.run_show(leaf, ctx).await,
            // `--json` arrives lifted into `flags` by the kernel, not in the
            // words; `diff` is the one verb that needs to know before it runs,
            // because `--json` decides whether a patch renderer is asked.
            "diff" => self.run_diff(leaf, args.has_flag("json"), ctx).await,
            "branch" => self.run_branch(leaf, ctx).await,
            "tag" => self.run_tag(leaf, ctx).await,
            "worktree list" => self.run_worktree_list(leaf, ctx).await,
            // A node with no subcommand word after it. Routing stops at the
            // node, which is correct — the caller named a group, not a verb —
            // and the answer names what the group holds rather than guessing
            // which one was meant.
            "worktree" => failure(GitError::Usage {
                operation: "worktree",
                message: "names a group of subcommands, not a verb. This \
                          build has 'git worktree list'; create, remove, lock \
                          and prune are write verbs the read profile does not \
                          carry"
                    .to_string(),
            }),
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

/// A renderer's answer, and how many files it got no content for.
#[cfg(feature = "textdiff")]
struct Rendered {
    patch: crate::renderer::RenderedPatch,
    /// Files with a side over `max_blob_bytes`.
    over_blob: usize,
    /// Files after `max_patch_content_bytes` was spent.
    over_budget: usize,
}

/// What `git diff --patch` produces when an embedder installed a renderer.
#[cfg(feature = "textdiff")]
type MaybeRendered = Option<Rendered>;
/// Never anything: a build without `textdiff` has no renderer to ask.
#[cfg(not(feature = "textdiff"))]
type MaybeRendered = Option<std::convert::Infallible>;

/// `--patch`'s published description when an embedder's renderer makes the
/// text. Replaces the derive's, which promises `git apply` compatibility.
#[cfg(feature = "textdiff")]
const RENDERED_PATCH_HELP: &str = "Report the changes as patch text from this \
    tool's patch renderer, which the embedder supplies; whether `git apply` \
    takes it is up to that renderer. The endpoints move to stderr. --json is \
    unchanged and does not use the renderer: it reports this build's own \
    model, and every file gains a `hunks` array. Needs the `textdiff` build \
    feature.";

/// An error and each of its sources, joined with ": ", so a renderer's
/// wrapped engine error reaches stderr whole.
#[cfg(feature = "textdiff")]
fn error_chain(err: &(dyn std::error::Error + 'static)) -> String {
    let mut out = err.to_string();
    let mut source = err.source();
    while let Some(next) = source {
        out.push_str(": ");
        out.push_str(&next.to_string());
        source = next.source();
    }
    out
}

/// The tool's own description, shared by the clap tree and the schema.
const DESCRIPTION: &str =
    "Read a git repository — shallow, safety-first, and read-only by construction";

/// The `worktree` node's own description. It is a group rather than a verb,
/// and the read profile carries one member of it.
const WORKTREE_DESCRIPTION: &str = "Work with the repository's working trees";

/// Examples the schema carries into `help git` and completion.
///
/// Every example's command starts `git <verb>`, and [`examples_for`] filters
/// on that word before the schema is built — an example for a verb this
/// config subtracted is exactly the kind of thing E.1's gate exists to keep
/// out of `help git`: a disabled verb must be absent from what an agent is
/// told exists, not merely refused once it tries the example. There is at
/// least one entry per implemented [`Verb`] — `help <tool>`'s renderer
/// (`kaish-help`'s `tool_help`) shows only params and examples, never a bare
/// subcommand list, so a verb with no example here would never be named in
/// `help git` at all, enabled or not.
const EXAMPLES: [(&str, &str); 18] = [
    ("What repository is this", "git info"),
    ("Inspect a specific repository", "git info --repo /mnt/repos/kaish"),
    ("Structured, for a script", "git info --json"),
    ("What changed in the working tree", "git status"),
    ("Recent commit history", "git log"),
    ("One line per commit", "git log --oneline"),
    ("Read a file as of the last release", "git show v0.1.0:src/lib.rs"),
    ("List a directory as of HEAD", "git ls HEAD src"),
    ("See the unstaged changes", "git diff"),
    ("See what is staged", "git diff --staged"),
    ("Compare two revisions under one directory", "git diff v0.1.0 HEAD -- src"),
    ("List the branches", "git branch"),
    ("Include remote-tracking branches", "git branch --all"),
    ("Find which branches carry a fix", "git branch --contains 1a2b3c4"),
    ("Count each branch against its upstream", "git branch --ahead-behind"),
    ("List the tags", "git tag"),
    ("Find which tags carry a fix", "git tag --contains 1a2b3c4"),
    ("List every working tree", "git worktree list"),
];

/// [`EXAMPLES`], narrowed to the ones whose verb this config still enables.
///
/// Driven by `Verb::ALL`/[`GitConfig::has`] rather than a hand-matched list
/// of example strings, so a verb subtracted by an embedder — or added by a
/// later phasing PR, per the coordination rule with the sibling `git diff`
/// PR — needs no change here to stay correct.
fn examples_for(config: &GitConfig) -> impl Iterator<Item = (&'static str, &'static str)> + '_ {
    EXAMPLES
        .iter()
        .copied()
        .filter(move |(_, code)| verb_of(code).is_some_and(|verb| config.has(verb)))
}

/// The verb an example command demonstrates.
///
/// Matches on the longest verb name that starts the command, because a verb
/// name can be two words (`worktree list`) and matching on the first word
/// alone would credit `git worktree list` to a `worktree` verb that does not
/// exist.
fn verb_of(code: &str) -> Option<Verb> {
    let rest = code.strip_prefix("git ")?;
    Verb::ALL
        .iter()
        .filter(|verb| {
            let name = verb.as_str();
            rest.strip_prefix(name)
                .is_some_and(|tail| tail.is_empty() || tail.starts_with(' '))
        })
        .max_by_key(|verb| verb.as_str().len())
        .copied()
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
/// Returns the selected verb path joined by spaces, and how many words it
/// consumed. Descent stops at the first word that is not a child name,
/// exactly as `select_leaf` does, so the leaf's own words are left alone.
fn route(schema: &ToolSchema, words: &[String]) -> Result<(String, usize), GitError> {
    let mut node = schema;
    let mut path: Vec<String> = Vec::new();
    let mut consumed = 0usize;

    for word in words {
        if node.subcommands.is_empty() {
            break;
        }
        let Some(child) = node.subcommands.iter().find(|c| c.matches_command(word)) else {
            break;
        };
        path.push(child.name.clone());
        node = child;
        consumed += 1;
    }

    if path.is_empty() {
        let available: Vec<&str> = schema.subcommands.iter().map(|s| s.name.as_str()).collect();
        let got = match words.first() {
            // `git --repo x status`: a flag where the verb goes. Flags belong
            // to a verb here, so they follow it.
            Some(word) if word.starts_with('-') => format!(
                "'{word}' is a flag, and flags follow the verb ('git <verb> {word} …')"
            ),
            Some(word) => format!("'{word}' is not one of them"),
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

/// `git <verb> --help`: the verb's description, flags and examples, read
/// from the same schema `help git` renders.
fn verb_help(schema: &ToolSchema, verb: &str) -> ExecResult {
    let mut node = schema;
    for word in verb.split(' ') {
        match node.subcommands.iter().find(|c| c.name == word) {
            Some(child) => node = child,
            // `route` produced this path from this schema.
            None => unreachable!("routed verb {verb:?} is not in the schema"),
        }
    }
    let mut out = format!("{} {verb} — {}\n", schema.name, node.description);
    let flags: Vec<_> = node
        .params
        .iter()
        .filter(|p| !p.positional)
        .collect();
    if !flags.is_empty() {
        out.push_str("\nFlags:\n");
        for p in flags {
            let mut spelling = format!("--{}", p.name);
            for alias in &p.aliases {
                let alias = alias.trim_start_matches('-');
                // The schema also carries the name's underscore spelling
                // (`no_merges`); it is the same flag, not one to teach.
                if alias.replace('_', "-") == p.name {
                    continue;
                }
                if alias.chars().count() == 1 {
                    spelling.push_str(&format!(", -{alias}"));
                } else {
                    spelling.push_str(&format!(", --{alias}"));
                }
            }
            if p.param_type != "bool" {
                spelling.push_str(" <VALUE>");
            }
            out.push_str(&format!("  {spelling}\n      {}\n", p.description));
        }
    }
    let prefix = format!("{} {verb}", schema.name);
    let examples: Vec<_> = schema
        .examples
        .iter()
        .filter(|e| e.code == prefix || e.code.starts_with(&format!("{prefix} ")))
        .collect();
    if !examples.is_empty() {
        out.push_str("\nExamples:\n");
        for e in examples {
            out.push_str(&format!("  {}\n      {}\n", e.code, e.description));
        }
    }
    out.push_str(&format!("\nSee 'help {}' for every verb.\n", schema.name));
    ExecResult::with_output(OutputData::text(out))
}

/// Parse the leaf's own argv — the words after the verb — with its flat clap
/// `Parser` (E.1). `route` has already taken the verb words off the front,
/// so the clap tree is never asked to route.
fn parse_leaf<P: Parser>(argv: &[String], operation: &'static str) -> Result<P, Box<ExecResult>> {
    // Boxed: `ExecResult` is a wide type, and an unboxed `Err` variant makes
    // every `Ok` return pay for it (clippy::result_large_err).
    P::try_parse_from(std::iter::once(format!("git {operation}")).chain(argv.iter().cloned()))
        .map_err(|e| Box::new(failure(crate::spelling::refuse(operation, &e, &P::command()))))
}

/// A verb's operands, split at git's `--` end-of-options marker.
///
/// The tool is bound verbatim, so the leaf argv is the caller's own, `--`
/// included. clap collects every operand into the leaf's `operands` field in
/// order, on both sides of the marker; everything after the first `--` in the
/// argv is an operand by definition, which is what places the split.
struct Operands {
    /// Positionals before `--`. A revision, for a verb that takes one.
    before: Vec<String>,
    /// Positionals after `--`. Always pathspecs, for every verb.
    after: Vec<String>,
    /// Whether the caller wrote the `--` marker. With it, git reads every
    /// word before the marker as a revision and never as a path, which is how
    /// a caller settles a word that is both.
    marked: bool,
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

/// Split the operands clap parsed (`parsed`, in order) at the caller's `--`
/// in the leaf argv.
fn operands(parsed: &[String], argv: &[String]) -> Operands {
    let marker = argv.iter().position(|word| word == "--");
    let after = marker.map_or(0, |i| argv.len() - i - 1);
    let split = parsed.len().saturating_sub(after);
    Operands {
        before: parsed[..split].to_vec(),
        after: parsed[split..].to_vec(),
        marked: marker.is_some(),
    }
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
    let all = ops.all();
    let first = crate::verbs::diff::kaish_word(&all[0]);
    // The next step depends on what the word most likely meant for this verb.
    let next = match operation {
        "branch" => format!(
            "This git lists branches and never creates one; 'git log {first}' \
             shows a branch's history"
        ),
        "tag" => format!(
            "This git lists tags and never creates one; 'git show {first}' \
             shows a tag and 'git log {first}' its history"
        ),
        _ => "Name the repository with --repo".to_string(),
    };
    Err(GitError::Usage {
        operation,
        message: format!("takes no operands, but got '{}'. {next}", all.join("' '")),
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
///
/// Lexical on purpose. Callers hand it paths that came out of repository
/// content (`git worktree list`'s registrations), and canonicalizing one that
/// points outside the mount would touch the host filesystem to answer a
/// question about the caller's own sandbox.
pub(crate) fn to_vfs_path(real: &Path, mount_real: &Path, mount_vfs: &Path) -> Option<String> {
    let rest = real.strip_prefix(mount_real).ok()?;
    // `Path::join` with an empty remainder appends a separator, so the mount
    // root itself came back as `/mnt/` rather than `/mnt` — a VFS path that
    // reads like a different one.
    if rest.as_os_str().is_empty() {
        return Some(mount_vfs.display().to_string());
    }
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

    fn word(s: &str) -> String {
        s.to_string()
    }

    /// Every routable leaf of the schema, with the verb words that reach it.
    ///
    /// The schema is a tree: `worktree` is a node with a `list` child, so a
    /// guard that only looked at `schema.subcommands` would check a node that
    /// has no parameters and skip the leaf that has them all. Written as a
    /// walk rather than a two-level special case so B.11's later worktree
    /// verbs need no change here.
    fn leaves(schema: &ToolSchema) -> Vec<(String, &ToolSchema)> {
        fn walk<'a>(node: &'a ToolSchema, path: &str, out: &mut Vec<(String, &'a ToolSchema)>) {
            for child in &node.subcommands {
                let child_path = if path.is_empty() {
                    child.name.clone()
                } else {
                    format!("{path} {}", child.name)
                };
                if child.subcommands.is_empty() {
                    out.push((child_path, child));
                } else {
                    walk(child, &child_path, out);
                }
            }
        }
        let mut out = Vec::new();
        walk(schema, "", &mut out);
        out
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
                .without_verb(Verb::Show)
                .without_verb(Verb::Diff)
                .without_verb(Verb::Branch)
                .without_verb(Verb::Tag)
                .without_verb(Verb::WorktreeList),
        )
        .expect_err("a tool with no verbs cannot run anything");
        assert_eq!(err, ConfigError::NoVerbsEnabled);
    }

    #[test]
    fn schema_carries_only_the_enabled_verbs() {
        let full = tool(GitConfig::read_only()).expect("read-only config").schema();
        let names: Vec<&str> = full.subcommands.iter().map(|s| s.name.as_str()).collect();
        assert_eq!(
            names,
            ["info", "status", "log", "ls", "show", "diff", "branch", "tag", "worktree"],
            "`worktree` is a node rather than a leaf; its `list` child is the verb"
        );
        assert_eq!(
            full.subcommands
                .iter()
                .find(|s| s.name == "worktree")
                .map(|s| s.subcommands.iter().map(|c| c.name.as_str()).collect::<Vec<_>>()),
            Some(vec!["list"])
        );

        // Subtract one, and only the others survive — the schema is built from
        // the config, so a disabled verb is absent, not merely rejected.
        let narrowed = tool(GitConfig::read_only().without_verb(Verb::Status))
            .expect("the rest is a valid config")
            .schema();
        let names: Vec<&str> = narrowed.subcommands.iter().map(|s| s.name.as_str()).collect();
        assert_eq!(names, ["info", "log", "ls", "show", "diff", "branch", "tag", "worktree"]);

        // Subtract a different one, to prove the removal tracks the config
        // rather than the last verb in the list.
        let narrowed = tool(GitConfig::read_only().without_verb(Verb::Show))
            .expect("the rest is a valid config")
            .schema();
        let names: Vec<&str> = narrowed.subcommands.iter().map(|s| s.name.as_str()).collect();
        assert_eq!(names, ["info", "status", "log", "ls", "diff", "branch", "tag", "worktree"]);

        // And subtracting the one verb under a node takes the node with it,
        // so no group is left advertising a subcommand this build cannot run.
        let narrowed = tool(GitConfig::read_only().without_verb(Verb::WorktreeList))
            .expect("the rest is a valid config")
            .schema();
        let names: Vec<&str> = narrowed.subcommands.iter().map(|s| s.name.as_str()).collect();
        assert_eq!(names, ["info", "status", "log", "ls", "show", "diff", "branch", "tag"]);
    }

    /// The same rule one level down: the schema's **leaves** are exactly the
    /// enabled verbs, spelled as an agent types them.
    ///
    /// `schema_carries_only_the_enabled_verbs` reads the top-level names and
    /// then names `worktree`'s children by hand, which is exact only while
    /// `worktree` has one child. With a second verb under that node (B.11's
    /// `add`, `remove`, `lock`, `prune`), disabling `worktree list` would
    /// leave the node standing — correctly, since a sibling still needs it —
    /// and every node-level check would pass with the disabled leaf still
    /// routable underneath. `help git` cannot catch that either: kaish 0.17's
    /// renderer stops at the node (K1 in docs/issues.md), and the examples it
    /// prints are filtered by the config rather than by the schema, so they
    /// would drop the verb whether or not the leaf survived.
    ///
    /// So the leaf-level property is asserted here, from the walk that
    /// already recurses, rather than left to the coincidence that today's
    /// only node has a single child.
    #[test]
    fn schema_leaves_are_exactly_the_enabled_verbs() {
        for disabled in std::iter::once(None).chain(Verb::ALL.iter().copied().map(Some)) {
            let config = match disabled {
                Some(verb) => GitConfig::read_only().without_verb(verb),
                None => GitConfig::read_only(),
            };
            let schema = tool(config).expect("the rest is a valid config").schema();

            let mut found: Vec<String> =
                leaves(&schema).into_iter().map(|(path, _)| path).collect();
            found.sort();
            let mut want: Vec<String> = Verb::ALL
                .iter()
                .filter(|verb| Some(**verb) != disabled)
                .map(|verb| verb.as_str().to_string())
                .collect();
            want.sort();

            assert_eq!(
                found, want,
                "the schema's routable leaves disagree with the enabled verbs \
                 while {disabled:?} was disabled"
            );
        }
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
                .without_verb(Verb::Show)
                .without_verb(Verb::Diff)
                .without_verb(Verb::Branch)
                .without_verb(Verb::Tag)
                .without_verb(Verb::WorktreeList),
        };
        let schema = git.schema();
        assert!(
            schema.subcommands.is_empty(),
            "a subtracted verb must not appear in the schema"
        );
        let err = route(&schema, &[word("info")]).expect_err("nothing to route to");
        assert_eq!(err.exit_code(), 2);
    }


    /// `docs/embedding-git.md` is the guide two embedders read before
    /// registering this tool, and it enumerates the verb set in prose. Prose
    /// cannot iterate `Verb::ALL`, so it went stale the moment `diff` merged:
    /// the guide said "five verbs" in three places and named none of them
    /// `diff`, so an embedder reading it would not have learned the verb
    /// exists. A cross-model review found it; nothing in the build did.
    ///
    /// This is the cheapest thing that would have. It is deliberately dumb —
    /// it asserts each verb's name appears somewhere in the file, not that the
    /// file describes it well — because the failure it exists to catch is a
    /// verb landing with no mention at all.
    #[test]
    fn the_embedding_guide_names_every_verb() {
        let guide = include_str!("../../../docs/embedding-git.md");
        for verb in Verb::ALL {
            let quoted = format!("`{}`", verb.as_str());
            assert!(
                guide.contains(&quoted),
                "docs/embedding-git.md never names {:?} — a verb landed \
                 without reaching the guide two embedders read before \
                 registering this tool",
                verb
            );
        }
        // Negative control: prove the search can return false, so the loop
        // above is not passing because `contains` always succeeds.
        //
        // The first spelling tried here was "`commit`", on the reasoning that
        // a write verb in a read-profile guide would be a real finding. It
        // fired immediately — on `gix-ref`'s `transaction`/`prepare`/`commit`
        // API names in the read-only layer discussion, which are legitimate.
        // A control that reports a defect for correct content is worse than
        // no control, so this one tests the mechanism instead of guessing at
        // content.
        assert!(
            !guide.contains("`a-verb-this-crate-will-never-have`"),
            "the sentinel matched, so `contains` is not discriminating and \
             the loop above proves nothing"
        );
        // And prove the file was actually loaded, not empty.
        assert!(
            guide.len() > 4096,
            "embedding-git.md is {} bytes — too small to be the guide",
            guide.len()
        );
    }

    /// The guide's `Verb` section enumerates the verb set for a reader, and
    /// prose cannot iterate `Verb::ALL`. That enumeration went stale when
    /// `branch`, `tag` and `worktree list` landed — it said six while the same
    /// page said nine forty lines above.
    ///
    /// What makes this different from the drift `the_embedding_guide_names_
    /// every_verb` already catches: that one asks whether a verb is named
    /// *anywhere in the file*, which the three new verbs were, in the `Limits`
    /// table. The guide's own prose claimed it pinned "this list", and it never
    /// did. So the sentence was describing a guarantee no test provided — the
    /// same defect as a doc claiming a property the code lacks, one level up.
    ///
    /// The markers are what make the claim true rather than aspirational: a
    /// region small enough that "every name is in here" is a real assertion.
    #[test]
    fn the_verb_list_in_the_guide_is_the_verb_enum() {
        let guide = include_str!("../../../docs/embedding-git.md");
        let begin = "<!-- verb-list:begin";
        let start = guide.find(begin).expect(
            "docs/embedding-git.md has no `<!-- verb-list:begin` marker — the \
             enumeration this test pins was renamed or deleted, and with it \
             the only thing keeping that list honest",
        );
        let after_marker = guide[start..]
            .find("-->")
            .map(|i| start + i + 3)
            .expect("the verb-list:begin marker is unterminated");
        let end = guide[after_marker..]
            .find("<!-- verb-list:end -->")
            .map(|i| after_marker + i)
            .expect("docs/embedding-git.md has no `<!-- verb-list:end -->` marker");
        let region = &guide[after_marker..end];

        for verb in Verb::ALL {
            let quoted = format!("`{}`", verb.as_str());
            assert!(
                region.contains(&quoted),
                "the guide's verb list does not name {:?}. It reads:\n{}\n\
                 A verb landed without reaching the list an embedder reads to \
                 learn what this tool can do",
                verb,
                region.trim()
            );
        }

        // Negative control, and a real one: `blame` is designed in
        // architecture.md B.8 and deliberately unbuilt, so it is named
        // elsewhere in this guide and in docs/issues.md. Naming it *here*
        // would promise an embedder a verb that does not exist — which is the
        // exact failure `Verb::Blame` was kept out of the enum to avoid.
        // `commit` covers the other direction: a write verb in a read profile.
        for absent in ["blame", "commit"] {
            assert!(
                !region.contains(&format!("`{absent}`")),
                "the guide's verb list names `{absent}`, which is not in \
                 `Verb::ALL` — the list promises a verb that cannot run:\n{}",
                region.trim()
            );
        }

        // And prove the region was actually found and is not empty, so the
        // loop above cannot pass by searching nothing.
        assert!(
            region.trim().len() > 32,
            "the verb-list region is {} bytes — too small to hold the list, \
             so every assertion above passed against nothing",
            region.trim().len()
        );
    }

    /// AGENTS.md, "Published text is published": a `///` on a clap argument is
    /// copied into `ParamSchema.description` and reaches agents through the
    /// tool schema. Behavior goes there; mechanism goes in a `//` comment.
    ///
    /// The rule was broken on all six verbs at once and nobody saw it, because
    /// the offending text was a doc comment on a field marked `hide = true` —
    /// and `hide` does not mean hidden here. `params_from_clap` deliberately
    /// keeps hidden *positionals* (kaish-tool-api 0.16's `clap_schema.rs`
    /// documents why: for most tools they ARE the public surface, `cat
    /// paths…`), dropping only hidden *flags*. So six descriptions reading
    /// "do not read this field" and naming `ToolArgs::to_argv` shipped to
    /// agents as parameter documentation.
    ///
    /// Reads the built schema rather than the source, because AGENTS.md also
    /// says not to infer the published text by grepping.
    #[test]
    fn no_published_description_is_a_note_to_ourselves() {
        let schema = tool(GitConfig::read_only()).expect("config").schema();
        // Internal vocabulary: types, modules and fields an agent cannot
        // resolve, and second-person instructions aimed at this codebase.
        const INTERNAL: &[&str] = &[
            "ToolArgs",
            "to_argv",
            "args.positional",
            "tool.rs",
            "clap",
            "do not read this field",
        ];
        let leaves = leaves(&schema);
        let mut checked = 0usize;
        for (path, leaf) in &leaves {
            for param in &leaf.params {
                let lowered = param.description.to_lowercase();
                for needle in INTERNAL {
                    assert!(
                        !lowered.contains(&needle.to_lowercase()),
                        "'git {path} --{}' publishes '{}' to agents: {:?}",
                        param.name,
                        needle,
                        param.description
                    );
                }
                checked += 1;
            }
        }
        // Negative control: a guard that only ever proves absence passes
        // vacuously over an empty schema. The `operands` sink is the param
        // that carried the defect, so prove it is present and described.
        assert!(checked >= 6, "only {checked} params checked — schema is empty?");
        assert_eq!(
            leaves.len(),
            Verb::ALL.len(),
            "the walk must reach every verb, nested ones included: {:?}",
            leaves.iter().map(|(p, _)| p).collect::<Vec<_>>()
        );
        for (path, leaf) in &leaves {
            let operands = leaf
                .params
                .iter()
                .find(|p| p.name == "operands")
                .unwrap_or_else(|| panic!("'git {path}' publishes no operands param"));
            assert!(
                operands.description.contains("git "),
                "'git {path}' operands must show the spelling an agent types: {:?}",
                operands.description
            );
        }
    }

    /// The schema and the leaf parsers are two hand-maintained lists, and
    /// `help git` is nearly everything an embedded agent learns about this
    /// tool. This fails the moment they disagree: every flag the schema
    /// advertises for a verb is fed to that verb's own clap parser, and a
    /// flag the schema invents fails with clap's own "unexpected argument".
    ///
    /// The same instinct as `kaish-tools-curl`'s `schema_matches_the_parser`,
    /// applied per verb because this tool's schema is a tree.
    #[test]
    fn schema_matches_the_parser() {
        let schema = tool(GitConfig::read_only()).expect("config").schema();
        let leaves = leaves(&schema);
        assert_eq!(
            leaves.len(),
            Verb::ALL.len(),
            "the guard must see every verb this build ships: {:?}",
            leaves.iter().map(|(p, _)| p).collect::<Vec<_>>()
        );

        // The negative control. This guard is a search for one phrase in
        // clap's error text, so it fails open the day clap rewords it — and a
        // gate that can only pass proves nothing. A flag no verb has must
        // produce the phrase the loop below looks for.
        let planted = verbs::diff::DiffArgs::try_parse_from([
            "git diff".to_string(),
            "--no-such-flag".to_string(),
        ])
        .expect_err("a flag no verb has must not parse");
        assert!(
            planted.to_string().contains("unexpected argument"),
            "clap no longer says 'unexpected argument'; this guard would pass \
             over a schema that advertises flags the parser refuses. Its \
             wording now: {planted}"
        );

        for (path, leaf) in &leaves {
            assert!(
                !leaf.params.is_empty(),
                "'{path}' advertises no parameters — the guard would pass \
                 vacuously over it"
            );
            for param in &leaf.params {
                // The hidden `operands` field collects a verb's operands; it
                // is not a flag an agent types, and clap hides it from help
                // for that reason.
                if param.name == "operands" {
                    continue;
                }
                let mut argv = vec![format!("git {path}"), format!("--{}", param.name)];
                if param.param_type != "bool" {
                    // A value every value-taking flag on this surface accepts:
                    // `--limit`/`--context` want a number, the rest a string.
                    argv.push("1".to_string());
                }
                let parsed = match path.as_str() {
                    "info" => verbs::info::InfoArgs::try_parse_from(&argv).map(|_| ()),
                    "status" => verbs::status::StatusArgs::try_parse_from(&argv).map(|_| ()),
                    "log" => verbs::log::LogArgs::try_parse_from(&argv).map(|_| ()),
                    "ls" => verbs::ls::LsArgs::try_parse_from(&argv).map(|_| ()),
                    "show" => verbs::show::ShowArgs::try_parse_from(&argv).map(|_| ()),
                    "diff" => verbs::diff::DiffArgs::try_parse_from(&argv).map(|_| ()),
                    "branch" => verbs::branch::BranchArgs::try_parse_from(&argv).map(|_| ()),
                    "tag" => verbs::tag::TagArgs::try_parse_from(&argv).map(|_| ()),
                    "worktree list" => {
                        verbs::worktree::WorktreeListArgs::try_parse_from(&argv).map(|_| ())
                    }
                    other => panic!("verb '{other}' is in the schema with no parser here"),
                };
                if let Err(e) = parsed {
                    // A value the flag rejects (`--untracked 1`) is the
                    // parser honoring it, not ignoring it; only "this flag
                    // does not exist" is drift.
                    let text = e.to_string();
                    assert!(
                        !text.contains("unexpected argument"),
                        "`help git {path}` advertises --{}, which its parser \
                         does not accept: {text}",
                        param.name
                    );
                }
            }
        }
    }

    /// The converse direction: a flag the parser honors that the schema never
    /// mentions is invisible to every agent, which is the half of the drift
    /// that fails silently. Spot-checked on `diff`, whose flag set is the
    /// newest and therefore the likeliest to be half-wired.
    #[test]
    fn every_diff_flag_reaches_the_schema() {
        let schema = tool(GitConfig::read_only()).expect("config").schema();
        let leaf = schema
            .subcommands
            .iter()
            .find(|s| s.name == "diff")
            .expect("diff is in the schema");
        let named: Vec<&str> = leaf.params.iter().map(|p| p.name.as_str()).collect();
        for flag in [
            "staged",
            "from",
            "to",
            "path",
            "name-only",
            "patch",
            "context",
            "find-renames",
            "no-find-renames",
            "limit",
            "repo",
            // `--json` is deliberately absent: it comes from the flattened
            // `GlobalFlags`, which the kernel merges from the root into every
            // leaf lookup (E.1). It binds at any depth without the leaf's own
            // param list carrying it.
        ] {
            assert!(
                named.contains(&flag),
                "the parser honors --{flag} and the schema never mentions it; \
                 `help git diff` would hide a flag that works. Schema has: \
                 {named:?}"
            );
        }
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
        // The mount root itself. `Path::join` with an empty remainder appends
        // a separator, so this used to answer `/mnt/` — a VFS path an agent
        // would compare unequal to the mount it was handed.
        assert_eq!(
            to_vfs_path(
                Path::new("/srv/repos"),
                Path::new("/srv/repos"),
                Path::new("/mnt")
            ),
            Some("/mnt".to_string())
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
                all.iter().any(|code| verb_of(code) == Some(*verb)),
                "{verb:?} has no EXAMPLES entry, so help git would never name \
                 it even when enabled"
            );
        }

        for verb in Verb::ALL {
            let narrowed = GitConfig::read_only().without_verb(*verb);
            let remaining: Vec<&str> = examples_for(&narrowed).map(|(_, code)| code).collect();
            assert!(
                !remaining.iter().any(|code| verb_of(code) == Some(*verb)),
                "an example for disabled verb {verb:?} survived filtering: {remaining:?}"
            );
            // Negative control: every other verb's examples survive.
            for other in Verb::ALL {
                if other == verb {
                    continue;
                }
                assert!(
                    remaining.iter().any(|code| verb_of(code) == Some(*other)),
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
