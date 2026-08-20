//! Happy-path functional tests: the flag table in docs/curl.md, each one
//! asserting on what the SERVER actually received (not just what the tool
//! rendered back), then on what the tool returned.
//!
//! Every server here runs on `127.0.0.1`, so every call threads
//! `support::loopback_config()` — `CurlConfig::default().with_allow_egress(
//! AllowByList::new().with_allow_loopback(true))`. Without it the request
//! never leaves the tool (see `errors.rs`'s egress-denial test), and every
//! test below would silently exercise the wrong thing.

#[path = "support.rs"]
mod support;

use std::path::Path;
use std::sync::Arc;

use base64::{engine::general_purpose::STANDARD, Engine as _};

use support::{argv, curl, curl_with_backend, loopback_config, MemoryBackend, Response, Server};

#[tokio::test]
async fn get_reaches_the_server_and_the_body_reaches_stdout() {
    let server = Server::builder()
        .route("/hello", Response::text(200, "hello world"))
        .start_tcp();
    let url = server.url("/hello");

    let result = curl(loopback_config(), argv(&[&url])).await;
    assert_eq!(result.code, 0, "curl failed: {}", result.err);
    assert_eq!(result.text_out(), "hello world");

    let req = server.last_request().expect("server received a request");
    assert_eq!(req.method, "GET");
    assert_eq!(req.path_no_query(), "/hello");
}

#[tokio::test]
async fn default_user_agent_is_sent_exactly_once() {
    let server = Server::builder().route("/ua", Response::text(200, "")).start_tcp();
    let url = server.url("/ua");

    let result = curl(loopback_config(), argv(&[&url])).await;
    assert_eq!(result.code, 0, "{}", result.err);

    let req = server.last_request().expect("request received");
    assert_eq!(req.headers_named("User-Agent"), vec!["kaish-curl"]);
}

#[tokio::test]
async fn dash_a_overrides_the_user_agent_without_duplicating_it() {
    let server = Server::builder().route("/ua", Response::text(200, "")).start_tcp();
    let url = server.url("/ua");

    let result = curl(loopback_config(), argv(&["-A", "my-agent/1.0", &url])).await;
    assert_eq!(result.code, 0, "{}", result.err);

    let req = server.last_request().expect("request received");
    // A duplicated User-Agent header was a real bug here (see args.rs's
    // module doc) — the count assertion is the point, not just presence.
    assert_eq!(req.headers_named("User-Agent"), vec!["my-agent/1.0"]);
}

#[tokio::test]
async fn header_flag_user_agent_overrides_without_duplicating_it() {
    let server = Server::builder().route("/ua", Response::text(200, "")).start_tcp();
    let url = server.url("/ua");

    let result = curl(
        loopback_config(),
        argv(&["-H", "User-Agent: header-agent/2.0", &url]),
    )
    .await;
    assert_eq!(result.code, 0, "{}", result.err);

    let req = server.last_request().expect("request received");
    assert_eq!(req.headers_named("User-Agent"), vec!["header-agent/2.0"]);
}

#[tokio::test]
async fn referer_flag_sends_the_referer_header() {
    let server = Server::builder().route("/r", Response::text(200, "")).start_tcp();
    let url = server.url("/r");

    let result = curl(
        loopback_config(),
        argv(&["-e", "http://referring.example/page", &url]),
    )
    .await;
    assert_eq!(result.code, 0, "{}", result.err);

    let req = server.last_request().expect("request received");
    assert_eq!(req.header("Referer"), Some("http://referring.example/page"));
}

#[tokio::test]
async fn repeated_data_flags_post_form_encoded_and_join_with_ampersand() {
    let server = Server::builder().route("/form", Response::text(200, "")).start_tcp();
    let url = server.url("/form");

    let result = curl(loopback_config(), argv(&["-d", "a=1", "-d", "b=2", &url])).await;
    assert_eq!(result.code, 0, "{}", result.err);

    let req = server.last_request().expect("request received");
    assert_eq!(req.method, "POST");
    assert_eq!(req.body_str(), "a=1&b=2");
    assert_eq!(
        req.headers_named("Content-Type"),
        vec!["application/x-www-form-urlencoded"]
    );
}

#[tokio::test]
async fn explicit_content_type_overrides_the_form_default_without_duplicating_it() {
    let server = Server::builder().route("/json", Response::text(200, "")).start_tcp();
    let url = server.url("/json");

    let result = curl(
        loopback_config(),
        argv(&["-d", "{}", "-H", "Content-Type: application/json", &url]),
    )
    .await;
    assert_eq!(result.code, 0, "{}", result.err);

    let req = server.last_request().expect("request received");
    assert_eq!(req.headers_named("Content-Type"), vec!["application/json"]);
}

#[tokio::test]
async fn data_urlencode_percent_encodes_only_the_value() {
    let server = Server::builder().route("/enc", Response::text(200, "")).start_tcp();
    let url = server.url("/enc");

    let result = curl(loopback_config(), argv(&["--data-urlencode", "q=a b&c", &url])).await;
    assert_eq!(result.code, 0, "{}", result.err);

    let req = server.last_request().expect("request received");
    assert_eq!(req.method, "POST");
    assert_eq!(req.body_str(), "q=a%20b%26c");
}

#[tokio::test]
async fn dash_x_overrides_the_method() {
    let server = Server::builder().route("/put", Response::text(200, "")).start_tcp();
    let url = server.url("/put");

    let result = curl(loopback_config(), argv(&["-X", "PUT", &url])).await;
    assert_eq!(result.code, 0, "{}", result.err);

    let req = server.last_request().expect("request received");
    assert_eq!(req.method, "PUT");
}

#[tokio::test]
async fn dash_u_sends_basic_auth() {
    let server = Server::builder().route("/auth", Response::text(200, "")).start_tcp();
    let url = server.url("/auth");

    let result = curl(loopback_config(), argv(&["-u", "user:pass", &url])).await;
    assert_eq!(result.code, 0, "{}", result.err);

    let req = server.last_request().expect("request received");
    let expected = format!("Basic {}", STANDARD.encode("user:pass"));
    assert_eq!(req.header("Authorization"), Some(expected.as_str()));
}

#[tokio::test]
async fn dash_i_prints_the_status_line_and_headers_above_the_body() {
    let server = Server::builder()
        .route("/i", Response::text(200, "body-text"))
        .start_tcp();
    let url = server.url("/i");

    let result = curl(loopback_config(), argv(&["-i", &url])).await;
    assert_eq!(result.code, 0, "{}", result.err);

    let text = result.text_out();
    let lower = text.to_lowercase();
    assert!(text.starts_with("HTTP/1.1 200\r\n"), "should lead with the status line: {text:?}");
    assert!(lower.contains("content-type: text/plain"), "headers should be printed: {text:?}");
    assert!(text.ends_with("body-text"), "body should follow the headers: {text:?}");
    let headers_pos = text.find("HTTP/1.1").unwrap();
    let body_pos = text.find("body-text").unwrap();
    assert!(headers_pos < body_pos, "headers must come before the body");
}

#[tokio::test]
async fn dash_capital_i_issues_head_and_prints_headers_with_no_body() {
    let server = Server::builder()
        .route("/head", Response::text(200, "should-never-appear"))
        .start_tcp();
    let url = server.url("/head");

    let result = curl(loopback_config(), argv(&["-I", &url])).await;
    assert_eq!(result.code, 0, "{}", result.err);

    let text = result.text_out();
    assert!(text.starts_with("HTTP/1.1 200\r\n"));
    assert!(
        !text.contains("should-never-appear"),
        "-I must discard the body: {text:?}"
    );

    let req = server.last_request().expect("request received");
    assert_eq!(req.method, "HEAD");
}

#[tokio::test]
async fn dash_o_writes_the_body_through_the_tool_ctx_backend() {
    let server = Server::builder()
        .route("/download", Response::text(200, "file-contents"))
        .start_tcp();
    let url = server.url("/download");

    let backend = Arc::new(MemoryBackend::new());
    let result = curl_with_backend(
        loopback_config(),
        argv(&["-o", "/out/file.txt", &url]),
        Arc::clone(&backend),
    )
    .await;
    assert_eq!(result.code, 0, "{}", result.err);

    let written = backend
        .written(Path::new("/out/file.txt"))
        .expect("-o should have written through the backend");
    assert_eq!(written, b"file-contents");
}

#[tokio::test]
async fn dash_i_dash_o_together_put_the_headers_in_the_file_too() {
    let server = Server::builder()
        .route("/download", Response::text(200, "file-contents"))
        .start_tcp();
    let url = server.url("/download");

    let backend = Arc::new(MemoryBackend::new());
    let result = curl_with_backend(
        loopback_config(),
        argv(&["-i", "-o", "/out/file.txt", &url]),
        Arc::clone(&backend),
    )
    .await;
    assert_eq!(result.code, 0, "{}", result.err);

    let written = backend
        .written(Path::new("/out/file.txt"))
        .expect("-o should have written through the backend");
    let text = String::from_utf8(written).expect("headers+body are valid utf-8 here");
    assert!(text.starts_with("HTTP/1.1 200\r\n"), "headers should lead the file: {text:?}");
    assert!(text.ends_with("file-contents"), "body should still be in the file: {text:?}");
}

#[tokio::test]
async fn dash_capital_l_follows_a_redirect() {
    let server = Server::builder()
        .route("/start", Response::redirect(302, "/end"))
        .route("/end", Response::text(200, "landed"))
        .start_tcp();
    let url = server.url("/start");

    let result = curl(loopback_config(), argv(&["-L", &url])).await;
    assert_eq!(result.code, 0, "{}", result.err);
    assert_eq!(result.text_out(), "landed");

    let reqs = server.requests();
    assert_eq!(reqs.len(), 2, "should have hit both the redirect and its target");
    assert_eq!(reqs[0].path_no_query(), "/start");
    assert_eq!(reqs[1].path_no_query(), "/end");
}

#[tokio::test]
async fn without_dash_capital_l_the_redirect_is_returned_as_is() {
    let server = Server::builder()
        .route("/start", Response::redirect(302, "/end"))
        .route("/end", Response::text(200, "landed"))
        .start_tcp();
    let url = server.url("/start");

    let result = curl(loopback_config(), argv(&[&url])).await;
    assert_eq!(result.code, 0, "a bare 302 is not a failure without --fail: {}", result.err);
    assert_eq!(result.baggage.get("curl.status").map(String::as_str), Some("302"));

    let reqs = server.requests();
    assert_eq!(reqs.len(), 1, "must not follow the redirect on its own");
}

#[tokio::test]
async fn max_redirs_caps_a_two_hop_chain() {
    let server = Server::builder()
        .route("/a", Response::redirect(302, "/b"))
        .route("/b", Response::redirect(302, "/c"))
        .route("/c", Response::text(200, "should not be reached with a cap of 1"))
        .start_tcp();
    let url = server.url("/a");

    let result = curl(loopback_config(), argv(&["-L", "--max-redirs", "1", &url])).await;
    assert_eq!(
        result.code, 47,
        "a two-hop chain over --max-redirs 1 should hit TooManyRedirects (exit 47): {}",
        result.err
    );
}
