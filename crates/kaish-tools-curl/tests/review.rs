//! Findings from the 2026-11 cross-model review of the curl tool (deepseek-flash
//! via kaibo), each checked against real curl 8.23 on the same loopback server
//! before it was fixed. A case runs real curl first, then this tool, and
//! compares what the server received, the exit code, stdout, and any file
//! written. Without curl installed the oracle half is skipped (or, where
//! `KAISH_REQUIRE_CURL_ORACLE` is set, a panic) and the literal assertions
//! still run.

#[path = "support.rs"]
mod support;

use support::{compare, Response, Server, TcpGuard, Wire};

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
