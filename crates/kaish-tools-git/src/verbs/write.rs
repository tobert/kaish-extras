//! The commit profile's write side (docs/design/git-write-profile.md).
//!
//! This directory is the only place write-shaped plumbing may appear
//! (architecture.md D.1; `tests/write_shaped_identifiers.rs` scans for it).
//! Phase 1a holds the scaffolding every write verb stands on and no verb:
//!
//! - [`repo::WriteRepo`], a [`crate::ReadRepo`] plus a ref store that writes
//!   reflogs;
//! - [`containment`], the `lstat` walk each write passes before it touches the
//!   disk (§6.1);
//! - [`reflog`], the message check gix-ref never runs.
//!
//! No `mod.rs`, by house rule.

pub(crate) mod add;
pub(crate) mod commit;
pub(crate) mod ident;
pub(crate) mod message;
pub(crate) mod refmove;
pub(crate) mod tree;
pub(crate) mod var;
pub(crate) mod attributes;
pub(crate) mod containment;
pub(crate) mod reflog;
pub(crate) mod head;
pub(crate) mod ignore;
pub(crate) mod indexfile;
pub(crate) mod mv;
pub(crate) mod objects;
pub(crate) mod refusal;
pub(crate) mod relpath;
pub(crate) mod rm;
pub(crate) mod repo;
pub(crate) mod spec;
pub(crate) mod stage;
pub(crate) mod state;
pub(crate) mod unstage;

// Unix-only: the fixtures plant symbolic and hard links and run real git.
#[cfg(all(test, unix))]
mod fixtures;
