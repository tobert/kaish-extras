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

use url::Url;

/// Execute the HTTP request, following redirects **ourselves**.
///
/// ureq can follow redirects internally, and that is what this used to let it
/// do — which meant the egress allowlist was consulted once, on the URL the
/// caller typed, and every hop after that went wherever the previous response
/// pointed. An allowed host that answers `302 Location:
/// http://169.254.169.254/` was a hole straight through the embedder's
/// policy (docs/issues.md CU8). So the agent is built with
/// `max_redirects(0)` and the hop loop lives here, where the policy can see
/// each one before it is dialed.
pub fn fetch(req: &Request, config: &CurlConfig) -> Result<CurlResponse, CurlError> {
    let limits = config.limits();

    // A flag may lower a ceiling the embedder set, never raise it — which is
    // what `Limits` has always claimed and what `unwrap_or` did not do
    // (CU34). An agent typing `--max-time 3600` inside a bounded hook was
    // overriding the embedder outright.
    let max_time = req.max_time.min(limits.max_time);
    // "Not following" and "following, cap 0" are different answers, not the
    // same number: without `-L`, curl hands the 302 back as an ordinary
    // response, while `-L --max-redirs 0` is exit 47 on the first hop.
    let following = req.follow_redirects || config.follow_redirects() == RedirectPolicy::Auto;
    let max_redirs = req
        .max_redirects
        .unwrap_or(limits.max_redirects)
        .min(limits.max_redirects);

    let mut builder_cfg = ureq::Agent::config_builder()
        // ureq's `http_status_as_error` defaults to **true**
        // (ureq-3.4.0/src/config.rs:867), which turns every 4xx/5xx into
        // `Err(Error::StatusCode)` before `fetch` can look at it. That is the
        // opposite of curl: a plain `curl <url>` against a 404 prints the
        // error page and exits 0, and only `-f` turns a status into a
        // failure. Turning it off leaves that decision where it belongs —
        // the `fail_on_error` check below.
        .http_status_as_error(false)
        // We drive redirects; ureq hands the 3xx back rather than following
        // or erroring on it.
        .max_redirects(0)
        .max_redirects_will_error(false)
        // The whole-request deadline is always set, clamped to the embedder's
        // ceiling above. An omitted `--max-time` must not mean "wait forever".
        .timeout_global(Some(Duration::from_secs_f64(max_time)));
    if let Some(connect) = req.connect_timeout {
        builder_cfg =
            builder_cfg.timeout_connect(Some(Duration::from_secs_f64(connect.min(max_time))));
    }
    let agent = builder_cfg.build().new_agent();

    // Credentials from `-u`, or from the URL's own userinfo when the caller
    // spelled them there instead (curl accepts both; `-u` wins).
    let mut current = parse_url(&req.url)?;
    let mut credentials = match (&req.user, &req.password) {
        (Some(u), p) => Some((u.clone(), p.clone().unwrap_or_default())),
        (None, _) => url_credentials(&current),
    };
    // The URL we dial carries no userinfo, so the string the policy checked is
    // the string ureq resolves — no parser sitting between the two.
    strip_userinfo(&mut current);

    let body_bytes: Vec<u8> = if req.bodies.is_empty() {
        Vec::new()
    } else {
        req.bodies.join("&").into_bytes()
    };

    let started = std::time::Instant::now();
    let mut hops: u32 = 0;
    loop {
        if config.permit_egress(current.as_str()) != EgressResult::Allowed {
            return Err(CurlError::CouldNotConnect {
                host: host_of(&current),
                reason: "request denied by embedder egress policy".into(),
            });
        }

        let mut builder = ureq::http::Request::builder()
            .method(req.method.as_str())
            .uri(current.as_str());

        // Headers arrive complete from the parser — User-Agent and
        // Content-Type included, exactly once each.
        for (name, value) in &req.headers {
            builder = builder.header(name, value);
        }
        if let Some((user, pass)) = &credentials {
            let encoded = STANDARD.encode(format!("{user}:{pass}"));
            builder = builder.header("Authorization", format!("Basic {encoded}"));
        }

        let http_req = builder
            .body(body_bytes.clone())
            .map_err(|e| CurlError::MalformedUrl {
                url: current.to_string(),
                reason: format!("could not build request: {e}"),
            })?;

        let mut resp = agent
            .run(http_req)
            .map_err(|e| map_ureq_into_curl(e, &current, started.elapsed(), max_redirs))?;
        let status = resp.status().as_u16();

        if let Some(location) = redirect_target(status, resp.headers()).filter(|_| following) {
            if hops >= max_redirs {
                return Err(CurlError::TooManyRedirects { limit: max_redirs });
            }
            let next = current
                .join(&location)
                .map_err(|e| CurlError::MalformedUrl {
                    url: location.clone(),
                    reason: format!("redirect target is not a usable URL: {e}"),
                })?;
            // Credentials do not cross a host boundary. docs/curl.md and CU10
            // both promised this; nothing implemented it, so `-u` plus a
            // redirect was an exfiltration path.
            if next.host_str() != current.host_str() {
                credentials = None;
            }
            current = next;
            strip_userinfo(&mut current);
            hops += 1;
            continue;
        }

        if req.fail_on_error && status >= 400 {
            return Err(CurlError::HttpFailure { status });
        }

        let body = read_body_within(&mut resp, limits.max_response_bytes, &current)?;
        return Ok(CurlResponse {
            // The URL the response actually came from, after any hops — which
            // is what `model.rs` documents and what the caller needs to
            // resolve anything relative in the body.
            url: current.to_string(),
            status,
            headers: resp_headers(resp.headers()),
            body,
        });
    }
}

/// The `Location` of a redirect this build follows, if this is one.
///
/// 304 and 305 are excluded deliberately: `Not Modified` carries no new
/// location, and `Use Proxy` is a proxy instruction this build does not honor.
fn redirect_target(status: u16, headers: &ureq::http::HeaderMap) -> Option<String> {
    if !matches!(status, 301 | 302 | 303 | 307 | 308) {
        return None;
    }
    headers
        .get("location")?
        .to_str()
        .ok()
        .map(|s| s.trim().to_string())
}

/// Read the body, refusing to exceed the embedder's cap.
///
/// Bounded by `.take()` rather than read-then-measure: reading the whole
/// response first and trimming afterwards let a hostile endpoint make the
/// embedder buffer any amount it liked, and then returned the trimmed body as
/// a **success** — so `-o` wrote a silently corrupt file at exit 0 (CU35).
/// Over the cap is now a failure, not a quiet truncation.
fn read_body_within(
    resp: &mut ureq::http::Response<ureq::Body>,
    max_bytes: u64,
    url: &Url,
) -> Result<Vec<u8>, CurlError> {
    use std::io::Read;

    let mut buf = Vec::new();
    let read = resp
        .body_mut()
        .as_reader()
        .take(max_bytes.saturating_add(1))
        .read_to_end(&mut buf)
        .map_err(|e| {
            // A partial read is data loss dressed up as a response.
            CurlError::Transport(format!(
                "failed reading the response body from {}: {e}",
                host_of(url)
            ))
        })?;

    if read as u64 > max_bytes {
        return Err(CurlError::Transport(format!(
            "response body from {} exceeds the {max_bytes}-byte limit this embedder allows. \
             Request a smaller range, or ask the embedder to raise `Limits::max_response_bytes`.",
            host_of(url)
        )));
    }
    Ok(buf)
}

fn parse_url(raw: &str) -> Result<Url, CurlError> {
    Url::parse(raw).map_err(|e| CurlError::MalformedUrl {
        url: raw.to_string(),
        reason: format!("{e}"),
    })
}

/// Credentials spelled into the URL itself (`http://user:pass@host/`).
fn url_credentials(url: &Url) -> Option<(String, String)> {
    if url.username().is_empty() && url.password().is_none() {
        return None;
    }
    Some((
        url.username().to_string(),
        url.password().unwrap_or_default().to_string(),
    ))
}

/// Remove userinfo from a URL before it is dialed.
///
/// `Url::set_username`/`set_password` only fail for URLs that cannot have a
/// host (`mailto:`, `data:`); this build refuses anything but http/https long
/// before here, so there is nothing to recover from — the results are
/// deliberately dropped rather than pretended to be handled.
fn strip_userinfo(url: &mut Url) {
    let _ = url.set_username("");
    let _ = url.set_password(None);
}

fn host_of(url: &Url) -> String {
    url.host_str().unwrap_or("[unknown]").to_string()
}

fn resp_headers(headers: &ureq::http::HeaderMap) -> std::collections::BTreeMap<String, Vec<String>> {
    use std::collections::BTreeMap;
    let mut map: BTreeMap<String, Vec<String>> = BTreeMap::new();
    for (name, value) in headers.iter() {
        // A header value that is not UTF-8 became an empty string, which
        // reads as "the server sent nothing" rather than "we could not render
        // this". Say which.
        let rendered = match value.to_str() {
            Ok(v) => v.to_string(),
            Err(_) => format!("<{} bytes, not valid UTF-8>", value.as_bytes().len()),
        };
        // Every value under the name, not the last one: three `Set-Cookie`s
        // used to collapse into one (CU37).
        map.entry(name.to_string()).or_default().push(rendered);
    }
    map
}

/// Map a ureq failure onto the curl exit-code taxonomy (docs/curl.md
/// "Exit codes"). The current URL and the caps are here so the error names
/// the real host and the real limits, instead of a placeholder.
fn map_ureq_into_curl(
    err: ureq::Error,
    url: &Url,
    elapsed: std::time::Duration,
    max_redirs: u32,
) -> CurlError {
    let host = host_of(url);
    match err {
        // Exit 6. ureq names this case exactly and nothing matched it, so a
        // DNS failure arrived as "could not connect" (7) or as a generic
        // transport error (1) — and an agent that branches on 6 to try a
        // different name never saw one (CU39).
        ureq::Error::HostNotFound => CurlError::HostNotFound { host },
        ureq::Error::ConnectionFailed => CurlError::CouldNotConnect {
            host,
            reason: "connection failed".into(),
        },
        // Exit 28, not 7: a deadline that fired is not a connection that
        // never opened, and an agent retrying on 7 would retry the wrong
        // thing.
        // The time actually spent, not the budget that was configured — an
        // agent reading "timed out after 30s" when it waited 0.2s learns the
        // wrong thing about the endpoint (CU42).
        ureq::Error::Timeout(_) => CurlError::Timeout { seconds: elapsed.as_secs_f64() },
        // ureq collapses every TLS failure into one &'static str, so telling
        // "the certificate did not verify" (60) from "the handshake broke"
        // (35) means reading that string. The split is a heuristic on ureq's
        // wording, stated here rather than hidden: if it stops matching, both
        // land on 35, which is the safer of the two to over-report.
        ureq::Error::Tls(msg) => {
            let lowered = msg.to_ascii_lowercase();
            if lowered.contains("cert") || lowered.contains("verif") {
                CurlError::CertificateNotAuthenticated { host, reason: msg.into() }
            } else {
                CurlError::TlsHandshakeFailed { host, reason: msg.into() }
            }
        }
        ureq::Error::Pem(e) => CurlError::CertificateNotAuthenticated {
            host,
            reason: format!("{e}"),
        },
        ureq::Error::TooManyRedirects => CurlError::TooManyRedirects { limit: max_redirs },
        // ureq will not replay a request body across a 307/308. Say so,
        // rather than letting it fall through as a bare "redirect failed".
        ureq::Error::RedirectFailed => CurlError::Transport(format!(
            "{host} answered with a redirect that would require sending the request body again, which this build will not do. Reissue the request against the redirect target."
        )),
        ureq::Error::BadUri(reason) => CurlError::MalformedUrl {
            url: url.to_string(),
            reason,
        },
        ureq::Error::Io(e) => CurlError::CouldNotConnect {
            host,
            reason: format!("IO error: {e}"),
        },
        _ => CurlError::Transport(format!("{err}")),
    }
}
