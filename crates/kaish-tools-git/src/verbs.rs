//! Verb module declarations.
//!
//! `pub(crate) mod` here rather than a `verbs/mod.rs` — kaish's house rule is
//! no `mod.rs` files. Each phasing PR adds its verb beside `info`.
//!
//! When the write profiles land they arrive as `verbs/write/`, gated behind
//! `#[cfg(feature = "worktree")]` / `#[cfg(feature = "commit")]`. Until then
//! that directory does not exist, and the source-scanning invariant test in
//! `tests/write_shaped_identifiers.rs` asserts that no write-shaped plumbing
//! has leaked out of it — into a directory that is not there yet, which is
//! exactly when the assertion is cheapest to keep true.

pub(crate) mod info;
