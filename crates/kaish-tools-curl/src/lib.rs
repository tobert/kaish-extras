//! An 80/20 `curl`-shaped HTTP tool for kaish — one URL, GET or POST,
//! headers, a body, `-i`/`-I`/`-o`/`-O`, `-L`, `--unix-socket` — the slice
//! of `curl(1)` an agent actually types. See
//! [`docs/curl.md`](https://github.com/tobert/kaish-extras/blob/main/docs/curl.md)
//! for the full surface, the exit-code table, the literate-error catalog,
//! and the native-vs-wasm backend story.
//!
//! # Status
//!
//! This is the crate skeleton only. [`CurlConfig`], [`CurlError`], and
//! [`Response`] exist so the pieces around them (`args`, `render`,
//! `backend`) have concrete types to build against, but argument parsing,
//! the ureq backend, and the renders are not implemented yet — they wait on
//! the cross-model review docs/curl.md's "Status" section calls for. There
//! is no `Tool` impl in this crate yet, so nothing here is registrable on a
//! kaish kernel.
//!
//! # Posture
//!
//! Depends on `kaish-tool-api` and `kaish-types` only, never
//! `kaish-kernel` — the same honest-embedder property `kaish-tools-git`
//! proves for its own crate. Unlike `kaish-tools-git`, curl is meant to be
//! the first tool in this workspace that also builds for
//! `wasm32-unknown-unknown`, so there is no `compile_error!` gating this
//! module tree the way git's does.

mod args;
mod backend;
mod config;
mod error;
mod model;
mod render;
mod util;

pub use config::{AllowAll, AllowByList, AllowEgress, CurlConfig, EgressResult, Limits, RedirectPolicy};
pub use error::CurlError;
pub use model::Response;
