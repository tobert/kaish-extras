//! `--unix-socket` over a real `UnixListener`.
//!
//! The transport is a ureq `Connector`/`Transport` pair over `UnixStream`
//! (`src/backend/unix.rs`), reached through ureq's `unversioned::transport`
//! API — which carries no semver guarantee, so these tests are also the
//! tripwire for a ureq bump that moves it.
//!
//! Containment is the other half and is tested here too: the path goes
//! through `ToolCtx::resolve_path` and has to land inside a mount, so a
//! socket outside the shell's filesystem is refused rather than opened.

#[path = "support.rs"]
mod support;

use std::sync::Arc;

use support::{argv, curl_with_backend, loopback_config, MemoryBackend, Response, Server};

#[tokio::test]
async fn a_request_goes_over_the_socket_and_the_response_comes_back() {
    let server = Server::builder()
        .route("/info", Response::json(200, r#"{"ok":true}"#))
        .start_unix();

    // Mount the socket's directory at /run, so /run/curl-test.sock is the
    // shell's name for it.
    let dir = server.socket_path().parent().expect("socket has a parent");
    let backend = Arc::new(MemoryBackend::new().with_mount("/run", dir));
    let file = server.socket_path().file_name().expect("socket file name");

    let result = curl_with_backend(
        loopback_config(),
        argv(&[
            "--unix-socket",
            &format!("/run/{}", file.to_string_lossy()),
            &server.url("/info"),
        ]),
        backend,
    )
    .await;

    assert_eq!(result.code, 0, "{}", result.err);
    assert!(result.text_out().contains("\"ok\":true"), "{}", result.text_out());

    let reqs = server.requests();
    assert_eq!(reqs.len(), 1, "the request should have gone over the socket");
    assert_eq!(reqs[0].path_no_query(), "/info");
    // The URL's host is a placeholder that is never resolved, but HTTP still
    // needs it in the Host header.
    assert_eq!(reqs[0].header("host"), Some("localhost"));
}

#[tokio::test]
async fn a_post_body_reaches_the_socket() {
    let server = Server::builder()
        .route("/submit", Response::text(200, "stored"))
        .start_unix();

    let dir = server.socket_path().parent().expect("socket has a parent");
    let backend = Arc::new(MemoryBackend::new().with_mount("/run", dir));
    let file = server.socket_path().file_name().expect("socket file name");

    let result = curl_with_backend(
        loopback_config(),
        argv(&[
            "--unix-socket",
            &format!("/run/{}", file.to_string_lossy()),
            "-d",
            "a=1",
            &server.url("/submit"),
        ]),
        backend,
    )
    .await;

    assert_eq!(result.code, 0, "{}", result.err);
    let reqs = server.requests();
    assert_eq!(reqs[0].method, "POST");
    assert_eq!(reqs[0].body_str(), "a=1");
}

#[tokio::test]
async fn a_socket_outside_the_mount_is_refused() {
    // The containment rule CU9 asks for, applied to the flag that names a
    // path. `..` out of the mount is caught because the check asks the
    // backend where the path really lands, not whether the string looks
    // contained.
    let server = Server::builder()
        .route("/info", Response::text(200, "unreachable"))
        .start_unix();

    let dir = server.socket_path().parent().expect("socket has a parent");
    let nested = dir.join("inner");
    std::fs::create_dir_all(&nested).expect("mount dir");
    let backend = Arc::new(MemoryBackend::new().with_mount("/run", &nested));
    let file = server.socket_path().file_name().expect("socket file name");

    let result = curl_with_backend(
        loopback_config(),
        argv(&[
            "--unix-socket",
            &format!("/run/../{}", file.to_string_lossy()),
            &server.url("/info"),
        ]),
        backend,
    )
    .await;

    assert_ne!(result.code, 0, "a socket outside the mount must be refused");
    assert!(server.requests().is_empty(), "nothing should have been sent");
}

#[tokio::test]
async fn a_socket_on_no_mount_at_all_is_refused() {
    let server = Server::builder()
        .route("/info", Response::text(200, "unreachable"))
        .start_unix();

    // No mount configured, so nothing is reachable by path.
    let backend = Arc::new(MemoryBackend::new());
    let result = curl_with_backend(
        loopback_config(),
        argv(&[
            "--unix-socket",
            &server.socket_path().to_string_lossy(),
            &server.url("/info"),
        ]),
        backend,
    )
    .await;

    assert_ne!(result.code, 0, "with no mounts, no path is reachable");
    assert!(
        result.err.contains("mount"),
        "say why it cannot be reached: {}",
        result.err
    );
    assert!(server.requests().is_empty());
}

#[tokio::test]
async fn a_symlink_out_of_the_mount_is_refused() {
    // The case a lexical check cannot see, and the reason curl canonicalizes
    // instead of trusting `starts_with`: the path is spelled entirely inside
    // the mount, and a symlink inside the mount points out of it. A backend
    // whose `resolve_real_path` simply joins the mount root to the rest of the
    // path — which this harness's does, deliberately — hands back something
    // that passes a prefix test and opens a socket the shell cannot name.
    let server = Server::builder()
        .route("/info", Response::text(200, "unreachable"))
        .start_unix();

    let outside = server.socket_path().parent().expect("socket has a parent");
    let mount_dir = outside.join("mount");
    std::fs::create_dir_all(&mount_dir).expect("mount dir");
    std::os::unix::fs::symlink(outside, mount_dir.join("escape")).expect("symlink");

    let backend = Arc::new(MemoryBackend::new().with_mount("/run", &mount_dir));
    let file = server.socket_path().file_name().expect("socket file name");

    let result = curl_with_backend(
        loopback_config(),
        argv(&[
            "--unix-socket",
            &format!("/run/escape/{}", file.to_string_lossy()),
            &server.url("/info"),
        ]),
        backend,
    )
    .await;

    assert_ne!(result.code, 0, "a symlink out of the mount must be refused");
    assert!(
        result.err.contains("outside the mount"),
        "say what was wrong with it: {}",
        result.err
    );
    assert!(server.requests().is_empty(), "nothing should have been sent");
}
