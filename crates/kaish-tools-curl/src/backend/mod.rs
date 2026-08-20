//! The blocking HTTP backend: ureq on native, xhr stub on wasm (deferred).
//!
//! Native builds compile `ureq.rs` behind the `Backend` trait; wasm builds are
//! gated by a `compile_error!` until the XHR path is implemented. Compare
//! `kaish-tools-git` which ships the same pattern for gix.

#[cfg(not(target_family = "wasm"))]
mod ureq;

// `--unix-socket`'s transport. Unix-family only: `UnixStream` does not exist
// on every native target, and the flag is refused where the transport is
// absent rather than silently connecting over TCP instead.
#[cfg(all(not(target_family = "wasm"), unix))]
mod unix;

#[cfg(not(target_family = "wasm"))]
pub use ureq::*;

/// A blocked build of curl cannot execute without a backend.
#[cfg(target_family = "wasm")]
compile_error!("curl requires a native build target; the wasm XHR backend is not yet implemented");
