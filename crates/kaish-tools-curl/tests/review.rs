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

// ── 7. -w prints even when no response came ─────────────────────────────

#[tokio::test(flavor = "multi_thread")]
async fn write_out_prints_000_when_the_connection_fails() {
    let server = redirector();
    // Nothing listens on port 1.
    let args = "-s -w '%{http_code}|%{url_effective}|%{size_download}|%{num_redirects}|[%{content_type}]\\n' http://127.0.0.1:1/x";
    let p = compare(&server, args, &[]).await;
    assert_eq!(p.ours.code, 7, "{}", p.ours.err);
    assert_eq!(p.ours.text_out(), "000|http://127.0.0.1:1/x|0|0|[]\n");
    assert!(!p.ours.err.is_empty(), "the error still reaches stderr");
    if let Some(theirs) = &p.theirs {
        assert_eq!((theirs.code, theirs.stdout.as_slice()), (7, p.ours.text_out().as_bytes()));
    }
}

// ── 8. --json has nowhere to go with -o ─────────────────────────────────

#[tokio::test(flavor = "multi_thread")]
async fn json_output_with_a_file_is_refused_before_any_request() {
    let server = blob_server();
    for args in ["--json -o out.json \"$URL/text\"", "--json -O \"$URL/text\""] {
        let p = compare(&server, args, &[]).await;
        assert_eq!(p.ours.code, 2, "{args}: {}", p.ours.err);
        assert!(p.ours.err.contains("--json") && p.ours.err.contains("-o"), "{}", p.ours.err);
        assert!(p.ours_req.is_empty(), "{args}: no request was sent");
        assert!(p.fx.file("out.json").await.is_none() && p.fx.file("text").await.is_none());
    }
    // Control: --json alone, and `-o -` (stdout), are fine.
    let p = compare(&server, "--json \"$URL/text\"", &[]).await;
    assert_eq!(p.ours.code, 0, "{}", p.ours.err);
}

// ── 9. -i -L prints every hop's head ────────────────────────────────────

#[tokio::test(flavor = "multi_thread")]
async fn include_with_location_prints_each_hops_status_and_headers() {
    let server = redirector();
    let p = compare(&server, "-s -i -L \"$URL/r301\"", &[]).await;
    assert_eq!(p.ours.code, 0, "{}", p.ours.err);
    let out = p.ours.text_out().to_string();
    let status_lines: Vec<&str> = out.lines().filter(|l| l.starts_with("HTTP/")).map(|l| l.trim_end()).collect();
    assert_eq!(status_lines, ["HTTP/1.1 301 Moved Permanently", "HTTP/1.1 200 OK"], "{out}");
    assert!(out.to_ascii_lowercase().contains("location: /echo"), "the redirect's own headers print: {out}");
    assert!(out.ends_with("landed"), "the final body comes last: {out}");
    if let Some(theirs) = &p.theirs {
        let theirs = String::from_utf8_lossy(&theirs.stdout).into_owned();
        let theirs: Vec<&str> = theirs.lines().filter(|l| l.starts_with("HTTP/")).map(|l| l.trim_end()).collect();
        assert_eq!(theirs, status_lines, "real curl prints the same status lines");
    }
    // Control: without -L there is one head.
    let p = compare(&server, "-s -i \"$URL/r301\"", &[]).await;
    assert_eq!(p.ours.text_out().matches("HTTP/1.1").count(), 1);
}

// ── 10. 0 and -1 mean "no limit of your own" ────────────────────────────

#[tokio::test(flavor = "multi_thread")]
async fn zero_timeouts_and_unlimited_redirects_are_accepted_like_curl() {
    let server = redirector();
    for args in [
        "-s -m 0 \"$URL/echo\"",
        "-s --max-time 0 \"$URL/echo\"",
        "-s --connect-timeout 0 \"$URL/echo\"",
        "-s -L --max-redirs -1 \"$URL/r302\"",
    ] {
        let p = compare(&server, args, &[]).await;
        assert_eq!(p.ours.code, 0, "{args}: {}", p.ours.err);
        if let Some(theirs) = &p.theirs {
            assert_eq!(theirs.code, 0, "{args}: real curl accepts it");
        }
    }
    // -L -1 really follows.
    let p = compare(&server, "-s -L --max-redirs -1 \"$URL/r302\"", &[]).await;
    assert_eq!(p.ours.text_out(), "landed");
    // Controls: other negatives and junk are still refused, as in curl.
    for args in ["-s -m -1 \"$URL/echo\"", "-s --max-redirs -2 \"$URL/echo\"", "-s --connect-timeout -3 \"$URL/echo\""] {
        let p = compare(&server, args, &[]).await;
        assert_eq!(p.ours.code, 2, "{args}: {}", p.ours.err);
        assert!(p.ours_req.is_empty(), "{args}");
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn zero_max_time_leaves_the_embedders_ceiling_in_force() {
    // `-m 0` means "no limit of my own", not "raise the ceiling": the
    // embedder's max_time still bounds the request.
    let server = Server::builder()
        .route_fn("/slow", |_| {
            std::thread::sleep(std::time::Duration::from_millis(1500));
            Response::text(200, "late")
        })
        .start_tcp();
    let config = support::loopback_config().with_limits(kaish_tools_curl::Limits {
        max_time: 0.3,
        ..kaish_tools_curl::Limits::default()
    });
    let fx = support::Fixture::new(config, &[]).await;
    let r = fx.run(&server.url(""), "curl -s -m 0 \"$URL/slow\"").await;
    assert_eq!(r.code, 28, "{}", r.err);
}

// ── 11. A command line this build refuses is exit 2, as in curl ─────────

#[tokio::test(flavor = "multi_thread")]
async fn refused_command_lines_exit_2_like_curl() {
    let server = redirector();
    for args in [
        "--bogus \"$URL/echo\"",
        "",
        "-d",
        "-m abc \"$URL/echo\"",
        "-X",
    ] {
        let p = compare(&server, args, &[]).await;
        assert_eq!(p.ours.code, 2, "{args:?}: {}", p.ours.err);
        assert!(p.ours_req.is_empty(), "{args:?}: no request");
        if let Some(theirs) = &p.theirs {
            assert_eq!(theirs.code, 2, "{args:?}: real curl exits 2 too");
        }
    }
    // The refusals curl has no equivalent for (a flag this build declines,
    // a second URL) are usage errors as well.
    for args in ["--retry 3 \"$URL/echo\"", "\"$URL/echo\" \"$URL/echo\"", "-v \"$URL/echo\""] {
        let p = compare(&server, args, &[]).await;
        assert_eq!(p.ours.code, 2, "{args}: {}", p.ours.err);
    }
}

// ── 12. A short flag's value may be attached ────────────────────────────

#[tokio::test(flavor = "multi_thread")]
async fn attached_short_values_parse_like_curl() {
    let server = redirector();
    for args in [
        "-s -dfoo=1 \"$URL/echo\"",
        // curl takes everything after the flag letter, '=' included.
        "-s '-d=a=1' \"$URL/echo\"",
        "-s -XPUT \"$URL/echo\"",
        "-s -m5 \"$URL/echo\"",
        "-s -uuser:pass \"$URL/echo\"",
        "-s '-HX-A: b' \"$URL/echo\"",
        "-sd x=1 \"$URL/echo\"",
        // A value-taking flag at the end of a cluster takes the rest, or the next word.
        "-sdx=1 \"$URL/echo\"",
        "-sXPUT \"$URL/echo\"",
    ] {
        let p = compare(&server, args, &[]).await;
        assert_eq!(p.ours.code, 0, "{args}: {}", p.ours.err);
        if p.theirs.is_some() {
            assert_eq!(
                p.ours_req.iter().map(Wire::of).collect::<Vec<_>>(),
                p.theirs_req.iter().map(Wire::of).collect::<Vec<_>>(),
                "{args}: differs from real curl"
            );
            assert_eq!(
                p.ours_req[0].header("X-A"),
                p.theirs_req[0].header("X-A"),
                "{args}"
            );
            assert_eq!(
                p.ours_req[0].header("Authorization"),
                p.theirs_req[0].header("Authorization"),
                "{args}"
            );
        }
    }
    let p = compare(&server, "-s '-oattached.txt' \"$URL/echo\"", &[]).await;
    assert_eq!(p.ours.code, 0, "{}", p.ours.err);
    assert_eq!(p.fx.file("attached.txt").await.as_deref(), Some(b"landed".as_slice()));
    let p = compare(&server, "-so out.txt \"$URL/echo\"", &[]).await;
    assert_eq!(p.fx.file("out.txt").await.as_deref(), Some(b"landed".as_slice()), "{}", p.ours.err);
    // The literal assertions, for a box without curl.
    let p = compare(&server, "-dfoo=1 \"$URL/echo\"", &[]).await;
    assert_eq!(p.ours_req[0].body, b"foo=1");
    let p = compare(&server, "'-d=a=1' \"$URL/echo\"", &[]).await;
    assert_eq!(p.ours_req[0].body, b"=a=1");
}

// ── 13. -H forms with an empty value ────────────────────────────────────

#[tokio::test(flavor = "multi_thread")]
async fn header_forms_with_no_value_match_curl() {
    let server = redirector();
    // 'Name;' sends the header with an empty value.
    let p = compare(&server, "-s -H 'X-Empty;' \"$URL/echo\"", &[]).await;
    assert_eq!(p.ours.code, 0, "{}", p.ours.err);
    assert_eq!(p.ours_req[0].header("X-Empty"), Some(""), "sent, empty");
    if p.theirs.is_some() {
        assert_eq!(p.theirs_req[0].header("X-Empty"), Some(""), "real curl agrees");
    }

    // 'Name:' removes a header the client would add itself.
    for name in ["Accept", "User-Agent"] {
        let p = compare(&server, &format!("-s -H '{name}:' \"$URL/echo\""), &[]).await;
        assert_eq!(p.ours.code, 0, "{name}: {}", p.ours.err);
        assert_eq!(p.ours_req[0].header(name), None, "{name} is not sent");
        if p.theirs.is_some() {
            assert_eq!(p.theirs_req[0].header(name), None, "{name}: real curl agrees");
        }
    }
    // The body's own Content-Type goes too.
    let p = compare(&server, "-s -d a=1 -H 'Content-Type:' \"$URL/echo\"", &[]).await;
    assert_eq!(p.ours_req[0].header("Content-Type"), None);
    if p.theirs.is_some() {
        assert_eq!(p.theirs_req[0].header("Content-Type"), None);
    }
    // Controls: a header of the same name that the caller set stays (curl
    // removes only what the client adds), and the defaults are there without the flag.
    let p = compare(&server, "-s -H 'X-A: 1' -H 'X-A:' \"$URL/echo\"", &[]).await;
    assert_eq!(p.ours_req[0].header("X-A"), Some("1"));
    let p = compare(&server, "-s \"$URL/echo\"", &[]).await;
    assert!(p.ours_req[0].header("Accept").is_some() && p.ours_req[0].header("User-Agent").is_some());

    // 'Host:' cannot be removed here, and a bare name is not a header.
    for args in ["-s -H 'Host:' \"$URL/echo\"", "-s -H 'Accept' \"$URL/echo\""] {
        let p = compare(&server, args, &[]).await;
        assert_eq!(p.ours.code, 2, "{args}: {}", p.ours.err);
        assert!(p.ours_req.is_empty(), "{args}");
    }
}

// ── 17. -w treats a backslash and the character after it as a pair ──────

#[tokio::test(flavor = "multi_thread")]
async fn write_out_backslash_pairs_match_curl() {
    let server = redirector();
    for fmt in [
        // An unknown pair prints as typed and does not hide a following '%'.
        r"\%{http_code}|\\n|%%|\q\n",
        r"a\\%{http_code}\n",
        r"x\tz\n",
        r"end\",
    ] {
        let args = format!("-s -o /dev/null -w '{fmt}' \"$URL/echo\"");
        let p = compare(&server, &args, &[]).await;
        assert_eq!(p.ours.code, 0, "{fmt}: {}", p.ours.err);
        let Some(theirs) = &p.theirs else { continue };
        assert_eq!(p.ours.text_out().as_bytes(), theirs.stdout.as_slice(), "{fmt}");
    }
    // The literal, for a box without curl.
    let p = compare(&server, "-s -o /dev/null -w '\\%{http_code}|\\\\n|\\q\\n' \"$URL/echo\"", &[]).await;
    assert_eq!(p.ours.text_out(), "\\%{http_code}|\\\\n|\\q\n");
}

// ── 16. A gate with curl must not skip the comparison ───────────────────

#[test]
fn a_missing_curl_is_a_skip_unless_the_gate_requires_it() {
    assert!(support::oracle_policy(true, true) && support::oracle_policy(true, false));
    assert!(!support::oracle_policy(false, false), "no curl, not required: skip");
}

#[test]
#[should_panic(expected = "KAISH_REQUIRE_CURL_ORACLE")]
fn a_missing_curl_panics_where_the_gate_requires_it() {
    support::oracle_policy(false, true);
}
