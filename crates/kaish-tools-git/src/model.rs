//! The typed results verbs produce (architecture.md A.1, B).
//!
//! Every value here is owned and free of both gix types and kaish types. That
//! is not tidiness: it is what makes E.3's rule — *no gix value ever crosses
//! an `.await`* — enforceable. A verb opens the repository, works, and
//! produces one of these inside a single blocking closure; nothing `!Send`
//! survives the closure's end.
//!
//! Only `git info`'s model exists so far. Later phasing PRs add theirs beside
//! it.

use std::collections::BTreeMap;

use serde::Serialize;

/// The ref storage backend a repository uses.
///
/// Only `Files` is reachable: a reftable repository is refused at open time
/// with exit 4 (E.5), so this never carries a backend we did not read.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
#[non_exhaustive]
pub enum RefBackend {
    /// Loose refs under `refs/` plus `packed-refs` — the only backend
    /// gitoxide can read today.
    Files,
}

/// Where HEAD points.
///
/// The three states are all representable, including the one git tools
/// routinely get wrong: an unborn branch, where `branch` names a ref that
/// does not exist yet and `oid` is therefore `None`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Head {
    /// The short branch name HEAD points at, or `None` when detached.
    pub branch: Option<String>,
    /// The commit HEAD resolves to, or `None` on an unborn branch.
    pub oid: Option<String>,
    /// Whether HEAD names an object directly rather than a branch.
    pub detached: bool,
}

/// What this build will let the caller ask for.
///
/// Discoverability, not authority (B.1): the agent learns what it may ask for
/// without gaining any way to change it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Capabilities {
    /// Enabled profile names, e.g. `["read"]`.
    pub profiles: Vec<String>,
    /// Enabled verb names, in schema order.
    pub verbs: Vec<String>,
    /// Cargo features this build was compiled with.
    pub features: Vec<String>,
    /// The embedder's output caps.
    pub limits: LimitsReport,
}

/// The [`crate::Limits`] an embedder configured, as reported to the caller.
///
/// A separate type from `Limits` on purpose: `model` depends on nothing of
/// ours (A.1), and the wire shape should be free to diverge from the
/// embedder-facing struct without either dragging the other.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub struct LimitsReport {
    /// Maximum rows for any row-producing verb.
    pub max_rows: usize,
    /// Maximum files reported by a single diff.
    pub max_diff_files: usize,
    /// Maximum bytes of blob content returned by `git show`.
    pub max_blob_bytes: u64,
    /// Maximum bytes of hunk text per file.
    pub max_hunk_bytes_per_file: u64,
    /// How deep to descend into submodules.
    pub submodule_depth: u8,
}

/// `git info`'s result (architecture.md B.1) — what am I looking at, and what
/// am I allowed to do.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct RepoInfo {
    /// The repository root as a VFS path, or `None` when it cannot be mapped
    /// back into the mount the caller reached it through. An agent should be
    /// told when it cannot name a path it can see.
    pub repo_root_vfs: Option<String>,
    /// The repository root on the host filesystem.
    pub repo_root_real: String,
    /// The git directory on the host filesystem. For a linked worktree this
    /// is the worktree's private git dir, not the common dir.
    pub git_dir: String,
    /// Whether the repository has no working tree.
    pub bare: bool,
    /// Whether history is truncated (`shallow` exists in the common dir).
    pub shallow: bool,
    /// The ref storage backend actually read.
    pub ref_backend: RefBackend,
    /// Where HEAD points.
    pub head: Head,
    /// How many working trees this repository has: the main one, when it has
    /// one, plus every registered linked worktree. A bare repository with no
    /// linked worktrees reports 0.
    pub worktrees: usize,
    /// How many submodules `.gitmodules` declares in the working tree. A bare
    /// repository reports 0 — there is no working tree to read it from.
    pub submodules: usize,
    /// The gitoxide plumbing crates this build links, and their versions.
    /// There is no single facade version to report (A.2), so the pin set is
    /// what an embedder can act on.
    pub gix_pins: BTreeMap<String, String>,
    /// What this build will let the caller ask for.
    pub capabilities: Capabilities,
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The B.1 key names are a wire contract with agents that were shown the
    /// design's example object. A rename here is a silent breaking change for
    /// every prompt that learned the old shape.
    #[test]
    fn repo_info_serializes_with_the_documented_keys() {
        let info = RepoInfo {
            repo_root_vfs: Some("/mnt/repos/kaish".into()),
            repo_root_real: "/srv/kaish".into(),
            git_dir: "/srv/kaish/.git".into(),
            bare: false,
            shallow: false,
            ref_backend: RefBackend::Files,
            head: Head {
                branch: Some("main".into()),
                oid: Some("0".repeat(40)),
                detached: false,
            },
            worktrees: 2,
            submodules: 0,
            gix_pins: BTreeMap::from([("gix-object".to_string(), "0.63.0".to_string())]),
            capabilities: Capabilities {
                profiles: vec!["read".into()],
                verbs: vec!["info".into()],
                features: vec!["read".into()],
                limits: LimitsReport {
                    max_rows: 1000,
                    max_diff_files: 500,
                    max_blob_bytes: 8 * 1024 * 1024,
                    max_hunk_bytes_per_file: 256 * 1024,
                    submodule_depth: 1,
                },
            },
        };
        let json = serde_json::to_value(&info).expect("RepoInfo must serialize");
        for key in [
            "repo_root_vfs",
            "repo_root_real",
            "git_dir",
            "bare",
            "shallow",
            "ref_backend",
            "head",
            "worktrees",
            "submodules",
            "gix_pins",
            "capabilities",
        ] {
            assert!(json.get(key).is_some(), "B.1 key {key} is missing: {json}");
        }
        assert_eq!(json["ref_backend"], "files");
        assert_eq!(json["head"]["branch"], "main");
        assert_eq!(json["head"]["detached"], false);
        for key in ["profiles", "verbs", "features", "limits"] {
            assert!(
                json["capabilities"].get(key).is_some(),
                "capabilities.{key} is missing: {json}"
            );
        }
    }

    /// An unborn branch is a real state (`git init` then `git info`), and the
    /// honest encoding is a named branch with no oid — not a fabricated zero
    /// oid and not a missing branch.
    #[test]
    fn unborn_head_is_representable() {
        let head = Head {
            branch: Some("main".into()),
            oid: None,
            detached: false,
        };
        let json = serde_json::to_value(&head).expect("Head must serialize");
        assert_eq!(json["branch"], "main");
        assert!(json["oid"].is_null(), "unborn HEAD must report a null oid");
    }
}
