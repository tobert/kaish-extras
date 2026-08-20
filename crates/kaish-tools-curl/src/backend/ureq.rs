//! ureq-based native HTTP backend.
//!
//! Blocking calls run inside [`crate::util::block_in_place_compat`] to avoid
//! stalling a current-thread tokio runtime.

use std::time::Duration;

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

    // The whole-request deadline is always set: `--max-time` when the caller
    // gave one, the `CurlConfig` default otherwise. An embedder running curl
    // inside a bounded hook (kaijutsu) relies on this — an omitted flag must
    // not mean "wait forever".
    let mut builder_cfg = ureq::Agent::config_builder()
        .max_redirects(max_redirs)
        .timeout_global(Some(Duration::from_secs_f64(req.max_time)));
    if let Some(connect) = req.connect_timeout {
        builder_cfg = builder_cfg.timeout_connect(Some(Duration::from_secs_f64(connect)));
    }
    let agent = builder_cfg.build().new_agent();

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

    // Headers arrive complete from the parser — User-Agent and Content-Type
    // included, exactly once each. The backend adds none of its own.
    for (name, value) in &req.headers {
        builder = builder.header(name, value);
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
    let mut resp = agent.run(http_req).map_err(|e| map_ureq_into_curl(e, req, max_redirs))?;

    // --fail behavior.
    let status = resp.status();
    if req.fail_on_error && status.as_u16() >= 400 {
        return Err(CurlError::HttpFailure { status: status.as_u16() });
    }

    // Read body bytes.
    let max_bytes = config.limits().max_response_bytes;
    let body = resp.body_mut().read_to_vec().map_err(|e| {
        // A partial read is data loss dressed up as a response. Fail instead
        // of handing the caller a body that is quietly short.
        CurlError::Transport(format!(
            "curl: failed reading the response body from {}: {e}",
            extract_host(&req.url)
        ))
    })?;

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

/// Map a ureq failure onto the curl exit-code taxonomy (docs/curl.md
/// "Exit codes"). `req` and `max_redirs` are here so the error names the host
/// and the cap the caller actually asked for, instead of a placeholder.
fn map_ureq_into_curl(err: ureq::Error, req: &Request, max_redirs: u32) -> CurlError {
    let host = extract_host(&req.url);
    match err {
        ureq::Error::ConnectionFailed => CurlError::CouldNotConnect {
            host,
            reason: "connection failed".into(),
        },
        // Exit 28, not 7: a deadline that fired is not a connection that
        // never opened, and an agent retrying on 7 would retry the wrong
        // thing.
        ureq::Error::Timeout(_) => CurlError::Timeout {
            seconds: req.max_time,
        },
        ureq::Error::Tls(msg) => CurlError::TlsHandshakeFailed {
            host,
            reason: msg.into(),
        },
        ureq::Error::Pem(e) => CurlError::CertificateNotAuthenticated {
            host,
            reason: format!("{e}"),
        },
        ureq::Error::TooManyRedirects => CurlError::TooManyRedirects { limit: max_redirs },
        ureq::Error::Io(e) => CurlError::CouldNotConnect {
            host,
            reason: format!("IO error: {e}"),
        },
        _ => CurlError::Transport(format!("{err}")),
    }
}

fn extract_host(url: &str) -> String {
    url.split_once("://")
        .map(|(_, rest)| rest.split(['/', ':', '?']).next().unwrap_or(rest))
        .unwrap_or(url)
        .to_string()
}
