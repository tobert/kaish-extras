//! ureq-based native HTTP backend.
//!
//! Blocking calls run inside [`crate::util::block_in_place_compat`] to avoid
//! stalling a current-thread tokio runtime.

use base64::{Engine as _, engine::general_purpose::STANDARD};

use crate::args::Request;
use crate::config::{CurlConfig, EgressResult, RedirectPolicy};
use crate::error::CurlError;
use crate::model::Response as CurlResponse;

/// Execute the HTTP request via ureq, applying egress checks and config.
pub fn fetch(req: &Request, config: &CurlConfig) -> Result<CurlResponse, CurlError> {
    // Egress policy check on initial URL.
    if config.permit_egress(&req.url) != EgressResult::Allowed {
        return Err(CurlError::CouldNotConnect {
            host: extract_host(&req.url),
            reason: "request denied by embedder egress policy".into(),
        });
    }

    let max_redirs = match (req.follow_redirects, config.follow_redirects()) {
        (Some(n), _) => n,
        (None, RedirectPolicy::Auto) => config.limits().max_redirects,
        (None, _) => 0,
    };

    // Build ureq agent with timeout.
    let cfg = ureq::Agent::config_builder()
        .max_redirects(max_redirs)
        .build();
    let agent = cfg.new_agent();

    // Detect whether user explicitly set User-Agent (dedup fix).
    let has_user_header = req.headers.iter().any(|(k, _)| k.eq_ignore_ascii_case("User-Agent"));

    // Body from -d/--data joins with '&'.
    let mut body_bytes: Vec<u8> = Vec::new();
    if !req.bodies.is_empty() {
        body_bytes = req.bodies.join("&").into_bytes();
    }

    // Build http::Request manually for full control over method, headers, body.
    let mut builder = ureq::http::Request::builder()
        .method(match req.method.as_str() {
            "HEAD" => "HEAD",
            "GET" => "GET",
            "POST" => "POST",
            "PUT" => "PUT",
            "DELETE" => "DELETE",
            "PATCH" => "PATCH",
            "OPTIONS" => "OPTIONS",
            other => other,
        })
        .uri(&req.url);

    // Apply headers from parsed arguments.
    for (name, value) in &req.headers {
        builder = builder.header(name, value);
    }
    // Add default User-Agent only when user didn't set one.
    if !has_user_header {
        builder = builder.header("User-Agent", "kaish-curl");
    }

    // Basic auth.
    if let Some(ref u) = req.user {
        let p = req.password.clone().unwrap_or_default();
        let creds = format!("{u}:{p}");
        let encoded = STANDARD.encode(&creds);
        builder = builder.header("Authorization", format!("Basic {encoded}"));
    }

    let http_req = builder.body(body_bytes).map_err(|e| {
        CurlError::MalformedUrl {
            url: req.url.clone(),
            reason: format!("could not build request: {e}"),
        }
    })?;

    // Run the request.
    let mut resp = agent.run(http_req).map_err(map_ureq_into_curl)?;

    // --fail behavior.
    let status = resp.status();
    if req.fail_on_error && status.as_u16() >= 400 {
        return Err(CurlError::HttpFailure { status: status.as_u16() });
    }

    // Read body bytes.
    let max_bytes = config.limits().max_response_bytes;
    let body = read_body(resp.body_mut());

    if body.len() as u64 > max_bytes {
        let truncated = truncate_utf8_lossy(&body[..max_bytes as usize]);
        return Ok(CurlResponse {
            url: req.url.clone(),
            status: status.as_u16(),
            headers: resp_headers(resp.headers()),
            body: truncated.into_bytes(),
        });
    }

    let body = truncate_utf8_lossy(&body).into_bytes();

    Ok(CurlResponse {
        url: req.url.clone(),
        status: status.as_u16(),
        headers: resp_headers(resp.headers()),
        body,
    })
}

fn read_body(body: &mut ureq::Body) -> Vec<u8> {
    match body.read_to_vec() {
        Ok(v) => v,
        Err(_) => Vec::new(),
    }
}

fn resp_headers(headers: &ureq::http::HeaderMap) -> std::collections::BTreeMap<String, String> {
    use std::collections::BTreeMap;
    let mut map = BTreeMap::new();
    for (name, value) in headers.iter() {
        map.insert(name.to_string(), value.to_str().unwrap_or("").to_string());
    }
    map
}

fn truncate_utf8_lossy(bytes: &[u8]) -> String {
    String::from_utf8_lossy(bytes).to_string()
}

fn map_ureq_into_curl(err: ureq::Error) -> CurlError {
    match err {
        ureq::Error::ConnectionFailed => {
            CurlError::CouldNotConnect {
                host: "[unknown]".into(),
                reason: "connection failed".into(),
            }
        }
        ureq::Error::Timeout(_) => {
            CurlError::CouldNotConnect {
                host: "[timeout]".into(),
                reason: "request timed out".into(),
            }
        }
        ureq::Error::Tls(msg) => {
            CurlError::CertificateNotAuthenticated {
                host: "[tls]".into(),
                reason: msg.into(),
            }
        }
        ureq::Error::Pem(e) => {
            CurlError::CertificateNotAuthenticated {
                host: "[pem]".into(),
                reason: format!("{e}"),
            }
        }
        ureq::Error::TooManyRedirects => {
            CurlError::TooManyRedirects { limit: 0 }
        }
        ureq::Error::Io(e) => {
            CurlError::CouldNotConnect {
                host: "[io]".into(),
                reason: format!("IO error: {e}"),
            }
        }
        _ => {
            CurlError::Transport(format!("{err}"))
        }
    }
}

fn extract_host(url: &str) -> String {
    url.split_once("://")
        .map(|(_, rest)| rest.split(['/', ':', '?']).next().unwrap_or(rest))
        .unwrap_or(url)
        .to_string()
}
