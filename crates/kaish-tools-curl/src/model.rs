//! The response model: status, headers, body, url — one shape shared by
//! both backends and both renders.
//!
//! Stub: this exists so `render.rs` and a future `backend` trait have a
//! concrete type to produce and consume, matching docs/curl.md's "Crate
//! shape". Nothing constructs a [`Response`] from a real fetch yet — that
//! is HTTP-surface work, not built yet (see docs/curl.md "Status"). No
//! dual representation is coming later: this is the one model, and
//! `render.rs`'s job is only ever to format it two ways (text and
//! `--json`), the same "no dual representations" house rule
//! `kaish-tools-git` keeps.

use std::collections::BTreeMap;

/// The result of one HTTP request-response cycle, backend-agnostic.
///
/// Headers are a `BTreeMap`, not a multimap: docs/curl.md's 80/20 surface
/// has no need to preserve repeated response headers, and a stable
/// iteration order keeps `-i` output and the `--json` render deterministic
/// across runs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Response {
    /// The final URL the response came from, after any redirects `-L`
    /// followed.
    pub url: String,
    /// The HTTP status code.
    pub status: u16,
    /// Response headers, lower-cased names.
    pub headers: BTreeMap<String, String>,
    /// The response body, exactly as received.
    pub body: Vec<u8>,
}
