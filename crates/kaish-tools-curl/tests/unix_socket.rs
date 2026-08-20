//! `--unix-socket` functional coverage — currently just the refusal.
//!
//! docs/curl.md's "Native backend: ureq" section and docs/issues.md's CU7
//! entry describe the intended design: a custom ureq `Transport` over
//! `std::os::unix::net::UnixStream` (ureq's unstable
//! `unversioned::transport` API), routed through `ToolCtx::resolve_path` +
//! `backend().resolve_real_path()` the same way `-o` is. None of that is
//! built. `args.rs::find_refused_flag` refuses `--unix-socket` at parse
//! time instead — CU7's "Corrected 2026-08-20" entry explains why: the flag
//! used to be parsed into `Request::unix_socket` and never read by
//! `backend/ureq.rs`, so a caller asking for a unix socket silently got a
//! TCP connection to the URL's host instead. Refusing beats that.
//!
//! `support::Server::start_unix` and `UnixGuard` exist in the harness and
//! work — they stand up a real `UnixListener` and capture requests sent
//! over it, ready for the day a transport exists to drive one. Nothing in
//! this crate can reach that transport yet, so this file does not use them:
//! standing up a `UnixGuard` here and never sending it a request would be
//! dead harness code, and pretending to exercise it would be worse — it
//! would claim `--unix-socket` works. This file stays this short and this
//! honest until CU7 lands a transport, at which point it grows the real
//! request/response coverage `surface.rs` has for TCP.

#[path = "support.rs"]
mod support;

use support::{argv, curl, loopback_config};

#[tokio::test]
async fn unix_socket_is_refused_at_parse_time_not_silently_ignored() {
    let result = curl(
        loopback_config(),
        argv(&["--unix-socket", "/tmp/kaish-curl-test.sock", "http://127.0.0.1:1/"]),
    )
    .await;

    assert_ne!(
        result.code, 0,
        "--unix-socket must be refused outright, not accepted and quietly ignored"
    );
    assert!(
        result.err.contains("--unix-socket"),
        "the refusal should name the flag: {}",
        result.err
    );
    assert!(
        result.err.to_lowercase().contains("af_unix") || result.err.to_lowercase().contains("transport"),
        "the refusal should say *why* — no transport exists — not just refuse silently: {}",
        result.err
    );
}
