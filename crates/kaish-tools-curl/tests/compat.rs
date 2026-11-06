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
    if out.status.code().is_none() {
        return None;
    }
    Some((
        String::from_utf8_lossy(&out.stdout).into_owned(),
        out.status.code().unwrap_or(-1),
    ))
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
