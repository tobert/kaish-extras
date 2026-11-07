//! Verb module declarations.
//!
//! `pub(crate) mod` here rather than a `verbs/mod.rs` — kaish's house rule is
//! no `mod.rs` files. Each phasing PR adds its verb beside `info`.
//!
//! The write profiles live in `verbs/write/`, gated behind
//! `#[cfg(feature = "commit")]` (and a `worktree` feature when that exists).
//! The source-scanning invariant test in `tests/write_shaped_identifiers.rs`
//! asserts that no write-shaped plumbing appears outside that directory.

pub(crate) mod branch;
pub(crate) mod diff;
pub(crate) mod info;
pub(crate) mod log;
pub(crate) mod ls;
pub(crate) mod show;
pub(crate) mod status;
pub(crate) mod tag;
pub(crate) mod worktree;

// Phase 1a of the commit profile: the scaffolding every write verb stands on
// (`WriteRepo`, write containment, reflog-message validation). No verb calls
// it yet, so the unit tests are its only callers; phase 1b removes the allow.
#[cfg(feature = "commit")]
#[allow(dead_code)]
pub(crate) mod write;
