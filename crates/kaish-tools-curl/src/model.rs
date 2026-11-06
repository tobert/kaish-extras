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
    /// The standard reason phrase for `status` (`Not Found` for 404), empty
    /// for a code with none. The backend cannot see the phrase the server
    /// sent: ureq drops it while parsing the status line.
    pub reason: String,
    /// Response headers, lower-cased names, **every** value the server sent
    /// under each name. A `BTreeMap<String, String>` kept only the last of
    /// three `Set-Cookie`s and silently dropped the rest (CU37); `-i` is
    /// documented to print what the server actually sent.
    pub headers: BTreeMap<String, Vec<String>>,
    /// How many redirects were followed to reach this response.
    pub redirects: u32,
    /// The response body, exactly as received.
    pub body: Vec<u8>,
}

impl Response {
    /// What `-w` reports when the request failed before any response:
    /// status 0 (printed `000`), nothing downloaded, the URL as asked for.
    pub fn unanswered(url: String) -> Self {
        Self {
            url,
            status: 0,
            reason: String::new(),
            headers: BTreeMap::new(),
            redirects: 0,
            body: Vec::new(),
        }
    }

    /// Whether the server labelled this body as JSON.
    ///
    /// The `Content-Type` is the only authority worth trusting here: sniffing
    /// the bytes would turn a text/plain `42` into a number.
    pub fn is_json(&self) -> bool {
        self.headers
            .iter()
            .filter(|(name, _)| name.eq_ignore_ascii_case("content-type"))
            .flat_map(|(_, values)| values.iter())
            .any(|v| {
                let v = v.to_ascii_lowercase();
                v.starts_with("application/json") || v.contains("+json")
            })
    }
}
