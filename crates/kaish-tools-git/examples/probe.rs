//! Run kaish commands against a real repository through a real kernel with
//! the read-only `git` tool registered — the same assembly an embedder makes.
//!
//! The repository is mounted at `/mnt/repo`. Each command line runs in a
//! fresh kernel so one command's state cannot leak into the next.
//!
//! ```text
//! cargo run -p kaish-tools-git --example probe -- <repo> <cwd-under-repo> <cmd>...
//! cargo run -p kaish-tools-git --example probe -- ~/src/kaijutsu . 'git status'
//! ```

#[path = "../tests/support.rs"]
mod support;

use std::path::PathBuf;
use std::sync::Arc;

use kaish_kernel::{Kernel, KernelConfig};
use kaish_tool_api::KernelBackend;
use kaish_tools_git::GitConfig;

#[tokio::main(flavor = "multi_thread")]
async fn main() {
    let mut args = std::env::args().skip(1);
    let repo = PathBuf::from(args.next().expect("usage: probe <repo> <cwd> <cmd>..."));
    let cwd = args.next().expect("usage: probe <repo> <cwd> <cmd>...");
    let repo = std::fs::canonicalize(&repo).expect("repo path exists");
    let vfs_cwd = PathBuf::from("/mnt/repo").join(cwd.trim_start_matches("./"));

    for cmd in args {
        let backend: Arc<dyn KernelBackend> = Arc::new(support::StrictBackend::single(
            PathBuf::from("/mnt/repo"),
            repo.clone(),
        ));
        let git = kaish_tools_git::tool(GitConfig::read_only()).expect("valid config");
        let mut cfg = KernelConfig::transient();
        cfg.cwd = vfs_cwd.clone();
        let kernel = Kernel::with_backend(backend, cfg, |_| {}, |tools| tools.register(git))
            .expect("kernel assembles");
        println!("$ {cmd}    # cwd {}", vfs_cwd.display());
        match kernel.execute(&cmd).await {
            Ok(r) => {
                print!("{}", r.text_out());
                if !r.err.is_empty() {
                    println!("[stderr] {}", r.err.trim_end());
                }
                println!("[exit {}]", r.code);
            }
            Err(e) => println!("[kernel error] {e}"),
        }
        println!();
    }
}
