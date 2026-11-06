//! Request bodies from files, `-G`, `-F`, `-O`, `-T`, and `-C -`, each run
//! against real curl on the same loopback server.
//!
//! A case seeds the same files for both: into the kernel's in-memory
//! filesystem for this tool, into a scratch directory for real curl. It runs
//! real curl first, then this tool, and compares what the server received
//! (method, request target, `Content-Type`, `Content-Length`, `Range`, body
//! bytes), plus exit code, stdout and any file written. The `User-Agent`,
//! `Accept` and `Host` headers differ by design and are not compared.
//!
//! When no curl is installed the oracle half is skipped and the case still
//! runs its own assertions, so a green run on such a box proves less. The
//! assertions that name literal bytes are there so it still proves
//! something.

#[path = "support.rs"]
mod support;

use kaish_types::ExecResult;
use support::{loopback_config, CapturedRequest, Fixture, Oracle, Response, Server, TcpGuard, Wire};

use kaish_tools_curl::Limits;

/// A server that records every request and answers `ok`.
fn recorder() -> TcpGuard {
    Server::builder()
        .route("/echo", Response::text(200, "ok"))
        .route("/", Response::text(200, "root"))
        .start_tcp()
}

/// Both sides of one case.
struct Pair {
    ours: ExecResult,
    ours_req: Vec<CapturedRequest>,
    theirs: Option<Oracle>,
    theirs_req: Vec<CapturedRequest>,
    /// Kept for `file()` reads in cases that check what was written.
    #[allow(dead_code)]
    fx: Fixture,
}

/// Run `args` (the text after `curl`, with `$URL` for the server base) under
/// real curl and under this tool, each with `files` seeded.
async fn compare(server: &TcpGuard, args: &str, files: &[(&str, &[u8])]) -> Pair {
    compare_with(server, args, files, loopback_config()).await
}

async fn compare_with(
    server: &TcpGuard,
    args: &str,
    files: &[(&str, &[u8])],
    config: kaish_tools_curl::CurlConfig,
) -> Pair {
    let base = server.url("");
    server.take_requests();
    let theirs = Oracle::run(&base, args, files);
    let theirs_req = server.take_requests();
    let fx = Fixture::new(config, files).await;
    let ours = fx.run(&base, &format!("curl {args}")).await;
    let ours_req = server.take_requests();
    Pair { ours, ours_req, theirs, theirs_req, fx }
}

impl Pair {
    /// Same requests on the wire, same exit code, same stdout.
    fn assert_same(&self, what: &str) {
        let Some(theirs) = &self.theirs else { return };
        assert_eq!(
            self.ours_req.iter().map(Wire::of).collect::<Vec<_>>(),
            self.theirs_req.iter().map(Wire::of).collect::<Vec<_>>(),
            "{what}: request differs from real curl's (stderr: {})",
            self.ours.err
        );
        assert_eq!(self.ours.code as i32, theirs.code, "{what}: exit code; ours stderr: {}", self.ours.err);
        assert_eq!(
            self.ours.text_out().as_bytes(),
            theirs.stdout.as_slice(),
            "{what}: stdout"
        );
    }

    /// Same requests and exit code; stdout is not compared (`-i` and `-I`
    /// print header names lower-cased and sorted, curl prints them as sent).
    fn assert_same_wire(&self, what: &str) {
        let Some(theirs) = &self.theirs else { return };
        assert_eq!(
            self.ours_req.iter().map(Wire::of).collect::<Vec<_>>(),
            self.theirs_req.iter().map(Wire::of).collect::<Vec<_>>(),
            "{what}: request differs from real curl's"
        );
        assert_eq!(self.ours.code as i32, theirs.code, "{what}: exit code");
    }

    fn body(&self) -> &[u8] {
        &self.ours_req.first().expect("a request was sent").body
    }
}

// ── 1. Request bodies from files ────────────────────────────────────────

const CRLF: &[u8] = b"a\r\nb\nc\n";

#[tokio::test(flavor = "multi_thread")]
async fn data_at_file_strips_every_cr_and_lf_like_curl() {
    let server = recorder();
    let p = compare(&server, "-s -d @f.txt \"$URL/echo\"", &[("f.txt", CRLF)]).await;
    assert_eq!(p.ours.code, 0, "{}", p.ours.err);
    assert_eq!(p.body(), b"abc", "CR and LF are removed wherever they sit, not only at the end");
    p.assert_same("-d @file");
    assert_eq!(
        Wire::of(&p.ours_req[0]).content_type.as_deref(),
        Some("application/x-www-form-urlencoded")
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn data_binary_at_file_keeps_every_byte() {
    let server = recorder();
    let bytes: &[u8] = b"a\r\nb\0\xff\n";
    let p = compare(&server, "-s --data-binary @f.bin \"$URL/echo\"", &[("f.bin", bytes)]).await;
    assert_eq!(p.ours.code, 0, "{}", p.ours.err);
    assert_eq!(p.body(), bytes);
    // curl 8.23 sends the form Content-Type here too; assert_same compares it.
    assert_eq!(
        Wire::of(&p.ours_req[0]).content_type.as_deref(),
        Some("application/x-www-form-urlencoded")
    );
    p.assert_same("--data-binary @file");
}

#[tokio::test(flavor = "multi_thread")]
async fn data_raw_takes_the_at_sign_literally() {
    let server = recorder();
    let p = compare(&server, "-s --data-raw @f.txt \"$URL/echo\"", &[("f.txt", CRLF)]).await;
    assert_eq!(p.body(), b"@f.txt");
    p.assert_same("--data-raw @file");
}

#[tokio::test(flavor = "multi_thread")]
async fn file_and_text_parts_join_with_an_ampersand_in_order() {
    let server = recorder();
    let files: &[(&str, &[u8])] = &[("f.txt", CRLF), ("g.txt", b"x\n")];
    let p = compare(&server, "-s -d @f.txt -d k=v --data-binary @g.txt \"$URL/echo\"", files).await;
    assert_eq!(p.body(), b"abc&k=v&x\n");
    p.assert_same("mixed parts");
}

#[tokio::test(flavor = "multi_thread")]
async fn a_bare_dash_is_the_literal_body_dash_not_stdin() {
    let server = recorder();
    for flag in ["-d", "--data-binary", "--data-raw"] {
        let p = compare(&server, &format!("-s {flag} - \"$URL/echo\""), &[]).await;
        assert_eq!(p.ours.code, 0, "{flag}: {}", p.ours.err);
        assert_eq!(p.body(), b"-", "{flag}");
        p.assert_same(flag);
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn a_missing_body_file_sends_nothing_and_exits_26() {
    let server = recorder();
    for flag in ["-d", "--data-binary"] {
        let p = compare(&server, &format!("-s {flag} @nope.txt \"$URL/echo\""), &[]).await;
        assert_eq!(p.ours.code, 26, "{flag}: {}", p.ours.err);
        assert!(p.ours.err.contains("nope.txt"), "name the file: {}", p.ours.err);
        assert!(p.ours_req.is_empty(), "no request when the body cannot be read");
        if let Some(t) = &p.theirs {
            assert_eq!(t.code, 26);
            assert!(p.theirs_req.is_empty());
        }
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn a_directory_is_not_a_body_file() {
    let server = recorder();
    let p = compare(&server, "-s -d @sub \"$URL/echo\"", &[("sub/x", b"1")]).await;
    assert_eq!(p.ours.code, 26, "{}", p.ours.err);
    assert!(p.ours_req.is_empty());
}

#[tokio::test(flavor = "multi_thread")]
async fn stdin_body_is_refused_with_the_way_forward() {
    let server = recorder();
    let fx = Fixture::new(loopback_config(), &[]).await;
    for flag in ["-d", "--data-binary"] {
        let r = fx
            .run(&server.url(""), &format!("echo hi | curl -s {flag} @- \"$URL/echo\""))
            .await;
        assert_eq!(r.code, 2, "{flag}: {}", r.err);
        assert!(r.err.contains("stdin"), "{flag}: {}", r.err);
        assert!(r.err.contains("no request was sent"), "{flag}: {}", r.err);
        assert!(r.err.contains("@<path>") || r.err.contains("-d @"), "{flag} names the file form: {}", r.err);
    }
    assert!(server.requests().is_empty());
}

#[tokio::test(flavor = "multi_thread")]
async fn a_body_file_over_the_byte_limit_is_refused_before_any_request() {
    let server = recorder();
    let big = vec![b'x'; 2048];
    let config = loopback_config().with_limits(Limits { max_response_bytes: 1024, ..Limits::default() });
    let p = compare_with(&server, "-s --data-binary @big \"$URL/echo\"", &[("big", &big)], config).await;
    assert_ne!(p.ours.code, 0);
    assert!(p.ours.err.contains("1024"), "name the limit: {}", p.ours.err);
    assert!(p.ours.err.contains("big"), "name the file: {}", p.ours.err);
    assert!(p.ours_req.is_empty(), "nothing is sent past the budget");

    // Control: the same file under a budget that holds it goes through.
    let config = loopback_config().with_limits(Limits { max_response_bytes: 4096, ..Limits::default() });
    let p = compare_with(&server, "-s --data-binary @big \"$URL/echo\"", &[("big", &big)], config).await;
    assert_eq!(p.ours.code, 0, "{}", p.ours.err);
    assert_eq!(p.body().len(), 2048);
}

#[tokio::test(flavor = "multi_thread")]
async fn a_summed_body_over_the_limit_is_refused() {
    let server = recorder();
    let chunk = vec![b'y'; 700];
    let config = loopback_config().with_limits(Limits { max_response_bytes: 1024, ..Limits::default() });
    let files: &[(&str, &[u8])] = &[("a", &chunk), ("b", &chunk)];
    let p = compare_with(&server, "-s --data-binary @a --data-binary @b \"$URL/echo\"", files, config).await;
    assert_ne!(p.ours.code, 0, "two files that fit alone but not together");
    assert!(p.ours_req.is_empty());
}

// ── --data-urlencode ────────────────────────────────────────────────────

#[tokio::test(flavor = "multi_thread")]
async fn data_urlencode_forms_match_curl() {
    let server = recorder();
    let files: &[(&str, &[u8])] = &[("g.txt", b"x y&z\n")];
    for arg in [
        "'q=a b&c'",   // name=content
        "'=a b'",      // =content: the encoded content, no name and no =
        "'a b/c'",     // content alone
        "@g.txt",      // file contents, encoded
        "n@g.txt",     // name=<encoded file contents>
        "'é=ü'",
    ] {
        let p = compare(&server, &format!("-s --data-urlencode {arg} \"$URL/echo\""), files).await;
        assert_eq!(p.ours.code, 0, "{arg}: {}", p.ours.err);
        p.assert_same(arg);
    }
    // Literal bytes, so a box without curl still checks the encoding.
    let p = compare(&server, "-s --data-urlencode 'q=a b&c' --data-urlencode n@g.txt \"$URL/echo\"", files).await;
    assert_eq!(p.body(), b"q=a+b%26c&n=x+y%26z%0A");
}

#[tokio::test(flavor = "multi_thread")]
async fn data_urlencode_of_a_missing_file_exits_26() {
    let server = recorder();
    let p = compare(&server, "-s --data-urlencode n@nope \"$URL/echo\"", &[]).await;
    assert_eq!(p.ours.code, 26, "{}", p.ours.err);
    assert!(p.ours_req.is_empty());
}

#[tokio::test(flavor = "multi_thread")]
async fn a_body_file_is_read_from_the_shell_filesystem_never_the_host() {
    // /etc/passwd exists on the machine running this test and not in the
    // kernel's in-memory filesystem. Real curl reads the host file; this tool
    // must not, whatever the path looks like.
    let server = recorder();
    assert!(std::path::Path::new("/etc/passwd").exists(), "control: the host file exists");
    let p = compare(&server, "-s --data-binary @/etc/passwd \"$URL/echo\"", &[]).await;
    assert_eq!(p.ours.code, 26, "{}", p.ours.err);
    assert!(p.ours_req.is_empty(), "host bytes must not reach the wire");
    if let Some(t) = &p.theirs {
        assert_eq!(t.code, 0, "control: real curl does read the host file");
    }
}

// ── 2. -G / --get ───────────────────────────────────────────────────────

#[tokio::test(flavor = "multi_thread")]
async fn get_moves_the_data_to_the_query_string() {
    let server = recorder();
    let files: &[(&str, &[u8])] = &[("f.txt", CRLF)];
    for (args, want) in [
        ("-G -d a=1 -d b=2 \"$URL/echo\"", "/echo?a=1&b=2"),
        ("--get -d a=1 \"$URL/echo\"", "/echo?a=1"),
        // The survey's shape: urlencode puts '+' for the space.
        ("-G --data-urlencode 'q=hello world' --data-urlencode n=5 \"$URL/echo\"", "/echo?q=hello+world&n=5"),
        ("-G -d a=1 \"$URL/echo?z=1\"", "/echo?z=1&a=1"),
        ("-G -d @f.txt \"$URL/echo\"", "/echo?abc"),
        ("-G --data-binary k=v -d @f.txt \"$URL/echo\"", "/echo?k=v&abc"),
    ] {
        let p = compare(&server, &format!("-s {args}"), files).await;
        assert_eq!(p.ours.code, 0, "{args}: {}", p.ours.err);
        let got = Wire::of(&p.ours_req[0]);
        assert_eq!(got.method, "GET", "{args}");
        assert_eq!(got.path, want, "{args}");
        assert!(got.body.is_empty(), "{args}: no body");
        assert_eq!(got.content_type, None, "{args}: no form Content-Type without a body");
        p.assert_same(args);
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn get_keeps_an_explicit_method_and_head() {
    let server = recorder();
    for (args, method) in [
        ("-G -d a=1 -X POST \"$URL/echo\"", "POST"),
        ("-G -d a=1 -I \"$URL/echo\"", "HEAD"),
    ] {
        let p = compare(&server, &format!("-s {args}"), &[]).await;
        assert_eq!(p.ours.code, 0, "{args}: {}", p.ours.err);
        let got = Wire::of(&p.ours_req[0]);
        assert_eq!((got.method.as_str(), got.path.as_str()), (method, "/echo?a=1"), "{args}");
        p.assert_same_wire(args);
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn get_without_data_is_a_plain_get() {
    let server = recorder();
    let p = compare(&server, "-s -G \"$URL/echo\"", &[]).await;
    assert_eq!(Wire::of(&p.ours_req[0]).path, "/echo");
    p.assert_same("-G alone");
}

#[tokio::test(flavor = "multi_thread")]
async fn get_refuses_data_a_url_cannot_carry_and_sends_nothing() {
    // curl exits 3 (bad URL) here; this build exits 2 before any request and
    // names --data-urlencode, which is the fix.
    let server = recorder();
    let p = compare(&server, "-s -G -d 'a=b c' \"$URL/echo\"", &[]).await;
    assert_eq!(p.ours.code, 2, "{}", p.ours.err);
    assert!(p.ours.err.contains("--data-urlencode"), "{}", p.ours.err);
    assert!(p.ours_req.is_empty());
    if let Some(t) = &p.theirs {
        assert_ne!(t.code, 0, "control: real curl refuses it too");
        assert!(p.theirs_req.is_empty());
    }
}

// ── 3. -F / --form ──────────────────────────────────────────────────────

/// One multipart part as a server sees it.
#[derive(Debug, PartialEq, Eq)]
struct Part {
    disposition: String,
    content_type: Option<String>,
    data: Vec<u8>,
}

/// Split a multipart body on the boundary named in its `Content-Type`. The
/// boundary string itself differs between curl and this tool, and is allowed
/// to; the parts must not.
fn parts(req: &CapturedRequest) -> Vec<Part> {
    let ct = req.header("Content-Type").expect("a multipart Content-Type");
    assert!(ct.starts_with("multipart/form-data; boundary="), "Content-Type: {ct}");
    let boundary = ct.split("boundary=").nth(1).unwrap();
    let delim = format!("--{boundary}").into_bytes();
    let body = &req.body;
    assert!(body.starts_with(&delim), "body opens with the boundary");
    assert!(body.ends_with(&[&delim[..], b"--\r\n"].concat()), "body closes with --boundary--");
    let mut out = Vec::new();
    let mut rest = &body[delim.len()..];
    loop {
        if rest.starts_with(b"--\r\n") {
            assert_eq!(rest, b"--\r\n", "nothing after the closing boundary");
            break;
        }
        rest = rest.strip_prefix(b"\r\n").expect("CRLF after a boundary");
        let end = rest.windows(4).position(|w| w == b"\r\n\r\n").expect("blank line ends headers");
        let head = String::from_utf8(rest[..end].to_vec()).expect("headers are text");
        let after = &rest[end + 4..];
        let close = [&b"\r\n"[..], &delim[..]].concat();
        let stop = after.windows(close.len()).position(|w| w == close.as_slice()).expect("part ends at a boundary");
        let mut disposition = String::new();
        let mut content_type = None;
        for line in head.split("\r\n") {
            if let Some(v) = line.strip_prefix("Content-Disposition: ") {
                disposition = v.to_string();
            } else if let Some(v) = line.strip_prefix("Content-Type: ") {
                content_type = Some(v.to_string());
            } else {
                panic!("unexpected part header {line:?}");
            }
        }
        out.push(Part { disposition, content_type, data: after[..stop].to_vec() });
        rest = &after[stop + close.len()..];
    }
    out
}

/// Both sides sent the same parts, and the same Content-Length as the body.
fn assert_same_parts(p: &Pair, what: &str) {
    assert_eq!(p.ours.code, 0, "{what}: {}", p.ours.err);
    let ours = &p.ours_req[0];
    assert_eq!(ours.method, p.theirs_req.first().map_or("POST", |r| r.method.as_str()), "{what}");
    assert_eq!(
        ours.header("Content-Length").map(str::to_string),
        Some(ours.body.len().to_string()),
        "{what}: Content-Length is the body's length"
    );
    if p.theirs.is_some() {
        assert_eq!(parts(ours), parts(&p.theirs_req[0]), "{what}: parts differ from real curl's");
    }
}

const FORM_FILES: &[(&str, &[u8])] = &[
    ("up.txt", b"hello\n"),
    ("up.json", b"{\"a\":1}"),
    ("up.html", b"<p>"),
    ("up.PNG", b"\x89PNG\r\n\x1a\n\0\xff"),
    ("noext", b"plain"),
    ("up.csv", b"a,b\n"),
    ("sub/deep.txt", b"deep"),
];

#[tokio::test(flavor = "multi_thread")]
async fn form_parts_match_curl_part_for_part() {
    let server = recorder();
    for args in [
        "-F a=1",
        "-F 'a='",
        "-F f=@up.txt",
        "-F f=@up.json -F g=@up.html",
        "-F f=@up.PNG",                          // binary bytes, extension case-folded
        "-F f=@noext",                           // no extension: application/octet-stream
        "-F f=@up.csv",                          // not in curl's table
        "-F f=@sub/deep.txt",                    // filename is the basename
        "-F 'f=@up.json;type=text/x-foo'",
        "-F 'f=@up.txt;filename=zz.dat'",        // the shown name picks the type
        "-F 'f=@noext;filename=zz.txt'",
        "-F 'f=@up.json;type=text/x-foo;filename=zz.dat'",
        "-F 'f=@up.txt;type=text/plain; charset=x'",
        "-F 'f=<up.txt'",                        // contents as the value
        "-F 'f=<up.json;type=application/json'",
        "-F 'f=<up.txt;filename=ignored.txt'",
        "-F 'f=x;type=text/plain'",              // type on plain text
        "-F 'f=x;filename=n.txt'",               // filename on plain text
        "-F 'f=x;filename=n.bin'",
        "-F 'o=\"a;b\"'",                        // quotes protect a ';'
        "-F 'o=\"x\\\"y\\\\z\"'",                // \" and \\ inside quotes
        "-F 'p=@\"up.txt\";filename=\"q r.txt\"'",
        "-F 'p=@up.txt;filename=a\"b'",          // " becomes %22
        "-F 'n\"ame=v'",
        "-F a=1 -F f=@up.txt -F b=2",            // order is kept
        "-X PUT -F a=1",
    ] {
        let p = compare(&server, &format!("-s {args} \"$URL/echo\""), FORM_FILES).await;
        assert_same_parts(&p, args);
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn form_literal_parts_so_a_box_without_curl_still_checks_bytes() {
    let server = recorder();
    let p = compare(&server, "-s -F a=1 -F 'f=@up.txt' -F 'h=<up.txt' \"$URL/echo\"", FORM_FILES).await;
    let got = parts(&p.ours_req[0]);
    assert_eq!(got.len(), 3);
    assert_eq!(got[0].disposition, "form-data; name=\"a\"");
    assert_eq!((got[0].content_type.as_deref(), got[0].data.as_slice()), (None, &b"1"[..]));
    assert_eq!(got[1].disposition, "form-data; name=\"f\"; filename=\"up.txt\"");
    assert_eq!((got[1].content_type.as_deref(), got[1].data.as_slice()), (Some("text/plain"), &b"hello\n"[..]));
    assert_eq!(got[2].disposition, "form-data; name=\"h\"");
    assert_eq!((got[2].content_type.as_deref(), got[2].data.as_slice()), (None, &b"hello\n"[..]));
    assert_eq!(Wire::of(&p.ours_req[0]).method, "POST");
}

#[tokio::test(flavor = "multi_thread")]
async fn the_boundary_never_occurs_inside_a_part() {
    // A file made of boundary-shaped lines cannot split the body, because the
    // boundary is chosen so it appears in no part's data.
    let server = recorder();
    let hostile = [b"--------------------------".repeat(4), b"\r\n--x\r\n".to_vec()].concat();
    let p = compare(&server, "-s -F f=@hostile \"$URL/echo\"", &[("hostile", &hostile)]).await;
    assert_eq!(p.ours.code, 0, "{}", p.ours.err);
    let got = parts(&p.ours_req[0]);
    assert_eq!(got.len(), 1);
    assert_eq!(got[0].data, hostile);
}

#[tokio::test(flavor = "multi_thread")]
async fn each_request_gets_its_own_boundary() {
    let server = recorder();
    let fx = Fixture::new(loopback_config(), &[]).await;
    fx.run(&server.url(""), "curl -s -F a=1 \"$URL/echo\"\ncurl -s -F a=1 \"$URL/echo\"").await;
    let reqs = server.take_requests();
    let b = |r: &CapturedRequest| r.header("Content-Type").unwrap().to_string();
    assert_eq!(reqs.len(), 2);
    assert_ne!(b(&reqs[0]), b(&reqs[1]), "a fixed boundary would be guessable by the data");
}

#[tokio::test(flavor = "multi_thread")]
async fn form_file_errors_send_nothing_and_exit_like_curl() {
    let server = recorder();
    for (args, code) in [
        ("-F f=@nope", 26),
        ("-F 'f=<nope'", 26),
        ("-F junk", 2),                   // no '='
        ("-F f=@up.txt -d a=1", 2),       // -F and -d together
        ("-F a=1 -I", 2),
    ] {
        let p = compare(&server, &format!("-s {args} \"$URL/echo\""), FORM_FILES).await;
        assert_eq!(p.ours.code, code as i64, "{args}: {}", p.ours.err);
        assert!(p.ours_req.is_empty(), "{args}: nothing is sent");
        if let Some(t) = &p.theirs {
            assert_eq!(t.code, code, "{args}: control, real curl");
        }
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn a_directory_is_not_a_form_file() {
    // Real curl exits 0 here and sends something; a directory has no bytes to
    // send, and this build says so.
    let server = recorder();
    let p = compare(&server, "-s -F f=@sub \"$URL/echo\"", FORM_FILES).await;
    assert_eq!(p.ours.code, 26, "{}", p.ours.err);
    assert!(p.ours_req.is_empty());
}

#[tokio::test(flavor = "multi_thread")]
async fn form_features_this_build_does_not_carry_are_refused_not_dropped() {
    // curl accepts these; each would be a quiet difference if dropped.
    let server = recorder();
    for (args, needle) in [
        ("-F 'f=@up.txt,up.json'", "one file"),          // multi-file part
        ("-F 'f=@up.txt;headers=X-A: b'", "headers"),
        ("-F 'f=x;foo=bar'", "foo"),                     // curl ignores unknown params
        ("-F '=v'", "name"),                             // curl sends a nameless part
        ("-F 'f=@up.txt;type='", "type"),
        ("-F a=1 -G", "-G"),                             // curl ignores -G here
        ("-F 'f=@-'", "stdin"),
        ("-F 'f=<-'", "stdin"),
    ] {
        let p = compare(&server, &format!("-s {args} \"$URL/echo\""), FORM_FILES).await;
        assert_eq!(p.ours.code, 2, "{args}: {}", p.ours.err);
        assert!(p.ours.err.contains(needle), "{args}: names the problem ({needle}): {}", p.ours.err);
        assert!(p.ours.err.contains("no request was sent"), "{args}: {}", p.ours.err);
        assert!(p.ours_req.is_empty(), "{args}");
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn form_files_count_against_the_byte_limit() {
    let server = recorder();
    let big = vec![b'z'; 2000];
    let config = loopback_config().with_limits(Limits { max_response_bytes: 1024, ..Limits::default() });
    let p = compare_with(&server, "-s -F f=@big \"$URL/echo\"", &[("big", &big)], config).await;
    assert_ne!(p.ours.code, 0);
    assert!(p.ours.err.contains("1024"), "{}", p.ours.err);
    assert!(p.ours_req.is_empty());
    // Control: a roomier budget sends it.
    let config = loopback_config().with_limits(Limits { max_response_bytes: 8192, ..Limits::default() });
    let p = compare_with(&server, "-s -F f=@big \"$URL/echo\"", &[("big", &big)], config).await;
    assert_eq!(p.ours.code, 0, "{}", p.ours.err);
    assert_eq!(parts(&p.ours_req[0])[0].data, big);
}

// ── 4. -O / --remote-name ───────────────────────────────────────────────

/// A server with the paths the `-O` cases fetch. Routes match the raw
/// request target, query string included.
fn files_server() -> TcpGuard {
    let mut b = Server::builder();
    for target in [
        "/a/b.txt",
        "/a/b.txt?x=1",
        "/a/c%20d.txt",
        "/a/",
        "/a/%2e%2e%2fz",
        "/a/deep/f.bin",
        "/x/..",
        "/",
    ] {
        b = b.route(target, Response::text(200, format!("body of {target}")));
    }
    b.route("/gone", Response::json(404, "{\"error\":\"gone\"}\n"))
        .route("/a/gone.txt", Response::json(404, "{\"error\":\"gone\"}\n"))
        .start_tcp()
}

#[tokio::test(flavor = "multi_thread")]
async fn remote_name_saves_under_the_last_path_segment() {
    let server = files_server();
    for (args, name) in [
        ("-s -O \"$URL/a/b.txt\"", "b.txt"),
        ("-sO \"$URL/a/b.txt\"", "b.txt"),
        ("--remote-name \"$URL/a/b.txt\"", "b.txt"),
        ("-s -O \"$URL/a/b.txt?x=1\"", "b.txt"),           // the query is not part of the name
        ("-s -O \"$URL/a/c%20d.txt\"", "c%20d.txt"),       // curl 8.23 does not decode it
        ("-s -O \"$URL/a/deep/f.bin\"", "f.bin"),
        ("-s -O \"$URL/a/\"", "a"),                        // a trailing slash: the segment before it
    ] {
        let p = compare(&server, args, &[]).await;
        assert_eq!(p.ours.code, 0, "{args}: {}", p.ours.err);
        assert_eq!(p.ours.text_out(), "", "{args}: -O prints nothing on stdout");
        let ours = p.fx.file(name).await.unwrap_or_else(|| panic!("{args}: {name} was not written"));
        assert!(ours.starts_with(b"body of "), "{args}: {ours:?}");
        if let Some(t) = &p.theirs {
            assert_eq!(t.code, 0);
            assert_eq!(t.file(name).as_deref(), Some(ours.as_slice()), "{args}: file content");
        }
        p.assert_same(args);
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn remote_name_with_no_name_in_the_url_sends_nothing() {
    // curl falls back to the name `curl_response`. Amy's ruling is to refuse.
    let server = files_server();
    for target in ["/", "", "/x/.."] {
        let p = compare(&server, &format!("-s -O \"$URL{target}\""), &[]).await;
        assert_eq!(p.ours.code, 2, "{target:?}: {}", p.ours.err);
        assert!(p.ours.err.contains("-o <file>"), "name the way forward: {}", p.ours.err);
        assert!(p.ours_req.is_empty(), "{target:?}: no request");
        assert!(p.fx.file("curl_response").await.is_none());
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn remote_name_cannot_leave_the_working_directory() {
    // However the URL is spelled, the name is one path segment.
    let server = files_server();
    let p = compare(&server, "-s -O \"$URL/a/%2e%2e%2fz\"", &[]).await;
    assert_eq!(p.ours.code, 0, "{}", p.ours.err);
    assert!(p.fx.file("%2e%2e%2fz").await.is_some());
    assert!(p.fx.file("../z").await.is_none());
    assert!(p.fx.file("z").await.is_none());
}

#[tokio::test(flavor = "multi_thread")]
async fn output_dir_prefixes_both_o_and_dash_o() {
    let server = files_server();
    let files: &[(&str, &[u8])] = &[("d/keep", b"1")];
    for (args, path) in [
        ("-s -O --output-dir d \"$URL/a/b.txt\"", "d/b.txt"),
        ("-s -o w.txt --output-dir d \"$URL/a/b.txt\"", "d/w.txt"),
    ] {
        let p = compare(&server, args, files).await;
        assert_eq!(p.ours.code, 0, "{args}: {}", p.ours.err);
        assert!(p.fx.file(path).await.is_some(), "{args}: {path}");
        if let Some(t) = &p.theirs {
            assert!(t.file(path).is_some(), "control: curl wrote {path}");
        }
        assert!(p.fx.file("b.txt").await.is_none(), "{args}: nothing lands in the cwd");
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn a_missing_output_dir_fails_with_exit_23_and_names_the_path() {
    let server = files_server();
    let p = compare(&server, "-s -O --output-dir nodir \"$URL/a/b.txt\"", &[]).await;
    assert_eq!(p.ours.code, 23, "{}", p.ours.err);
    assert!(p.ours.err.contains("nodir/b.txt"), "{}", p.ours.err);
    if let Some(t) = &p.theirs {
        assert_eq!(t.code, 23, "control: real curl");
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn output_dir_without_an_output_flag_is_refused() {
    let server = files_server();
    let p = compare(&server, "-s --output-dir d \"$URL/a/b.txt\"", &[("d/x", b"")]).await;
    assert_eq!(p.ours.code, 2, "{}", p.ours.err);
    assert!(p.ours.err.contains("--output-dir"), "{}", p.ours.err);
    assert!(p.ours_req.is_empty());
}

#[tokio::test(flavor = "multi_thread")]
async fn remote_name_overwrites_an_existing_file_and_honors_include_and_fail() {
    let server = files_server();
    let p = compare(&server, "-s -O \"$URL/a/b.txt\"", &[("b.txt", b"OLD OLD OLD OLD OLD OLD")]).await;
    assert_eq!(p.fx.file("b.txt").await.as_deref(), Some(&b"body of /a/b.txt"[..]));

    let p = compare(&server, "-s -i -O \"$URL/a/b.txt\"", &[]).await;
    let got = String::from_utf8(p.fx.file("b.txt").await.unwrap()).unwrap();
    assert!(got.starts_with("HTTP/1.1 200 OK\r\n") && got.ends_with("body of /a/b.txt"), "{got:?}");

    // -f on a 404 writes nothing, as with -o.
    let p = compare(&server, "-s -f -O \"$URL/a/gone.txt\"", &[]).await;
    assert_eq!(p.ours.code, 22, "{}", p.ours.err);
    assert!(p.fx.file("gone.txt").await.is_none());
    if let Some(t) = &p.theirs {
        assert!(t.file("gone.txt").is_none(), "control: curl wrote nothing either");
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn remote_name_and_output_are_one_choice_and_j_stays_refused() {
    let server = files_server();
    let fx = Fixture::new(loopback_config(), &[]).await;
    let base = server.url("");
    for (args, needle) in [
        ("-O -o x \"$URL/a/b.txt\"", "-o"),
        ("-O -J \"$URL/a/b.txt\"", "-J"),
        ("-O --remote-header-name \"$URL/a/b.txt\"", "--remote-header-name"),
    ] {
        let r = fx.run(&base, &format!("curl -s {args}")).await;
        // `-J` is a refused flag (exit 1 like the others); `-O -o` is a usage error.
        assert!(r.code == 1 || r.code == 2, "{args}: {}", r.err);
        assert!(r.err.contains(needle), "{args}: {}", r.err);
        assert!(r.err.contains("no request was sent"), "{args}: {}", r.err);
    }
    assert!(server.requests().is_empty());
}
