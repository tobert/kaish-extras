//! Findings from the 2026-11 cross-model review of the curl tool (deepseek-flash
//! via kaibo), each checked against real curl 8.23 on the same loopback server
//! before it was fixed. A case runs real curl first, then this tool, and
//! compares what the server received, the exit code, stdout, and any file
//! written. Without curl installed the oracle half is skipped (or, where
//! `KAISH_REQUIRE_CURL_ORACLE` is set, a panic) and the literal assertions
//! still run.

#[path = "support.rs"]
mod support;

use support::{compare, compare_with, Response, Server, TcpGuard, Wire};

fn redirector() -> TcpGuard {
    Server::builder()
        .route("/echo", Response::text(200, "landed"))
        .route("/r301", Response::redirect(301, "/echo"))
        .route("/r302", Response::redirect(302, "/echo"))
        .route("/r303", Response::redirect(303, "/echo"))
        .route("/r307", Response::redirect(307, "/echo"))
        .route("/r308", Response::redirect(308, "/echo"))
        .start_tcp()
}

// ── 1. A redirect does not replay a POST body ───────────────────────────

#[tokio::test(flavor = "multi_thread")]
async fn post_body_is_dropped_across_301_302_303_and_kept_across_307_308() {
    let server = redirector();
    for (status, kept) in [(301, false), (302, false), (303, false), (307, true), (308, true)] {
        let p = compare(&server, &format!("-s -L -d a=1 -H 'X-Keep: y' \"$URL/r{status}\""), &[]).await;
        assert_eq!(p.ours.code, 0, "{status}: {}", p.ours.err);
        assert_eq!(p.ours_req.len(), 2, "{status}: one request per hop");
        let hop = &p.ours_req[1];
        if kept {
            assert_eq!((hop.method.as_str(), hop.body.as_slice()), ("POST", b"a=1".as_slice()), "{status}");
            assert!(hop.header("Content-Type").is_some(), "{status}");
        } else {
            assert_eq!(hop.method, "GET", "{status}: curl switches to GET");
            assert!(hop.body.is_empty(), "{status}: the body must not follow the redirect");
            assert!(hop.header("Content-Length").is_none(), "{status}: no Content-Length");
            assert!(hop.header("Content-Type").is_none(), "{status}: the form type went with the body");
        }
        assert_eq!(hop.header("X-Keep"), Some("y"), "{status}: -H headers follow");
        if let Some(theirs) = &p.theirs {
            assert_eq!(theirs.code, 0);
            assert_eq!(
                p.ours_req.iter().map(Wire::of).collect::<Vec<_>>(),
                p.theirs_req.iter().map(Wire::of).collect::<Vec<_>>(),
                "{status}: differs from real curl"
            );
        }
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn redirect_method_rules_match_curl_for_x_t_and_f() {
    let server = redirector();
    let files: &[(&str, &[u8])] = &[("up.txt", b"hi\n")];
    for args in [
        // An explicit -X keeps its name; the body still goes.
        "-s -L -X POST -d a=1 \"$URL/r302\"",
        "-s -L -X PUT -d a=1 \"$URL/r302\"",
        "-s -L -X DELETE \"$URL/r303\"",
        // -T is a PUT: 301/302/307 keep it and the file, 303 makes it a GET.
        "-s -L -T up.txt \"$URL/r302\"",
        "-s -L -T up.txt \"$URL/r303\"",
        "-s -L -T up.txt \"$URL/r307\"",
        "-s -L -F a=b \"$URL/r302\"",
        // A Content-Type the caller set is a header like any other.
        "-s -L -d a=1 -H 'Content-Type: text/x' \"$URL/r302\"",
        // A HEAD stays a HEAD.
        "-s -L -I \"$URL/r302\"",
    ] {
        let p = compare(&server, args, files).await;
        if p.theirs.is_none() {
            continue;
        }
        // The redirected hop is what this case is about. The first hop of a
        // `-F` differs by design: the boundary is random per request.
        assert_eq!(p.ours_req.len(), p.theirs_req.len(), "{args}: hop count");
        assert_eq!(
            p.ours_req.iter().skip(1).map(Wire::of).collect::<Vec<_>>(),
            p.theirs_req.iter().skip(1).map(Wire::of).collect::<Vec<_>>(),
            "{args}: redirected hop differs from real curl (stderr: {})",
            p.ours.err
        );
    }
}

// ── 2. One Authorization header, chosen by a rule ───────────────────────

fn authorizations(server: &TcpGuard) -> Vec<String> {
    server
        .last_request()
        .expect("a request arrived")
        .headers_named("Authorization")
        .into_iter()
        .map(str::to_string)
        .collect()
}

#[tokio::test(flavor = "multi_thread")]
async fn a_header_authorization_overrides_user_like_curl_and_goes_out_once() {
    let server = redirector();
    for args in [
        "-s -u a:b -H 'Authorization: Bearer X' \"$URL/echo\"",
        "-s -H 'Authorization: Bearer X' -u a:b \"$URL/echo\"",
    ] {
        let p = compare(&server, args, &[]).await;
        assert_eq!(p.ours.code, 0, "{}", p.ours.err);
        assert_eq!(
            p.ours_req.last().unwrap().headers_named("Authorization"),
            vec!["Bearer X"],
            "{args}: exactly one, and it is the -H one"
        );
        if p.theirs.is_some() {
            assert_eq!(
                p.theirs_req.last().unwrap().headers_named("Authorization"),
                vec!["Bearer X"],
                "real curl agrees"
            );
        }
    }
    // Control: with no -H, -u is sent.
    let p = compare(&server, "-s -u a:b \"$URL/echo\"", &[]).await;
    assert_eq!(p.ours_req.last().unwrap().headers_named("Authorization"), vec!["Basic YTpi"]);
}

#[tokio::test(flavor = "multi_thread")]
async fn an_injected_authorization_is_never_accompanied_by_an_agent_one() {
    let server = redirector();
    let config = support::loopback_config().with_injected_headers([("Authorization", "Bearer embedder")]);
    let fx = support::Fixture::new(config, &[]).await;
    for args in [
        "-s -u a:b \"$URL/echo\"",
        "-s -H 'Authorization: Bearer agent' \"$URL/echo\"",
        "-s -u a:b -H 'authorization: Bearer agent' \"$URL/echo\"",
        "-s -u a:b \"$URL/r302\" -L",
    ] {
        let r = fx.run(&server.url(""), &format!("curl {args}")).await;
        assert_eq!(r.code, 0, "{args}: {}", r.err);
        assert_eq!(authorizations(&server), vec!["Bearer embedder"], "{args}");
    }
}

// ── 3, 4, 5. What -o writes ─────────────────────────────────────────────

fn blob_server() -> TcpGuard {
    Server::builder()
        .route(
            "/blob",
            Response::new(200)
                .header("Content-Type", "application/octet-stream")
                .body(vec![0x00u8, 0xff, 0xfe, 0x80, b'\r', b'\n', 0x00]),
        )
        .route("/text", Response::text(200, "plain\n"))
        .start_tcp()
}

#[tokio::test(flavor = "multi_thread")]
async fn include_with_output_file_keeps_binary_bytes_intact() {
    let server = blob_server();
    let p = compare(&server, "-s -i -o out.bin \"$URL/blob\"", &[]).await;
    assert_eq!(p.ours.code, 0, "{}", p.ours.err);
    let ours = p.fx.file("out.bin").await.expect("the file was written");
    let body = [0x00u8, 0xff, 0xfe, 0x80, b'\r', b'\n', 0x00];
    assert!(ours.starts_with(b"HTTP/1.1 200 OK\r\n"), "headers lead: {:?}", String::from_utf8_lossy(&ours));
    assert!(ours.ends_with(&body), "the body bytes are written as received, not through UTF-8: {ours:?}");
    let split = ours.windows(4).position(|w| w == b"\r\n\r\n").expect("blank line") + 4;
    assert_eq!(&ours[split..], body, "nothing but the body after the headers");
    if let Some(theirs) = &p.theirs {
        let theirs = theirs.file("out.bin").expect("curl wrote it");
        assert!(theirs.ends_with(&body), "real curl agrees on the tail");
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn head_with_output_file_writes_the_headers() {
    let server = blob_server();
    let p = compare(&server, "-s -I -o h.txt \"$URL/text\"", &[]).await;
    assert_eq!(p.ours.code, 0, "{}", p.ours.err);
    let ours = p.fx.file("h.txt").await.expect("the file was written");
    assert!(ours.starts_with(b"HTTP/1.1 200 OK\r\n"), "{:?}", String::from_utf8_lossy(&ours));
    assert!(ours.ends_with(b"\r\n\r\n"), "headers end at the blank line and nothing follows");
    if let Some(theirs) = &p.theirs {
        let theirs = theirs.file("h.txt").expect("curl wrote it");
        assert!(theirs.starts_with(b"HTTP/1.1 200 OK\r\n") && theirs.ends_with(b"\r\n\r\n"));
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn output_dash_is_stdout_not_a_file_named_dash() {
    let server = blob_server();
    let p = compare(&server, "-s -o - \"$URL/text\"", &[]).await;
    assert_eq!(p.ours.code, 0, "{}", p.ours.err);
    assert_eq!(p.ours.text_out(), "plain\n");
    assert!(p.fx.file("-").await.is_none(), "no file named '-'");
    if let Some(theirs) = &p.theirs {
        assert_eq!(theirs.stdout, b"plain\n");
    }
}

// ── 6. -f decides at the headers ────────────────────────────────────────

#[tokio::test(flavor = "multi_thread")]
async fn fail_is_decided_before_the_body_is_read() {
    let server = Server::builder()
        .route("/big404", Response::json(404, "x".repeat(4096)))
        .route("/ok", Response::text(200, "fine\n"))
        .start_tcp();
    // A cap far below the 404 body: reading it first would fail with exit 1.
    let small = support::loopback_config().with_limits(kaish_tools_curl::Limits {
        max_response_bytes: 64,
        ..kaish_tools_curl::Limits::default()
    });
    let p = compare_with(
        &server,
        "-s -f -w '%{http_code} %{size_download}\\n' -o out.txt \"$URL/big404\"",
        &[],
        small,
    )
    .await;
    assert_eq!(p.ours.code, 22, "an over-cap error body must not turn 22 into 1: {}", p.ours.err);
    assert_eq!(p.ours.text_out(), "404 0\n", "-w still prints, and nothing was downloaded");
    assert!(p.fx.file("out.txt").await.is_none(), "no file under -f");
    if let Some(theirs) = &p.theirs {
        assert_eq!((theirs.code, theirs.stdout.as_slice()), (22, b"404 0\n".as_slice()));
    }

    // Controls: --fail-with-body still reads (and so still hits the cap), and
    // a 200 under -f is untouched.
    let small = support::loopback_config().with_limits(kaish_tools_curl::Limits {
        max_response_bytes: 64,
        ..kaish_tools_curl::Limits::default()
    });
    let p = compare_with(&server, "-s --fail-with-body \"$URL/big404\"", &[], small).await;
    assert_eq!(p.ours.code, 1, "the body is wanted, so the cap applies: {}", p.ours.err);
    let p = compare(&server, "-s -f \"$URL/ok\"", &[]).await;
    assert_eq!((p.ours.code, p.ours.text_out().as_ref()), (0, "fine\n"));
}
