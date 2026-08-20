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

use std::sync::{Arc, OnceLock};

use support::{argv, curl, loopback_config, Response, Server, TcpGuard};

use kaish_tools_curl::{AllowByList, CurlConfig, Limits};

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
        result.err.contains("denied") && result.err.contains("egress allowlist"),
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

// ── Containment: the 2026-08-20 review findings, as tests ────────────────
//
// None of these existed, which is how each bug shipped. They use `localhost`
// and `127.0.0.1` as two *different hosts* that both reach the harness: the
// egress policy and the credential rule both key on the host, and loopback is
// the only place a test can stand up a second one.

/// One listener, reachable under two host names. `127.0.0.1` and `localhost`
/// are different hosts to the egress policy and to the credential rule, and
/// both land on the same server — which is the only way a loopback-only test
/// can exercise a cross-host redirect at all. The target is filled in after
/// `start_tcp` assigns a port, and read when the request arrives.
fn redirect_to_other_name_for_self(from: &str, to_path: &'static str) -> (TcpGuard, String) {
    let target: Arc<OnceLock<String>> = Arc::new(OnceLock::new());
    let for_route = Arc::clone(&target);
    let server = Server::builder()
        .route_fn(from, move |_req| {
            Response::redirect(302, for_route.get().expect("port set before first request"))
        })
        .route(to_path, Response::text(200, "arrived"))
        .start_tcp();
    let port = server.url("/x").rsplit(':').next().unwrap().trim_end_matches("/x").to_string();
    let elsewhere = format!("http://localhost:{port}{to_path}");
    target.set(elsewhere.clone()).expect("set once");
    (server, elsewhere)
}

#[tokio::test]
async fn a_redirect_to_a_denied_host_is_not_followed() {
    // CU8: the allowlist used to be consulted once, on the URL the caller
    // typed, and ureq followed every hop after that unchecked. An allowed
    // host answering `302 Location: <anywhere>` was a hole straight through
    // the embedder's policy.
    let (server, _elsewhere) = redirect_to_other_name_for_self("/start", "/secret");

    // Only the literal `127.0.0.1` is permitted — `localhost` is not, even
    // though it reaches the very same listener.
    let config = CurlConfig::default()
        .with_allow_egress(AllowByList::new().with_allowed_hosts(["127.0.0.1"]));
    let result = curl(config, argv(&["-L", &server.url("/start")])).await;

    assert_eq!(result.code, 7, "a denied hop must fail, not be followed: {}", result.err);
    assert!(
        result.err.contains("egress allowlist"),
        "the error should name the policy that refused it: {}",
        result.err
    );
    let paths: Vec<String> =
        server.requests().iter().map(|r| r.path_no_query().to_string()).collect();
    assert_eq!(paths, vec!["/start"], "the redirect target must never be requested");
}

#[tokio::test]
async fn credentials_do_not_cross_a_host_boundary_on_redirect() {
    // CU27: docs/curl.md and CU10 both promised this stripping. Nothing
    // implemented it, so `-u` plus a redirect leaked the credential to
    // whatever the first host pointed at.
    let (server, _elsewhere) = redirect_to_other_name_for_self("/start", "/end");

    let config = CurlConfig::default()
        .with_allow_egress(AllowByList::new().with_allow_loopback(true));
    let result = curl(config, argv(&["-L", "-u", "user:pass", &server.url("/start")])).await;
    assert_eq!(result.code, 0, "{}", result.err);

    let reqs = server.requests();
    assert_eq!(reqs.len(), 2, "should have made both hops");
    assert!(
        reqs[0].header("authorization").is_some(),
        "the first hop is the one the caller authenticated"
    );
    assert!(
        reqs[1].header("authorization").is_none(),
        "credentials must not follow a redirect to a different host"
    );
}

#[tokio::test]
async fn credentials_survive_a_redirect_that_stays_on_the_same_host() {
    let server = Server::builder()
        .route("/start", Response::redirect(302, "/end"))
        .route("/end", Response::text(200, "arrived"))
        .start_tcp();

    let config = CurlConfig::default()
        .with_allow_egress(AllowByList::new().with_allow_loopback(true));
    let result = curl(config, argv(&["-L", "-u", "user:pass", &server.url("/start")])).await;
    assert_eq!(result.code, 0, "{}", result.err);

    let reqs = server.requests();
    assert_eq!(reqs.len(), 2);
    assert!(
        reqs[1].header("authorization").is_some(),
        "a same-host redirect keeps the credential, as curl does"
    );
}

#[tokio::test]
async fn a_flag_cannot_raise_the_embedders_time_ceiling() {
    // CU34: `max_time.unwrap_or(config)` let an agent override the embedder
    // outright, while `Limits`'s own doc said a flag may only lower it.
    let server = Server::builder()
        .route_fn("/slow", |_req| {
            std::thread::sleep(std::time::Duration::from_millis(600));
            Response::text(200, "eventually")
        })
        .start_tcp();

    let limits = Limits { max_time: 0.2, ..Limits::default() };
    let config = CurlConfig::default()
        .with_limits(limits)
        .with_allow_egress(AllowByList::new().with_allow_loopback(true));

    // The agent asks for an hour; the embedder said 0.2s, and the embedder wins.
    let result = curl(config, argv(&["--max-time", "3600", &server.url("/slow")])).await;
    assert_eq!(result.code, 28, "should time out at the embedder's ceiling: {}", result.err);
}

#[tokio::test]
async fn a_body_over_the_cap_fails_instead_of_being_quietly_truncated() {
    // CU35: the body was read whole, trimmed to the cap, and returned Ok — so
    // `-o` wrote a silently corrupt file and reported success.
    let server = Server::builder()
        .route("/big", Response::text(200, "x".repeat(4096)))
        .start_tcp();

    let limits = Limits { max_response_bytes: 64, ..Limits::default() };
    let config = CurlConfig::default()
        .with_limits(limits)
        .with_allow_egress(AllowByList::new().with_allow_loopback(true));

    let result = curl(config, argv(&[&server.url("/big")])).await;
    assert_ne!(result.code, 0, "over the cap must not be a success");
    assert!(
        result.err.contains("64") && result.err.contains("limit"),
        "the error should name the limit that was hit: {}",
        result.err
    );
}

#[tokio::test]
async fn url_userinfo_cannot_impersonate_an_allowlisted_host() {
    // CU24, end to end: the allowlist names the harness's host, and the URL
    // spells that host into the userinfo of a different one. The old lexical
    // split saw the allowlisted name and let the request out.
    let server = Server::builder()
        .route("/x", Response::text(200, "reached"))
        .start_tcp();
    let port = server.url("/").rsplit(':').next().unwrap().trim_end_matches('/').to_string();

    let config = CurlConfig::default()
        .with_allow_egress(AllowByList::new().with_allowed_hosts(["allowed.example"]));
    let sneaky = format!("http://allowed.example:{port}@127.0.0.1:{port}/x");
    let result = curl(config, argv(&[&sneaky])).await;

    assert_eq!(result.code, 7, "userinfo must not satisfy the allowlist: {}", result.err);
    assert!(result.err.contains("egress allowlist"), "{}", result.err);
    assert!(server.requests().is_empty(), "the request must never leave");
}

#[tokio::test]
async fn injected_headers_win_and_stop_at_a_host_boundary() {
    // CU46: an embedder holding a credential on the agent's behalf. The agent
    // must not be able to displace it with its own -H, and it must not follow
    // a Location off the host it was meant for.
    let (server, _elsewhere) = redirect_to_other_name_for_self("/start", "/end");

    let config = CurlConfig::default()
        .with_allow_egress(AllowByList::new().with_allow_loopback(true))
        .with_injected_headers([("Authorization", "Bearer embedder-token")]);

    let result = curl(
        config,
        argv(&["-L", "-H", "Authorization: Bearer agent-guess", &server.url("/start")]),
    )
    .await;
    assert_eq!(result.code, 0, "{}", result.err);

    let reqs = server.requests();
    assert_eq!(reqs.len(), 2, "should have made both hops");
    assert_eq!(
        reqs[0].header("authorization"),
        Some("Bearer embedder-token"),
        "the embedder's header must win over the agent's -H"
    );
    assert!(
        reqs[1].header("authorization").is_none(),
        "neither the injected credential nor the agent's own -H may cross to \
         another host — stripping only the -u form would leave -H as the way around it"
    );
}

#[tokio::test]
async fn dash_k_is_refused_unless_the_embedder_permits_it() {
    let server = Server::builder()
        .route("/x", Response::text(200, "ok"))
        .start_tcp();

    let result = curl(loopback_config(), argv(&["-k", &server.url("/x")])).await;
    assert_ne!(result.code, 0, "-k is refused by default");
    assert!(result.err.contains("-k"), "{}", result.err);
    assert!(server.requests().is_empty(), "refused before the request left");

    // Permitted, it parses and the request goes out (this server is plaintext,
    // so nothing is verified either way — what is under test is the gate).
    let permitted = loopback_config().with_insecure_permitted(true);
    let result = curl(permitted, argv(&["-k", &server.url("/x")])).await;
    assert_eq!(result.code, 0, "{}", result.err);
}
