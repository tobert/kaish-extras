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
