//! A shallow, safety-first git read surface for kaish, built on gitoxide
//! plumbing crates rather than the `gix` facade — see
//! [`docs/git.md`](https://github.com/tobert/kaish-extras/blob/main/docs/git.md)
//! for the history and design intent, and
//! [`docs/design/architecture.md`](https://github.com/tobert/kaish-extras/blob/main/docs/design/architecture.md)
//! for the crate layout, the verb surface, and the read-only enforcement
//! argument this crate exists to prove.
//!
//! # Registering it
//!
//! ```no_run
//! use kaish_tools_git::GitConfig;
//! # fn demo() -> Result<(), Box<dyn std::error::Error>> {
//! let git = kaish_tools_git::tool(GitConfig::read_only())?;
//! # let _ = git;
//! # Ok(()) }
//! ```
//!
//! # Read-only, and why you should believe it
//!
//! The claim is not that a flag is checked. It is that in a default build the
//! code that could write does not exist: [`ReadRepo`] assembles the plumbing
//! handles itself and never lends them, no build links `gix-command` /
//! `gix-transport` / `gix-filter` (CI asserts their absence), and
//! `tests/readonly_fingerprint.rs` fingerprints every path, size, mtime and
//! content hash under `.git` before and after running every read verb, then
//! asserts nothing moved.
//!
//! PR 1 landed `git info`, the repository open path, the fixture harness, and
//! that fingerprint test. PR 2 adds `git status`, hand-composed on the index,
//! a worktree walk, and a tree↔index comparison — `gix-status` is disqualified
//! because it hard-requires `gix-filter` (→ `gix-command`), the spawn
//! machinery the tripwires forbid. PR 3 adds `git log`: a `gix-traverse`
//! commit walk, a small revision grammar, and `--stat` line counts from
//! `gix-diff`'s tree walk plus `gix-imara-diff` — none of which links
//! `gix-command` either.

#[cfg(target_family = "wasm")]
compile_error!(
    "kaish-tools-git does not build for wasm targets yet: gix-sec fails to \
     compile on wasm32-unknown-unknown (ownership check uses geteuid). \
     Tracked in kaish-extras GH #3. Build kaish-web without the git tool."
);

// The whole implementation is native-only. Gating the module tree — rather
// than letting every module fail to resolve its imports — keeps a wasm
// build's output down to the one honest message above: the dependencies are
// target-scoped, so on wasm there is nothing for these modules to be written
// against.
#[cfg(not(target_family = "wasm"))]
mod config;
#[cfg(not(target_family = "wasm"))]
mod error;
#[cfg(not(target_family = "wasm"))]
mod index_depth_guard;
#[cfg(not(target_family = "wasm"))]
mod model;
#[cfg(not(target_family = "wasm"))]
mod pathfilter;
#[cfg(not(target_family = "wasm"))]
mod render;
#[cfg(not(target_family = "wasm"))]
mod repo;
#[cfg(not(target_family = "wasm"))]
mod tool;
#[cfg(not(target_family = "wasm"))]
mod verbs;

#[cfg(not(target_family = "wasm"))]
pub use config::{ConfigError, GitConfig, Limits, Profile, Verb};
#[cfg(not(target_family = "wasm"))]
pub use error::GitError;
#[cfg(not(target_family = "wasm"))]
pub use model::{
    Capabilities, CommitInfo, EntryKind, EntryStatus, Head, LimitsReport, LogReport, RefBackend,
    RepoInfo, Signature, StatSummary, StatusEntry, StatusReport, StatusTotals,
};
#[cfg(not(target_family = "wasm"))]
pub use repo::ReadRepo;
#[cfg(not(target_family = "wasm"))]
pub use tool::{tool, GitTool};

/// The gitoxide plumbing crates this build links, and the exact versions it
/// is pinned to (architecture.md A.4).
///
/// There is no single facade version to report — that is the point of Path 2 —
/// so `git info` names the pin set instead. Hand-maintained beside the
/// manifest, with `pins_match_the_manifest` below as the thing that says so:
/// it parses `Cargo.toml`'s `=` pins and fails if the two drift.
#[cfg(not(target_family = "wasm"))]
const GIX_PINS: &[(&str, &str)] = &[
    ("gix-actor", "0.41.2"),
    ("gix-commitgraph", "0.38.0"),
    ("gix-config", "0.59.0"),
    ("gix-date", "0.15.6"),
    ("gix-diff", "0.66.0"),
    ("gix-discover", "0.54.0"),
    ("gix-glob", "0.27.0"),
    ("gix-ignore", "0.22.0"),
    ("gix-imara-diff", "0.2.4"),
    ("gix-index", "0.54.0"),
    ("gix-object", "0.63.0"),
    ("gix-odb", "0.83.0"),
    ("gix-pack", "0.73.0"),
    ("gix-ref", "0.66.0"),
    ("gix-revision", "0.48.0"),
    ("gix-revwalk", "0.34.0"),
    ("gix-traverse", "0.60.0"),
    ("gix-worktree", "0.55.0"),
];

/// The pin set, as `git info` reports it.
#[cfg(not(target_family = "wasm"))]
pub fn gix_pins() -> std::collections::BTreeMap<String, String> {
    GIX_PINS
        .iter()
        .map(|(name, version)| (name.to_string(), version.to_string()))
        .collect()
}

/// The cargo features this build was compiled with.
///
/// `read` is unconditional today: architecture.md A.2's feature axes exist in
/// the design, but only the read profile is implemented, so declaring cargo
/// features that gate nothing would advertise a choice an embedder does not
/// have. Each axis arrives with the code it gates.
#[cfg(not(target_family = "wasm"))]
pub fn enabled_features() -> Vec<String> {
    vec!["read".to_string()]
}

#[cfg(all(test, not(target_family = "wasm")))]
mod tests {
    use super::*;

    /// `git info` reports `gix_pins` as fact. A hand-maintained table that
    /// drifted from the manifest would name a version this build does not
    /// link — a lie an embedder would act on when deciding whether a gitoxide
    /// advisory applies to them.
    #[test]
    fn pins_match_the_manifest() {
        let manifest = include_str!("../Cargo.toml");
        let mut from_manifest: Vec<(String, String)> = Vec::new();
        for line in manifest.lines() {
            let line = line.trim();
            if !line.starts_with("gix-") {
                continue;
            }
            let Some((name, rest)) = line.split_once('=') else {
                continue;
            };
            let name = name.trim();
            // `version = "=X.Y.Z"` — take what sits between the quotes after
            // the `=` marker that makes it an exact pin.
            let start = rest
                .find("\"=")
                .unwrap_or_else(|| panic!("{name} is not an exact `=` pin: {line}"));
            let after = &rest[start + 2..];
            let end = after
                .find('"')
                .unwrap_or_else(|| panic!("unterminated version string for {name}: {line}"));
            from_manifest.push((name.to_string(), after[..end].to_string()));
        }
        from_manifest.sort();
        assert!(
            !from_manifest.is_empty(),
            "found no gix pins in Cargo.toml — this test would pass vacuously"
        );

        let mut declared: Vec<(String, String)> = GIX_PINS
            .iter()
            .map(|(n, v)| (n.to_string(), v.to_string()))
            .collect();
        declared.sort();

        assert_eq!(
            declared, from_manifest,
            "GIX_PINS has drifted from Cargo.toml's exact pins; `git info` \
             would report versions this build does not link"
        );
    }

    #[test]
    fn gix_pins_are_reported_as_a_map() {
        let pins = gix_pins();
        assert_eq!(pins.get("gix-object").map(String::as_str), Some("0.63.0"));
        assert_eq!(pins.len(), GIX_PINS.len());
    }
}
