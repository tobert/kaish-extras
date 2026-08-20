//! The failure surface: docs/curl.md's exit-code table, asserted as the
//! actual `ExecResult::code` `crate::error::CurlError::exit_code` produces,
//! plus a check that every message names the thing that went wrong (never a
//! bare number an agent has to guess at).
//!
//! A malformed URL is exit **7**, not curl's 3: kaish reserves 3/124/130 for
//! kernel-internal conditions, so the mapping moved (CU11) and the error
//! names the real cause instead.

#[path = "support.rs"]
mod support;

use support::{argv, curl, loopback_config, Response, Server};

use kaish_tools_curl::{AllowByList, CurlConfig};

#[tokio::test]
async fn missing_url_fails_with_exit_7() {
    let result = curl(loopback_config(), argv(&[])).await;
    assert_eq!(result.code, 7, "{}", result.err);
    assert!(result.err.contains("URL is required"), "{}", result.err);
}

#[tokio::test]
async fn non_http_scheme_fails_with_exit_7() {
    let result = curl(loopback_config(), argv(&["ftp://example.com/file"])).await;
    assert_eq!(result.code, 7, "{}", result.err);
    assert!(
        result.err.contains("http") && result.err.contains("https"),
        "message should name the supported schemes: {}",
        result.err
    );
}

#[tokio::test]
async fn dash_f_on_a_404_fails_with_exit_22() {
    let server = Server::builder()
        .route("/missing", Response::text(404, "not found"))
        .start_tcp();
    let url = server.url("/missing");

    let result = curl(loopback_config(), argv(&["-f", &url])).await;
    assert_eq!(result.code, 22, "{}", result.err);
    assert!(result.err.contains("404"), "message should name the status: {}", result.err);
}

#[tokio::test]
async fn connection_refused_fails_with_exit_7() {
    // Bind an ephemeral port, then drop the listener so nothing accepts on
    // it — the most reliable way to get a real ECONNREFUSED from loopback
    // without depending on a port being closed by convention.
    let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind an ephemeral port");
    let port = listener.local_addr().expect("local_addr").port();
    drop(listener);
    let url = format!("http://127.0.0.1:{port}/");

    let result = curl(loopback_config(), argv(&[&url])).await;
    assert_eq!(result.code, 7, "{}", result.err);
}

#[tokio::test]
async fn max_time_shorter_than_the_route_fails_with_exit_28() {
    let server = Server::builder()
        .route_fn("/slow", |_req| {
            std::thread::sleep(std::time::Duration::from_millis(500));
            Response::text(200, "eventually")
        })
        .start_tcp();
    let url = server.url("/slow");

    let result = curl(loopback_config(), argv(&["--max-time", "0.05", &url])).await;
    assert_eq!(result.code, 28, "{}", result.err);
}

#[tokio::test]
async fn egress_denial_never_reaches_the_server_and_names_the_policy() {
    let server = Server::builder()
        .route("/x", Response::text(200, "should never be reached"))
        .start_tcp();
    let url = server.url("/x");

    // Loopback specifically NOT enabled, and the allowlist names a host that
    // isn't this server — the config a real embedder would ship to keep an
    // agent off its own metadata endpoint. `CurlConfig::default()` will NOT
    // do this job (see the note below); this is why every other test in this
    // suite threads `loopback_config()` explicitly.
    let config = CurlConfig::default()
        .with_allow_egress(AllowByList::new().with_allowed_hosts(["allowed.example"]));

    let result = curl(config, argv(&[&url])).await;
    assert_eq!(result.code, 7, "{}", result.err);
    assert!(
        result.err.contains("denied") && result.err.contains("policy"),
        "message should say the embedder policy denied it: {}",
        result.err
    );
    assert!(
        server.requests().is_empty(),
        "a denied request must never reach the server"
    );
}

#[tokio::test]
async fn refused_flags_name_themselves_and_never_run() {
    // These are refused because they are genuinely out of this build's
    // scope (progress meters, compression negotiation, verbose tracing), not
    // because the backend fails to implement something it claims to offer.
    for flag in ["-O", "-v", "--compressed"] {
        let result = curl(loopback_config(), argv(&[flag, "http://127.0.0.1:1/"])).await;
        assert_ne!(result.code, 0, "{flag} should be refused, not silently accepted");
        assert!(
            result.err.contains(flag),
            "refusal for {flag} should name it: {}",
            result.err
        );
    }
}

#[tokio::test]
async fn dash_k_and_unix_socket_are_refused_because_the_backend_cannot_honor_them() {
    // Unlike the flags above, `-k` and `--unix-socket` are refused for a
    // sharper reason: the backend has no code path that honors either one
    // (no TLS-verification bypass, no AF_UNIX transport — see CU7/CU22 in
    // docs/issues.md). Accepting either would silently do the opposite of
    // what the caller asked — verify TLS anyway, or dial TCP instead of the
    // socket. Refusing is deliberate, and this is the test that would catch
    // a future change that "fixes" the refusal by quietly wiring the flag to
    // nothing.
    for flag in ["-k", "--unix-socket"] {
        let result = curl(loopback_config(), argv(&[flag, "http://127.0.0.1:1/"])).await;
        assert_ne!(result.code, 0, "{flag} should be refused, not silently accepted");
        assert!(
            result.err.contains(flag),
            "refusal for {flag} should name it: {}",
            result.err
        );
    }
}

#[tokio::test]
async fn at_file_body_syntax_is_refused_and_names_at_path() {
    let result = curl(
        loopback_config(),
        argv(&["-d", "@/etc/passwd", "http://127.0.0.1:1/"]),
    )
    .await;
    assert_ne!(result.code, 0);
    assert!(result.err.contains("@path"), "{}", result.err);
}

#[tokio::test]
async fn head_with_data_is_refused() {
    let result = curl(
        loopback_config(),
        argv(&["-I", "-d", "x=1", "http://127.0.0.1:1/"]),
    )
    .await;
    assert_ne!(result.code, 0);
    assert!(result.err.contains("--head"), "{}", result.err);
}

#[tokio::test]
async fn max_time_with_a_non_number_is_refused_and_names_the_flag() {
    let result = curl(
        loopback_config(),
        argv(&["--max-time", "notanumber", "http://127.0.0.1:1/"]),
    )
    .await;
    assert_ne!(result.code, 0);
    assert!(result.err.contains("--max-time"), "{}", result.err);
}

#[tokio::test]
async fn max_redirs_with_a_non_number_is_refused_and_names_the_flag() {
    let result = curl(
        loopback_config(),
        argv(&["--max-redirs", "notanumber", "http://127.0.0.1:1/"]),
    )
    .await;
    assert_ne!(result.code, 0);
    assert!(result.err.contains("--max-redirs"), "{}", result.err);
}

#[tokio::test]
async fn plain_get_against_a_4xx_returns_the_body_with_exit_0() {
    // docs/curl.md's exit-code table, row 1: "0 | Success (any HTTP status
    // unless --fail)." Without `-f`, a 404 is not a curl failure — the body
    // is the point (an API's error payload, an HTML 404 page) and belongs
    // on stdout with exit 0, exactly like real curl.
    //
    // This is the test that caught ureq's `http_status_as_error` default
    // (`true`, ureq-3.4.0/src/config.rs:867): `agent.run()` was erroring on
    // any 4xx/5xx before `fetch`'s own `--fail` check could run, so a plain
    // 404 came back exit 1 with an empty body and the check was dead code.
    // `backend/ureq.rs` turns that default off; if someone turns it back on,
    // this fails again.
    let server = Server::builder()
        .route("/missing", Response::text(404, "not found body"))
        .start_tcp();
    let url = server.url("/missing");

    let result = curl(loopback_config(), argv(&[&url])).await;
    assert_eq!(result.code, 0, "a bare 404 without --fail is not a curl failure: {}", result.err);
    assert_eq!(result.text_out(), "not found body");
}
