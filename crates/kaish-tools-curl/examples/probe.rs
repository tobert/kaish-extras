//! Replay curl command lines through a real kaish kernel with the `curl`
//! tool registered and loopback egress allowed — the assembly an embedder
//! makes. Used by docs/design/curl-survey-2026-11.md.
//!
//! Each command runs in a fresh kernel with an in-memory filesystem; the
//! working directory is `/work`, seeded with the survey fixtures (FIXTURES).
//! `/tmp` exists but is empty. Files the command creates or changes are reported with their contents.
//!
//! ```text
//! cargo run -p kaish-tools-curl --example probe -- help
//! cargo run -p kaish-tools-curl --example probe -- run commands.jsonl > out.jsonl
//! ```
//!
//! `run` reads one `{"id": .., "cmd": ..}` per line and writes one JSON
//! object per line: `{id, cmd, stdout, stderr, exit, files}`.

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::Arc;

use kaish_kernel::vfs::{MemoryFs, VfsRouter};
use kaish_kernel::{Kernel, KernelConfig, LocalBackend};
use kaish_tool_api::KernelBackend;
use kaish_tools_curl::{AllowByList, CurlConfig};

/// Seeded into `/work` before every command. Keep in step with `fixtures.sh`.
const FIXTURES: &[(&str, &str)] = &[
    ("payload.json", "{\"name\":\"widget\",\"qty\":3}\n"),
    ("upload.txt", "hello upload\n"),
    ("token.txt", "abc123\n"),
    ("partial.txt", "0123456789abcdef\n0123456789abcdef\n0123456789abcdef\n"),
];

async fn kernel() -> Kernel {
    let mut vfs = VfsRouter::new();
    vfs.mount("/", MemoryFs::new());
    let backend: Arc<dyn KernelBackend> = Arc::new(LocalBackend::new(Arc::new(vfs)));
    let cfg = CurlConfig::default()
        .with_allow_egress(AllowByList::new().with_allow_loopback(true));
    let curl = kaish_tools_curl::tool(cfg);
    let mut kcfg = KernelConfig::transient();
    kcfg.cwd = PathBuf::from("/work");
    Kernel::with_backend(backend, kcfg, |_| {}, |tools| tools.register(curl))
        .expect("kernel assembles")
}

async fn seeded() -> Kernel {
    let k = kernel().await;
    // /tmp exists so shell workarounds that stage a scratch file are measured,
    // not refused for a missing directory the harness forgot.
    k.execute("mkdir -p /work /tmp").await.expect("mkdir /work /tmp");
    for (name, body) in FIXTURES {
        let q = body.replace('\'', "'\\''");
        let script = format!("printf '%s' '{q}' > /work/{name}");
        let r = k.execute(&script).await.expect("seed fixture");
        assert_eq!(r.code, 0, "seeding {name} failed: {}", r.err);
    }
    k
}

async fn run_one(cmd: &str) -> serde_json::Value {
    let k = seeded().await;
    let (stdout, stderr, code) = match k.execute(cmd).await {
        Ok(r) => (r.text_out().to_string(), r.err.clone(), r.code),
        Err(e) => (String::new(), format!("[kernel error] {e}"), -1),
    };
    let mut files = BTreeMap::new();
    if let Ok(ls) = k.execute("ls /work").await {
        for name in ls.text_out().split_whitespace() {
            let seeded = FIXTURES.iter().find(|(n, _)| *n == name);
            let Ok(r) = k.execute(&format!("cat '/work/{name}'")).await else { continue };
            let body = r.text_out().to_string();
            if seeded.is_none_or(|(_, orig)| *orig != body) {
                files.insert(name.to_string(), body);
            }
        }
    }
    serde_json::json!({ "stdout": stdout, "stderr": stderr, "exit": code, "files": files })
}

#[tokio::main(flavor = "multi_thread")]
async fn main() {
    let mut args = std::env::args().skip(1);
    match args.next().as_deref() {
        Some("help") => {
            let k = kernel().await;
            let r = k.execute("help curl").await.expect("help runs");
            print!("{}", r.text_out());
        }
        Some("run") => {
            let path = args.next().expect("usage: probe run <commands.jsonl>");
            let text = std::fs::read_to_string(&path).expect("commands file readable");
            for line in text.lines().filter(|l| !l.trim().is_empty()) {
                let v: serde_json::Value = serde_json::from_str(line).expect("jsonl line");
                let cmd = v["cmd"].as_str().expect("cmd is a string").to_string();
                let mut out = run_one(&cmd).await;
                out["id"] = v["id"].clone();
                out["cmd"] = serde_json::Value::String(cmd);
                println!("{out}");
            }
        }
        _ => panic!("usage: probe help | probe run <commands.jsonl>"),
    }
}
