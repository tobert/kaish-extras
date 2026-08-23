//! A canary for kaish behaviors this crate's *callers* depend on, pinned so a
//! dependency bump that changes one fails here instead of surfacing as a
//! mystery in an embedder (docs/issues.md, X3).
//!
//! kaijutsu — the live embedder registering this crate on a real kernel —
//! named exactly two behaviors it depends on, by asking it directly:
//!
//! 1. The egress allowlist refuses a non-allowlisted host **before any
//!    connection is attempted**. kaijutsu's own test asserts this with no
//!    network available, which is the property that makes it meaningful,
//!    and depends on the *ordering* (`permit_egress` ahead of DNS) as much
//!    as on the refusal itself.
//! 2. `-k`/`--insecure` is a **parse-time** refusal, not a runtime one: with
//!    `insecure_permitted` off (kaijutsu's own config, and the default), the
//!    flag is rejected before egress is even consulted.
//!
//! Every case here drives a real `kaish_kernel::Kernel` with this crate's
//! tool registered, exactly as an embedder does, and every case carries a
//! negative control — an assertion that the *other* answer is reachable
//! through the same kernel in the same test. Without one, a case that pins a
//! refusal passes just as well when the whole tool is broken (see
//! `crates/kaish-tools-git/tests/kaish_behavior_canary.rs`'s header for how
//! that bit a first draft there).
//!
//! **Neither case here needs network access to pass.** The denied-host case
//! uses `.invalid` (RFC 2606: a TLD permanently reserved, never delegated,
//! guaranteed not to resolve) precisely so the assertion is *real*: the
//! egress check happens on the URL's host before any socket is opened or any
//! name is resolved, so denying it never touches DNS. If the allowlist check
//! were ever moved to run after resolution, this same request would instead
//! have to resolve `.invalid` first — which fails, offline or on — and the
//! test would go red on both the exit code (6, "could not resolve host",
//! not 7) and the message (no "egress allowlist" substring). That is what
//! makes the assertion about *ordering*, not just about the refusal
//! existing. The negative control's own connection attempt is to
//! `127.0.0.1` on a closed port — loopback, so it needs no network route
//! either, and a real "connection failed" is a categorically different
//! failure from a policy refusal.

#[path = "support.rs"]
mod support;

use std::path::PathBuf;
use std::sync::Arc;

use kaish_kernel::{Kernel, KernelConfig};
use kaish_tool_api::KernelBackend;
use kaish_types::ExecResult;

use kaish_tools_curl::{AllowByList, CurlConfig};

use support::MemoryBackend;

/// A syntactically valid URL whose host can never resolve — see the module
/// doc comment for why `.invalid` is the deliberate choice, not a stand-in
/// for "some host we didn't allowlist".
const DENIED_URL: &str = "https://nope.example.invalid/";

/// A loopback address allowlisted by its literal IP, on a port nothing
/// listens on. Reaching this at all proves the allowlist let the request
/// through; failing to connect to it proves that got past the allowlist
/// into a real (if doomed) connection attempt, with no network required.
const ALLOWED_BUT_UNREACHABLE_URL: &str = "http://127.0.0.1:1/";

/// A `CurlConfig` whose egress allowlist holds exactly one entry — the
/// loopback address the negative control dials — and denies everything
/// else, `DENIED_URL` included. Insecure is left at its default (refused).
fn config() -> CurlConfig {
    CurlConfig::default().with_allow_egress(AllowByList::new().with_allowed_hosts(["127.0.0.1"]))
}

/// Build a `kaish_kernel::Kernel` with this crate's curl tool registered
/// under `cfg`, exactly as an embedder does.
async fn build_kernel(cfg: CurlConfig) -> Kernel {
    // No mount: neither case here touches `-o` or `--unix-socket`, so the
    // backend only needs to exist, not to hold real paths.
    let backend: Arc<dyn KernelBackend> = Arc::new(MemoryBackend::new());
    let curl = kaish_tools_curl::tool(cfg);

    let mut kernel_cfg = KernelConfig::transient();
    kernel_cfg.cwd = PathBuf::from("/");

    Kernel::with_backend(backend, kernel_cfg, |_| {}, |tools| tools.register(curl))
        .expect("kernel assembles from a valid curl tool")
}

/// Run a script through a fresh kernel built from `cfg`. Unlike the git
/// canary's `run`, a non-zero exit is exactly what several cases here
/// expect, so it is returned rather than asserted away.
async fn run(cfg: CurlConfig, script: &str) -> ExecResult {
    let kernel = build_kernel(cfg).await;
    kernel
        .execute(script)
        .await
        .unwrap_or_else(|e| panic!("kernel failed to execute {script:?}: {e}"))
}

/// The egress allowlist refuses a non-allowlisted host before any connection
/// is attempted: exit 7, and a message naming the policy that stopped it.
///
/// The negative control is `ALLOWED_BUT_UNREACHABLE_URL` in the *same*
/// config: its host is allowlisted, so the request gets past the check this
/// test pins and fails at a real (loopback) connection attempt instead —
/// proving the exit-7-plus-"egress allowlist" pairing above is the allowlist
/// doing the stopping, not some other reason every request in this test
/// fails for.
#[tokio::test]
async fn egress_allowlist_refuses_a_denied_host_before_any_network_attempt() {
    let denied = run(config(), &format!("curl '{DENIED_URL}'")).await;
    assert_eq!(
        denied.code, 7,
        "a denied host must be refused with exit 7; got {}: {}",
        denied.code, denied.err
    );
    assert!(
        denied.err.contains("egress allowlist"),
        "the refusal must name the policy that stopped it; got: {:?}",
        denied.err
    );

    // Negative control: the same kernel, an allowlisted host instead. This
    // must NOT read "egress allowlist" — it must fail as a real connection
    // attempt, proving the check above is what stops the denied case, not a
    // universal failure this whole test would pass under regardless.
    let allowed = run(config(), &format!("curl '{ALLOWED_BUT_UNREACHABLE_URL}'")).await;
    assert!(
        !allowed.err.contains("egress allowlist"),
        "control: an allowlisted host must get past the allowlist, or the \
         denied case above proves nothing; got: {:?}",
        allowed.err
    );
    assert!(
        allowed.err.contains("failed to connect") || allowed.err.contains("connect"),
        "control: the allowlisted host must fail as a real connection \
         attempt (loopback, closed port), not silently succeed; got: {:?}",
        allowed.err
    );
}

/// `-k`/`--insecure` is refused at parse time, before egress is consulted at
/// all: with `insecure_permitted` off (the default), `curl -k <url>` fails
/// with a message naming the refusal, even against a host the allowlist
/// would also deny — because the parser never gets far enough to ask.
///
/// The negative control is the same command against a config that permits
/// `-k`: the parser accepts the flag and the request reaches the egress
/// check instead, which then denies `DENIED_URL` for the ordinary allowlist
/// reason. Two different refusal messages for the same command line is what
/// proves which check ran first, not just that a refusal happened.
#[tokio::test]
async fn insecure_flag_is_refused_at_parse_time_before_egress_is_consulted() {
    let refused = run(config(), &format!("curl -k '{DENIED_URL}'")).await;
    assert!(
        refused.err.contains("is not permitted here"),
        "-k must be refused at parse time when the embedder has not \
         permitted it; got: {:?}",
        refused.err
    );
    assert!(
        !refused.err.contains("egress allowlist"),
        "control half of the same assertion: if this ever read \"egress \
         allowlist\" instead, -k stopped being refused before egress is \
         consulted — the exact regression this test exists to catch; \
         got: {:?}",
        refused.err
    );

    // Negative control: -k permitted, same denied host. The parser now lets
    // it through, so the request reaches (and is stopped by) the egress
    // check instead — proving the message above is a parse-time refusal,
    // not just the word "permitted" appearing for some unrelated reason.
    let permitted_cfg = config().with_insecure_permitted(true);
    let past_parser = run(permitted_cfg, &format!("curl -k '{DENIED_URL}'")).await;
    assert!(
        past_parser.err.contains("egress allowlist"),
        "control: with -k permitted, the same command must reach and be \
         stopped by the egress check instead; got: {:?}",
        past_parser.err
    );
    assert!(
        !past_parser.err.contains("is not permitted here"),
        "control: the parse-time refusal message must not appear once -k \
         is permitted; got: {:?}",
        past_parser.err
    );
}
