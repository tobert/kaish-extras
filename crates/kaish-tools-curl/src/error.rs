//! `CurlError` and the exit-code taxonomy from docs/curl.md's "Exit codes"
//! table.
//!
//! Stub, like the rest of this crate: the HTTP surface (argument parsing,
//! the ureq backend, and the URL/DNS/TLS failures that would actually
//! construct these variants) is not built yet — it waits on the review
//! docs/curl.md's "Status" section calls for. The variants and the exit-code
//! mapping below are the settled part: docs/curl.md's exit-code table names
//! every one of these codes already, so wiring them up is future work, not
//! future design.

/// Everything this crate can fail with, carrying its own curl-shaped exit
/// code (docs/curl.md "Exit codes").
///
/// Deliberately not `#[non_exhaustive]`: this enum is matched exhaustively
/// by [`CurlError::exit_code`], and a new variant that forgets to declare
/// its code should be a compile error here rather than a silent default
/// somewhere else — the same reasoning `kaish_tools_git::GitError` states
/// for itself.
#[derive(Debug, thiserror::Error)]
pub enum CurlError {
    /// The URL is malformed: an unsupported scheme, or syntax the parser
    /// rejects outright. Exit 7 ("could not connect") — kaish reserves
    /// 3/124/130 for kernel-internal conditions. Malformed URLs get exit 7
    /// with a literate error naming the actual cause.
    #[error("curl: '{url}' is not a valid http or https URL: {reason}")]
    MalformedUrl {
        /// The URL as the caller spelled it.
        url: String,
        /// What was wrong with it.
        reason: String,
    },

    /// DNS resolution failed. Exit 6.
    #[error("curl: could not resolve host: {host}")]
    HostNotFound {
        /// The hostname that did not resolve.
        host: String,
    },

    /// The connection could not be established — refused, unreachable, or
    /// otherwise never opened. Exit 7.
    #[error("curl: failed to connect to {host}: {reason}")]
    CouldNotConnect {
        /// The host a connection was attempted to.
        host: String,
        /// What the transport reported.
        reason: String,
    },

    /// `--fail` was given and the response status was >= 400. Exit 22.
    #[error("curl: the requested URL returned error: {status}")]
    HttpFailure {
        /// The HTTP status code that triggered the failure.
        status: u16,
    },

    /// `--max-time` or `--connect-timeout` elapsed before the request
    /// finished. Exit 28. Native only (docs/curl.md "Wasm limits").
    #[error("curl: operation timed out after {seconds}s")]
    Timeout {
        /// How long the request ran before the deadline fired.
        seconds: f64,
    },

    /// The TLS handshake failed. Exit 35.
    #[error("curl: TLS handshake with {host} failed: {reason}")]
    TlsHandshakeFailed {
        /// The host the handshake was attempted against.
        host: String,
        /// What the TLS layer reported.
        reason: String,
    },

    /// More redirects were followed than `--max-redirs` allows. Exit 47.
    #[error("curl: too many redirects (over the {limit}-redirect cap)")]
    TooManyRedirects {
        /// The cap that was exceeded.
        limit: u32,
    },

    /// The peer's certificate could not be authenticated. Exit 60.
    #[error("curl: certificate for {host} could not be authenticated: {reason}")]
    CertificateNotAuthenticated {
        /// The host whose certificate failed verification.
        host: String,
        /// What the verifier reported.
        reason: String,
    },

    /// Any other transport failure this table does not name. Exit 1.
    #[error("curl: {0}")]
    Transport(String),
}

impl CurlError {
    /// The process exit code for this failure (docs/curl.md "Exit codes").
    pub fn exit_code(&self) -> i64 {
        match self {
            CurlError::MalformedUrl { .. } => 7,
            CurlError::HostNotFound { .. } => 6,
            CurlError::CouldNotConnect { .. } => 7,
            CurlError::HttpFailure { .. } => 22,
            CurlError::Timeout { .. } => 28,
            CurlError::TlsHandshakeFailed { .. } => 35,
            CurlError::TooManyRedirects { .. } => 47,
            CurlError::CertificateNotAuthenticated { .. } => 60,
            CurlError::Transport(_) => 1,
        }
    }
}
