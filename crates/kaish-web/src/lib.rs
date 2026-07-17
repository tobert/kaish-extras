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

use std::path::{Path, PathBuf};
use std::sync::Arc;

use kaish_kernel::vfs::Filesystem;
use kaish_kernel::{Kernel, KernelConfig};
use wasm_bindgen::prelude::*;

fn js_err(e: impl std::fmt::Display) -> JsValue {
    JsValue::from_str(&e.to_string())
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
            Kernel::new(config).map_err(js_err)?.into_arc()
        };
        Ok(KaishShell { rt, kernel })
    }

    /// Execute one kaish statement or script. Returns a JSON string:
    /// `{"code": <i64>, "out": <string>, "err": <string|null>}`.
    pub fn execute(&self, input: &str) -> String {
        let outcome = self.rt.block_on(self.kernel.execute(input));
        let json = match outcome {
            Ok(r) => serde_json::json!({
                "code": r.code,
                "out": r.text_out().as_ref(),
                "err": if r.err.is_empty() { None } else { Some(r.err.as_str()) },
            }),
            Err(e) => serde_json::json!({ "code": 1, "out": "", "err": e.to_string() }),
        };
        json.to_string()
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
                    // AlreadyExists on an ancestor is the common case, not an error.
                    let _ = vfs.mkdir(&cur).await;
                }
            }
            vfs.write(p, contents).await.map_err(js_err)
        })
    }
}
