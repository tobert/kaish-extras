//! The backend seam: docs/curl.md's "Crate shape" names this
//! `trait Backend { fn fetch(...) -> Response }`, implemented by
//! `backend/ureq.rs` (native, `cfg(not(target_family = "wasm"))`) and
//! `backend/xhr.rs` (wasm, `cfg(target_family = "wasm")`) — neither of
//! which exists yet.
//!
//! Empty stub. Even the trait's exact method signature is HTTP-surface work:
//! whether `fetch` takes a `CurlError`-returning `Result`, how a request
//! (method, headers, body, `-k`, `--unix-socket`) is represented, and
//! whether it is sync or async are all open questions the review docs/
//! curl.md's "Status" section calls for should settle, not this skeleton.
//! Neither `ureq.rs` nor `xhr.rs` is created here — the task that added this
//! stub was explicitly told not to, and whether a wasm stub ships in cut 1
//! is itself an open review question (docs/curl.md "Wasm: designed in, not
//! built").
