//! kaish-web — the kaish kernel compiled to `wasm32-unknown-unknown` for the
//! browser, behind a small wasm-bindgen surface.
//!
//! The kernel runs exactly as any embedder gets it: `KernelConfig::isolated()`
//! — in-memory VFS at `/`, external commands disabled, hermetic env. All I/O
//! is in-memory, so kernel futures resolve without external events and a
//! current-thread tokio runtime driven by `block_on` per call is sufficient;
//! the browser tab never waits on anything outside the wasm instance.
//!
//! Seeding: `seed_file` writes through the kernel's own VFS router, so the
//! playground's sample tree is ordinary in-memory files — everything the
//! visitor does to them stays in their tab.

use std::borrow::Cow;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use kaish_kernel::vfs::Filesystem;
use kaish_kernel::{Kernel, KernelConfig};
use wasm_bindgen::prelude::*;

fn js_err(e: impl std::fmt::Display) -> JsValue {
    JsValue::from_str(&e.to_string())
}

/// Largest output field handed to the page per call.
const MAX_FIELD_BYTES: usize = 1_000_000;

/// Clip to `MAX_FIELD_BYTES` at a char boundary, marking the cut.
fn clip(s: &str) -> Cow<'_, str> {
    if s.len() <= MAX_FIELD_BYTES {
        return Cow::Borrowed(s);
    }
    let mut end = MAX_FIELD_BYTES;
    while !s.is_char_boundary(end) {
        end -= 1;
    }
    Cow::Owned(format!("{}\n… [output truncated at 1 MB]\n", &s[..end]))
}

#[wasm_bindgen]
pub struct KaishShell {
    rt: tokio::runtime::Runtime,
    kernel: Arc<Kernel>,
}

#[wasm_bindgen]
impl KaishShell {
    /// Construct an isolated kernel: in-memory VFS at `/`, no external
    /// commands, nothing shared with the page beyond this object.
    #[wasm_bindgen(constructor)]
    pub fn new() -> Result<KaishShell, JsValue> {
        console_error_panic_hook::set_once();
        let rt = tokio::runtime::Builder::new_current_thread()
            .build()
            .map_err(js_err)?;
        let kernel = {
            let _guard = rt.enter();
            let mut config = KernelConfig::isolated();
            // A tab is not a machine: cap the in-memory VFS so a runaway
            // write loop degrades into an ENOSPC-style error, not tab death.
            config.vfs_budget_bytes = Some(256 * 1024 * 1024);
            // Bound per-command output in the kernel (head/tail elision) so a
            // runaway `while true; do echo …` accumulates a window, not the
            // tab's memory; in_memory keeps it off the (nonexistent) disk.
            config.output_limit =
                kaish_kernel::output_limit::OutputLimitConfig::agent().in_memory();
            Kernel::new(config).map_err(js_err)?.into_arc()
        };
        Ok(KaishShell { rt, kernel })
    }

    /// Execute one kaish statement or script. Returns a JSON string:
    /// `{"code": <i64>, "out": <string>, "err": <string|null>}`.
    ///
    /// `out`/`err` are clipped to 1 MB each — a multi-megabyte string crossing
    /// the wasm boundary into `JSON.parse` and the DOM would freeze the tab.
    pub fn execute(&self, input: &str) -> String {
        let outcome = self.rt.block_on(self.kernel.execute(input));
        let json = match outcome {
            Ok(r) => serde_json::json!({
                "code": r.code,
                "out": clip(r.text_out().as_ref()),
                "err": if r.err.is_empty() { None } else { Some(clip(&r.err)) },
            }),
            Err(e) => serde_json::json!({ "code": 1, "out": "", "err": e.to_string() }),
        };
        json.to_string()
    }

    /// Tab completion at a cursor position. Returns a JSON string:
    /// `{"start": <byte offset the word begins at>,
    ///   "candidates": [{"display": <string>, "replacement": <string>}, …]}`.
    ///
    /// Context detection is shared with the native REPL
    /// (`kaish_client::completion`); candidates come from the live kernel —
    /// tool schemas for commands, the scope for variables, and the in-memory
    /// VFS for paths (which the native REPL doesn't have yet: it completes
    /// against the real filesystem).
    pub fn complete(&self, line: &str, pos: usize) -> String {
        use kaish_client::completion::{
            detect_completion_context, word_start, CompletionContext,
        };
        use kaish_kernel::vfs::DirEntryKind;

        let mut pos = pos.min(line.len());
        while !line.is_char_boundary(pos) {
            pos -= 1;
        }

        // (display, replacement) pairs; replacement spans [start..pos].
        let (start, mut pairs): (usize, Vec<(String, String)>) =
            self.rt.block_on(async {
                match detect_completion_context(line, pos) {
                    CompletionContext::Command => {
                        let start = word_start(line, pos);
                        let prefix = &line[start..pos];
                        let pairs = self
                            .kernel
                            .tool_schemas()
                            .into_iter()
                            .filter(|s| s.name.starts_with(prefix))
                            .map(|s| (s.name.clone(), s.name.clone()))
                            .collect();
                        (start, pairs)
                    }
                    CompletionContext::Variable => {
                        let before = &line[..pos];
                        let (start, prefix, braced) =
                            if let Some(b) = before.rfind("${") {
                                (b, &line[b + 2..pos], true)
                            } else if let Some(d) = before.rfind('$') {
                                (d, &line[d + 1..pos], false)
                            } else {
                                return (pos, Vec::new());
                            };
                        let pairs = self
                            .kernel
                            .list_vars()
                            .await
                            .into_iter()
                            .filter(|(name, _)| name.starts_with(prefix))
                            .map(|(name, _)| {
                                let replacement = if braced {
                                    format!("${{{name}}}")
                                } else {
                                    format!("${name}")
                                };
                                (name, replacement)
                            })
                            .collect();
                        (start, pairs)
                    }
                    CompletionContext::Path => {
                        let start = word_start(line, pos);
                        let word = &line[start..pos];
                        // A dash-word in argument position completes the
                        // governing command's flags from its schema, not paths.
                        if word.starts_with('-') {
                            use kaish_client::completion::{current_command, flag_candidates};
                            let pairs = current_command(line, pos)
                                .map(|(cs, ce)| &line[cs..ce])
                                .and_then(|cmd| {
                                    self.kernel
                                        .tool_schemas()
                                        .into_iter()
                                        .find(|s| s.name == cmd)
                                })
                                .map(|schema| {
                                    flag_candidates(&schema.params, word)
                                        .into_iter()
                                        .map(|f| (f.clone(), f))
                                        .collect()
                                })
                                .unwrap_or_default();
                            return (start, pairs);
                        }
                        let (dir_part, base) = match word.rfind('/') {
                            Some(i) => (&word[..=i], &word[i + 1..]),
                            None => ("", word),
                        };
                        let dir_abs = if dir_part.starts_with('/') {
                            PathBuf::from(dir_part)
                        } else {
                            self.kernel.cwd().await.join(dir_part)
                        };
                        let entries = self
                            .kernel
                            .vfs()
                            .list(&dir_abs)
                            .await
                            .unwrap_or_default();
                        let pairs = entries
                            .into_iter()
                            .filter(|e| {
                                !e.name.is_empty()
                                    && e.name.starts_with(base)
                                    // dotfiles only when asked for, like bash
                                    && (base.starts_with('.') || !e.name.starts_with('.'))
                            })
                            .map(|e| {
                                let slash = match e.kind {
                                    DirEntryKind::Directory => "/",
                                    _ => "",
                                };
                                (
                                    format!("{}{slash}", e.name),
                                    format!("{dir_part}{}{slash}", e.name),
                                )
                            })
                            .collect();
                        (start, pairs)
                    }
                }
            });

        pairs.sort();
        pairs.dedup();
        serde_json::json!({
            "start": start,
            "candidates": pairs
                .into_iter()
                .map(|(display, replacement)| {
                    serde_json::json!({ "display": display, "replacement": replacement })
                })
                .collect::<Vec<_>>(),
        })
        .to_string()
    }

    /// Write a file into the in-memory VFS, creating parent directories.
    pub fn seed_file(&self, path: &str, contents: &[u8]) -> Result<(), JsValue> {
        self.rt.block_on(async {
            let vfs = self.kernel.vfs();
            let p = Path::new(path);
            if let Some(parent) = p.parent() {
                let mut cur = PathBuf::new();
                for comp in parent.components() {
                    cur.push(comp);
                    // An ancestor that already exists is the common case;
                    // anything else (e.g. a file squatting on the dir name)
                    // must surface, or the write below fails confusingly.
                    if let Err(e) = vfs.mkdir(&cur).await {
                        if e.kind() != std::io::ErrorKind::AlreadyExists {
                            return Err(js_err(format!("mkdir {}: {e}", cur.display())));
                        }
                    }
                }
            }
            vfs.write(p, contents).await.map_err(js_err)
        })
    }
}
