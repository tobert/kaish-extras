//! Compatibility with real curl, checked through a real kaish kernel.
//!
//! These cases come from docs/design/curl-survey-2026-11.md: spellings that
//! models type and that used to differ from real curl. Each one runs a shell
//! line the way an embedder's agent does, so pipes, `$(...)`, and redirects
//! are the kernel's own. Where real curl is on the PATH, a case also runs it
//! on the same loopback server and compares (`oracle`).

#[path = "support.rs"]
mod support;

use std::path::PathBuf;
use std::process::Command;
use std::sync::Arc;

use kaish_kernel::vfs::{MemoryFs, VfsRouter};
use kaish_kernel::{Kernel, KernelConfig, LocalBackend};
use kaish_tool_api::KernelBackend;
use kaish_types::ExecResult;

use support::{loopback_config, Response, Server, TcpGuard};

/// A kernel with an in-memory filesystem (cwd `/work`) and curl registered
/// with loopback egress allowed.
async fn kernel() -> Kernel {
    let mut vfs = VfsRouter::new();
    vfs.mount("/", MemoryFs::new());
    let backend: Arc<dyn KernelBackend> = Arc::new(LocalBackend::new(Arc::new(vfs)));
    let mut cfg = KernelConfig::transient();
    cfg.cwd = PathBuf::from("/work");
    let curl = kaish_tools_curl::tool(loopback_config());
    let k = Kernel::with_backend(backend, cfg, |_| {}, |tools| tools.register(curl))
        .expect("kernel assembles");
    let r = k.execute("mkdir -p /work /tmp").await.expect("mkdir");
    assert_eq!(r.code, 0, "mkdir failed: {}", r.err);
    k
}

async fn run(script: &str) -> ExecResult {
    kernel()
        .await
        .execute(script)
        .await
        .unwrap_or_else(|e| panic!("kernel failed to execute {script:?}: {e}"))
}

/// Run `script` with `$URL` standing for the server's base and return the
/// result. The script is quoted by the caller.
async fn run_at(server: &TcpGuard, script: &str) -> ExecResult {
    let base = server.url("");
    run(&format!("URL='{}'\n{script}", base.trim_end_matches('/'))).await
}

/// Real curl on the same server, via bash, for stdout/exit comparison.
/// `None` when no curl is installed, so the case still runs its own
/// assertions.
fn oracle(server: &TcpGuard, args: &str) -> Option<(String, i32)> {
    let base = server.url("");
    let out = Command::new("bash")
        .arg("-c")
        .arg(format!("URL='{}'; curl {args}", base.trim_end_matches('/')))
        .current_dir(std::env::temp_dir())
        .output()
        .ok()?;
    let code = out.status.code()?;
    Some((String::from_utf8_lossy(&out.stdout).into_owned(), code))
}

fn api() -> TcpGuard {
    Server::builder()
        .route("/json", Response::json(200, "{\"name\":\"widget\",\"qty\":3}\n"))
        .route("/text", Response::text(200, "plain body\n"))
        .route("/status/404", Response::json(404, "{\"error\":\"status 404\"}\n"))
        .route(
            "/teapot",
            Response::text(418, "short and stout\n").reason("I'm a teapot"),
        )
        .route("/go", Response::redirect(302, "/text"))
        .start_tcp()
}

// ── SW-1 / X6: the body is the text, `.data` belongs to --json ──────────

#[tokio::test(flavor = "multi_thread")]
async fn a_pipe_into_jq_reads_the_body_not_the_envelope() {
    let server = api();
    let r = run_at(&server, "curl -s \"$URL/json\" | jq -r .name").await;
    assert_eq!(r.code, 0, "{}", r.err);
    assert_eq!(r.text_out(), "widget\n");
    // Without -s too: the survey saw both spellings.
    let r = run_at(&server, "curl \"$URL/json\" | jq -r .name").await;
    assert_eq!(r.text_out(), "widget\n");

    // Negative control: --json is the envelope, so `.status` is the HTTP
    // status and `.body.name` the parsed body. If the first assertion passed
    // because jq saw nothing at all, these fail.
    let r = run_at(&server, "curl --json \"$URL/json\" | jq -r .status").await;
    assert_eq!(r.text_out(), "200\n", "stderr: {}", r.err);
    let r = run_at(&server, "curl --json \"$URL/json\" | jq -r .body.name").await;
    assert_eq!(r.text_out(), "widget\n", "stderr: {}", r.err);
}

#[tokio::test(flavor = "multi_thread")]
async fn command_substitution_binds_the_text() {
    let server = api();
    let r = run_at(&server, "x=$(curl -s \"$URL/text\")\necho \"[$x]\"").await;
    assert_eq!(r.code, 0, "{}", r.err);
    assert_eq!(r.text_out(), "[plain body]\n");

    // Control: the same substitution of a JSON body is the body text, so jq
    // can read it, and it is not the envelope.
    let r = run_at(&server, "x=$(curl -s \"$URL/json\")\necho \"$x\" | jq -r .name").await;
    assert_eq!(r.text_out(), "widget\n", "stderr: {}", r.err);
}

#[tokio::test(flavor = "multi_thread")]
async fn only_json_output_carries_structured_data() {
    // `.data` set on a plain response is the SW-1 defect. Checked on the
    // result itself, because a pipe can look right for other reasons.
    let server = api();
    let plain = run_at(&server, "curl \"$URL/json\"").await;
    assert!(plain.data.is_none(), "plain curl must not set .data: {:?}", plain.data);
    let json = run_at(&server, "curl --json \"$URL/json\"").await;
    assert!(json.data.is_some(), "--json must set .data (the text is that envelope)");
}

#[tokio::test(flavor = "multi_thread")]
async fn command_substitution_of_json_output_binds_the_printed_text() {
    // The one case where `.data` is set. Declaring
    // `with_typed_substitution()` on curl's schema would bind the structured
    // value here instead of the text curl printed; this is what fails then.
    let server = api();
    let direct = run_at(&server, "curl --json \"$URL/json\"").await;
    let bound = run_at(&server, "x=$(curl --json \"$URL/json\")\necho \"$x\"").await;
    assert_eq!(bound.code, 0, "{}", bound.err);
    assert_eq!(
        bound.text_out().trim_end(),
        direct.text_out().trim_end(),
        "$(curl --json …) must bind exactly what `curl --json …` printed"
    );
    assert!(!bound.text_out().trim().is_empty(), "control: --json printed something");
}

#[tokio::test(flavor = "multi_thread")]
async fn json_output_carries_a_binary_body_as_base64() {
    // The binary refusal on stdout tells the agent to use --json, so --json
    // has to work for it.
    let server = Server::builder()
        .route(
            "/blob",
            Response::new(200)
                .header("Content-Type", "application/octet-stream")
                .body(vec![0x00u8, 0xff, 0xfe]),
        )
        .start_tcp();
    let r = run_at(&server, "curl --json \"$URL/blob\" | jq -r .body_format").await;
    assert_eq!(r.text_out(), "base64\n", "stderr: {}", r.err);
}

// ── SW-3: `-o` prints nothing on stdout, as curl does ───────────────────

#[tokio::test(flavor = "multi_thread")]
async fn output_file_leaves_stdout_empty_and_writes_the_body() {
    let server = api();
    let r = run_at(&server, "curl -o /tmp/out.txt \"$URL/text\"\ncat /tmp/out.txt").await;
    assert_eq!(r.code, 0, "{}", r.err);
    // Only the `cat` prints: the file holds the body and curl said nothing.
    assert_eq!(r.text_out(), "plain body\n");

    // The survey case: a value computed after `-o` must not be prefixed.
    let r = run_at(&server, "curl -o /tmp/out.txt \"$URL/text\"\necho \"after\"").await;
    assert_eq!(r.text_out(), "after\n");
    let r = run_at(&server, "x=$(curl -o /tmp/out.txt \"$URL/text\")\necho \"[$x]\"").await;
    assert_eq!(r.text_out(), "[]\n");

    if let Some((stdout, code)) = oracle(&server, "-o /dev/null \"$URL/text\"") {
        assert_eq!((stdout.as_str(), code), ("", 0), "real curl is silent on stdout with -o");
    }
}

// ── SW-2: the status line carries the reason phrase ─────────────────────

fn first_line(s: &str) -> &str {
    s.lines().next().unwrap_or("").trim_end_matches('\r')
}

#[tokio::test(flavor = "multi_thread")]
async fn include_prints_the_status_line_with_its_reason_phrase() {
    let server = api();
    let r = run_at(&server, "curl -i \"$URL/status/404\"").await;
    assert_eq!(first_line(&r.text_out()), "HTTP/1.1 404 Not Found");
    // The CR stays: header lines end in CRLF, as on the wire and in curl.
    assert!(r.text_out().starts_with("HTTP/1.1 404 Not Found\r\n"));

    let r = run_at(&server, "curl -I \"$URL/text\"").await;
    assert_eq!(first_line(&r.text_out()), "HTTP/1.1 200 OK");

    // The survey's pipe: no stray CR on the code after splitting on a space.
    let r = run_at(
        &server,
        "curl -i \"$URL/status/404\" | head -n 1 | awk '{print $2}'",
    )
    .await;
    assert_eq!(r.text_out().trim(), "404", "stderr: {}", r.err);

    if let Some((stdout, _)) = oracle(&server, "-i -s \"$URL/status/404\"") {
        assert_eq!(first_line(&stdout), "HTTP/1.1 404 Not Found");
    }
    if let Some((stdout, _)) = oracle(&server, "-i -s \"$URL/teapot\"") {
        let ours = run_at(&server, "curl -i \"$URL/teapot\"").await;
        assert_eq!(first_line(&ours.text_out()), first_line(&stdout));
    }
}

// ── Body from a file: the refusals name the form that works ─────────────

fn echo_server() -> TcpGuard {
    Server::builder()
        .route_fn("/echo", |req| {
            Response::text(200, format!("{}|{}", req.method, req.body_str()))
        })
        .start_tcp()
}

#[tokio::test(flavor = "multi_thread")]
async fn data_binary_dash_is_refused_with_exit_2_and_sends_nothing() {
    let server = echo_server();
    let r = run_at(
        &server,
        "echo '{\"a\":1}' | curl --data-binary - \"$URL/echo\"",
    )
    .await;
    assert_eq!(r.code, 2, "stderr: {}", r.err);
    assert!(
        r.err.contains("-d \"$(cat"),
        "the refusal must name the working form: {}",
        r.err
    );
    assert!(server.requests().is_empty(), "a refused flag must not reach the server");

    // Control: the form the message names works and sends the file's text.
    let r = run_at(
        &server,
        "echo '{\"a\":1}' > /work/p.json\ncurl -s -d \"$(cat /work/p.json)\" \"$URL/echo\"",
    )
    .await;
    assert_eq!(r.code, 0, "stderr: {}", r.err);
    assert_eq!(r.text_out(), "POST|{\"a\":1}");
}

#[tokio::test(flavor = "multi_thread")]
async fn the_at_path_refusal_names_the_working_form_not_data_binary_dash() {
    let server = echo_server();
    for flag in ["-d", "--data-binary"] {
        let r = run_at(&server, &format!("curl {flag} @/work/p.json \"$URL/echo\"")).await;
        assert_ne!(r.code, 0);
        assert!(r.err.contains("-d \"$(cat"), "{flag}: {}", r.err);
        assert!(
            !r.err.contains("data-binary -"),
            "{flag}: must not recommend a form that sends the wrong body: {}",
            r.err
        );
    }
    assert!(server.requests().is_empty());
}

// ── -w / --write-out ────────────────────────────────────────────────────

/// Replace the `time_total` value, which differs per run, so the rest of a
/// `-w` line can be compared with real curl's byte for byte.
fn is_seconds(s: &str) -> bool {
    s.split_once('.').is_some_and(|(whole, frac)| {
        !whole.is_empty()
            && whole.bytes().all(|b| b.is_ascii_digit())
            && frac.len() == 6
            && frac.bytes().all(|b| b.is_ascii_digit())
    })
}

#[tokio::test(flavor = "multi_thread")]
async fn write_out_variables_match_real_curl() {
    let server = api();
    let fmt = "%{http_code}|%{response_code}|%{size_download}|%{url_effective}|%{content_type}|[%{redirect_url}]|%{num_redirects}\\n";
    for (flags, path) in [
        ("", "/json"),
        ("", "/status/404"),
        ("", "/go"),
        ("-L", "/go"),
        ("-I", "/text"),
    ] {
        let ours = run_at(
            &server,
            &format!("curl -s {flags} -o /dev/null -w '{fmt}' \"$URL{path}\""),
        )
        .await;
        assert_eq!(ours.code, 0, "{flags} {path}: {}", ours.err);
        let theirs = oracle(&server, &format!("-s {flags} -o /dev/null -w '{fmt}' \"$URL{path}\""));
        if let Some((stdout, code)) = theirs {
            assert_eq!(code, 0);
            assert_eq!(ours.text_out(), stdout, "{flags} {path}");
        }
        assert!(ours.text_out().ends_with('\n'), "{flags} {path}: `\\n` must expand");
    }
    // Control: the line really carries values, not an empty format.
    let r = run_at(&server, &format!("curl -s -o /dev/null -w '{fmt}' \"$URL/json\"")).await;
    assert!(r.text_out().starts_with("200|200|"), "{}", r.text_out());
    assert!(r.text_out().contains("|application/json|"), "{}", r.text_out());
}

#[tokio::test(flavor = "multi_thread")]
async fn write_out_time_total_is_seconds_with_six_decimals() {
    let server = api();
    let r = run_at(&server, "curl -s -o /dev/null -w '%{time_total}' \"$URL/json\"").await;
    assert!(is_seconds(r.text_out().trim()), "got {:?}", r.text_out());
    if let Some((stdout, _)) = oracle(&server, "-s -o /dev/null -w '%{time_total}' \"$URL/json\"") {
        assert!(is_seconds(stdout.trim()), "the oracle's own shape: {stdout:?}");
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn write_out_follows_the_body_on_stdout() {
    let server = api();
    let r = run_at(&server, "curl -s -w '[%{http_code}]' \"$URL/text\"").await;
    assert_eq!(r.text_out(), "plain body\n[200]");
    // Escapes, as curl: %% is one %, \t a tab; `\\` stays two backslashes.
    let r = run_at(&server, "curl -s -o /dev/null -w '100%%\\t\\\\' \"$URL/text\"").await;
    assert_eq!(r.text_out(), "100%\t\\\\");
    if let Some((stdout, _)) = oracle(&server, "-s -o /dev/null -w '100%%\\t\\\\' \"$URL/text\"") {
        assert_eq!(stdout, "100%\t\\\\");
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn write_out_with_dev_null_discards_the_body_and_creates_no_file() {
    // /dev/null is a device in the kernel's filesystem, so the write is
    // discarded there. The listing is compared whole: a stray file under
    // /dev, or a `null` replaced by one holding the body, would change it.
    let server = api();
    let before = run("ls /dev").await;
    let r = run_at(
        &server,
        "curl -s -o /dev/null -w '%{http_code}' \"$URL/text\"\necho '|'\ncat /dev/null\nls /dev",
    )
    .await;
    assert_eq!(r.code, 0, "{}", r.err);
    assert_eq!(r.text_out(), format!("200|\n{}", before.text_out()), "stderr: {}", r.err);
    assert!(before.text_out().contains("null"), "control: the device is listed");
}

#[tokio::test(flavor = "multi_thread")]
async fn write_out_refuses_an_unsupported_variable_naming_the_supported_set() {
    let server = api();
    let r = run_at(&server, "curl -s -w '%{speed_download}' \"$URL/text\"").await;
    assert_eq!(r.code, 2, "stderr: {}", r.err);
    for name in [
        "speed_download",
        "http_code",
        "response_code",
        "time_total",
        "size_download",
        "url_effective",
        "content_type",
        "redirect_url",
        "num_redirects",
    ] {
        assert!(r.err.contains(name), "the refusal must name {name}: {}", r.err);
    }
    assert!(server.requests().is_empty(), "refusal comes before the request");
}

#[tokio::test(flavor = "multi_thread")]
async fn write_out_is_refused_with_json_output() {
    let server = api();
    let r = run_at(&server, "curl --json -w '%{http_code}' \"$URL/text\"").await;
    assert_eq!(r.code, 2, "stderr: {}", r.err);
    assert!(r.err.contains("--json"), "{}", r.err);
}

#[tokio::test(flavor = "multi_thread")]
async fn write_out_still_prints_when_fail_stops_the_body() {
    let server = api();
    let ours = run_at(&server, "curl -s -f -o /dev/null -w '%{http_code}' \"$URL/status/404\"").await;
    assert_eq!(ours.code, 22, "{}", ours.err);
    assert_eq!(ours.text_out(), "404");
    if let Some((stdout, code)) =
        oracle(&server, "-s -f -o /dev/null -w '%{http_code}' \"$URL/status/404\"")
    {
        assert_eq!((stdout.as_str(), code), ("404", 22));
    }
}

// ── --fail-with-body ────────────────────────────────────────────────────

#[tokio::test(flavor = "multi_thread")]
async fn fail_with_body_exits_22_and_prints_the_body() {
    let server = api();
    let ours = run_at(&server, "curl -s --fail-with-body \"$URL/status/404\"").await;
    assert_eq!(ours.code, 22, "{}", ours.err);
    assert_eq!(ours.text_out(), "{\"error\":\"status 404\"}\n");
    assert!(ours.err.contains("returned error: 404"), "stderr: {}", ours.err);
    if let Some((stdout, code)) = oracle(&server, "-s --fail-with-body \"$URL/status/404\"") {
        assert_eq!((stdout.as_str(), code), (ours.text_out().as_ref(), 22));
    }

    // Controls: -f drops the body (same exit), and a 200 is plain success.
    let f = run_at(&server, "curl -s -f \"$URL/status/404\"").await;
    assert_eq!((f.code, f.text_out().as_ref()), (22, ""));
    let ok = run_at(&server, "curl -s --fail-with-body \"$URL/text\"").await;
    assert_eq!((ok.code, ok.text_out().as_ref()), (0, "plain body\n"));
}

#[tokio::test(flavor = "multi_thread")]
async fn fail_with_body_writes_the_error_body_to_the_output_file() {
    let server = api();
    let r = run_at(
        &server,
        "curl -s --fail-with-body -o /tmp/err.json \"$URL/status/404\"\ncat /tmp/err.json",
    )
    .await;
    // The `cat` is the last command, so the script ends 0; the file is what matters.
    assert_eq!(r.text_out(), "{\"error\":\"status 404\"}\n", "{}", r.err);
}

// ── -m ──────────────────────────────────────────────────────────────────

#[tokio::test(flavor = "multi_thread")]
async fn dash_m_is_max_time() {
    let server = Server::builder()
        .route_fn("/slow", |_| {
            std::thread::sleep(std::time::Duration::from_millis(1500));
            Response::text(200, "late")
        })
        .start_tcp();
    let r = run_at(&server, "curl -s -m 0.3 \"$URL/slow\"").await;
    assert_eq!(r.code, 28, "stderr: {}", r.err);
    // Control: without the deadline the same route answers.
    let r = run_at(&server, "curl -s -m 5 \"$URL/slow\"").await;
    assert_eq!((r.code, r.text_out().as_ref()), (0, "late"), "{}", r.err);
}

// ── Refusals that lead with the consequence and name a working form ─────

#[tokio::test(flavor = "multi_thread")]
async fn verbose_refusal_names_the_substitute_and_sends_nothing() {
    let server = api();
    for flag in ["-v", "--verbose"] {
        let r = run_at(&server, &format!("curl {flag} \"$URL/text\"")).await;
        assert_ne!(r.code, 0);
        assert!(r.err.contains("no request was sent"), "{flag}: {}", r.err);
        assert!(r.err.contains("curl -i"), "{flag}: {}", r.err);
        assert!(r.err.contains("User-Agent"), "{flag} must say what request headers are known: {}", r.err);
    }
    assert!(server.requests().is_empty());
}

#[tokio::test(flavor = "multi_thread")]
async fn form_refusal_does_not_offer_data_for_files() {
    let server = Server::builder()
        .route_fn("/upload", |req| {
            Response::text(200, format!("{}\n{}", req.header("Content-Type").unwrap_or(""), req.body_str()))
        })
        .start_tcp();
    for flag in ["-F", "--form"] {
        let r = run_at(&server, &format!("curl {flag} file=@/work/upload.txt \"$URL/upload\"")).await;
        assert_ne!(r.code, 0);
        assert!(r.err.contains("no request was sent"), "{flag}: {}", r.err);
        assert!(r.err.contains("boundary"), "{flag} must show the working multipart form: {}", r.err);
        assert!(
            !r.err.contains("Use '--data'"),
            "{flag}: --data cannot send a file: {}",
            r.err
        );
    }
    assert!(server.requests().is_empty());

    // The form the message shows works, so the substitute is not folklore.
    let r = run_at(
        &server,
        "printf 'hello upload\\n' > /work/upload.txt\n\
         curl -s -H 'Content-Type: multipart/form-data; boundary=XXXX' --data-binary \
         \"$(printf -- '--XXXX\\r\\nContent-Disposition: form-data; name=\"file\"; filename=\"upload.txt\"\\r\\n\\r\\n%s\\r\\n--XXXX--\\r\\n' \"$(cat /work/upload.txt)\")\" \
         \"$URL/upload\"",
    )
    .await;
    assert_eq!(r.code, 0, "stderr: {}", r.err);
    assert!(r.text_out().starts_with("multipart/form-data; boundary=XXXX\n"), "{}", r.text_out());
    assert!(r.text_out().contains("filename=\"upload.txt\""), "{}", r.text_out());
    assert!(r.text_out().contains("hello upload"), "{}", r.text_out());
}
