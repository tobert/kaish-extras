//! The response model: status, headers, body, url — one shape shared by
//! both backends and both renders.

use std::collections::BTreeMap;

/// The result of one HTTP request-response cycle, backend-agnostic.
///
/// Headers are a `BTreeMap`: a stable iteration order keeps `-i` output
/// and the `--json` render deterministic across runs.
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
