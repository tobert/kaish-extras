//! An 80/20 `curl`-shaped HTTP tool for kaish — one URL, GET or POST,
//! headers, a body, `-i`/`-I`/`-o`, `-L`, `--unix-socket` — the slice
//! of `curl(1)` an agent actually types. See
//! [`docs/curl.md`](https://github.com/tobert/kaish-extras/blob/main/docs/curl.md)
//! for the full surface, the exit-code table, the literate-error catalog,
//! and the native-vs-wasm backend story.
//!
//! # Posture
//!
//! Depends on `kaish-tool-api` and `kaish-types` only, never
//! `kaish-kernel` — the same honest-embedder property `kaish-tools-git`
//! proves for its own crate. curl is meant to be the first cross-target
//! tool in this workspace (native ureq + wasm xhr), so there is no
//! `compile_error!` gating the module tree the way git's does — wasm
//! builds are gated by a `compile_error!` in `backend/mod.rs` until the
//! XHR path is implemented.

mod args;
mod backend;
pub mod config;
pub mod error;
pub mod model;
pub mod render;
mod tool;
mod util;

pub use config::{AllowAll, AllowByList, AllowEgress, CurlConfig, EgressResult, Limits, RedirectPolicy};
pub use error::CurlError;
pub use model::Response;
pub use tool::{tool, CurlTool};
