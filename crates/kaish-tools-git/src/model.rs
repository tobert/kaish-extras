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

/// One column of a status entry, in JSON's self-describing words (B.2).
///
/// The text surface speaks git's porcelain `XY` letters; this is the other
/// half of decision 9 — words in JSON, so a script never has to know that a
/// leading space means "unmodified" or that `?` means "untracked". Each of the
/// two columns (`index`, `worktree`) takes exactly one of these.
///
/// There is deliberately no `unmerged`/`conflicted` word: a conflict is the
/// `conflicted` boolean on the entry (B.2), and this column then carries the
/// side's own change (`modified`, `added`, `deleted`) so the two halves of the
/// model never disagree about *what* changed, only about whether it conflicts.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
#[non_exhaustive]
pub enum EntryStatus {
    /// No change on this side (`git`'s space in the `XY` pair).
    None,
    /// Added: present on this side, absent from the comparison base.
    Added,
    /// Modified: content changed.
    Modified,
    /// Deleted: removed on this side.
    Deleted,
    /// Renamed from [`StatusEntry::orig_path`] — exact-match only (B.2), never
    /// a similarity score.
    Renamed,
    /// Copied. Never produced by this build — copy detection needs the
    /// `gix-diff` `blob` feature, which is exactly what pulls `gix-command`
    /// (A.2). The variant exists so the word set is git-shaped.
    Copied,
    /// The item's type changed (file ↔ symlink ↔ submodule).
    Typechange,
    /// Untracked: in the worktree, in neither the index nor `.gitignore`.
    Untracked,
    /// Ignored: matched by a `.gitignore` / `info/exclude` rule.
    Ignored,
}

/// What kind of thing a status entry describes.
///
/// `commit` is how a submodule gitlink appears (B.6 names it plainly); `dir` is
/// how an untracked directory appears in `--untracked normal`, where git
/// collapses a wholly-untracked directory to a single `path/` row rather than
/// listing its contents.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
#[non_exhaustive]
pub enum EntryKind {
    /// A regular file (executable or not).
    File,
    /// A symbolic link.
    Symlink,
    /// A submodule gitlink.
    Commit,
    /// A directory — only produced for a collapsed untracked directory.
    Dir,
}

/// One changed path in a [`StatusReport`] (architecture.md B.2).
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct StatusEntry {
    /// The repository-relative path, slash-separated.
    pub path: String,
    /// The rename source, set only when `index` or `worktree` is
    /// [`EntryStatus::Renamed`]. Exact-match renames only.
    pub orig_path: Option<String>,
    /// What kind of item this is.
    pub kind: EntryKind,
    /// The staged column (index vs `HEAD`).
    pub index: EntryStatus,
    /// The unstaged column (worktree vs index).
    pub worktree: EntryStatus,
    /// Whether this path is unmerged (a nonzero index stage).
    pub conflicted: bool,
    /// The two porcelain letters this entry renders as, `XY` (B.2). Carried on
    /// the model so the text renderer and the JSON words are computed from one
    /// source; skipped from `--json`, which speaks words, not letters.
    #[serde(skip)]
    pub porcelain: [char; 2],
}

/// The five running counts a status reports (architecture.md B.2).
///
/// `staged` and `unstaged` count *columns*, not entries: a path modified and
/// re-modified without staging (git's `MM`) counts in both, exactly as
/// `git status` totals it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Default)]
pub struct StatusTotals {
    /// Entries with a staged change (a non-empty index column).
    pub staged: usize,
    /// Entries with an unstaged change (a non-empty worktree column).
    pub unstaged: usize,
    /// Untracked entries.
    pub untracked: usize,
    /// Ignored entries (only ever nonzero with `--ignored`).
    pub ignored: usize,
    /// Unmerged (conflicted) entries.
    pub conflicted: usize,
}

/// `git status`'s result (architecture.md B.2).
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct StatusReport {
    /// Where HEAD points.
    pub head: Head,
    /// The changed paths, capped at the effective `--limit`.
    pub entries: Vec<StatusEntry>,
    /// The running counts, taken over the *untruncated* set — a total is a
    /// fact about the repository, not about how many rows fit under `--limit`.
    pub totals: StatusTotals,
    /// Whether the working tree is clean: no staged, unstaged, untracked or
    /// conflicted changes. Ignored entries do not make a tree dirty.
    pub clean: bool,
    /// Whether the entry list was truncated by `--limit`. Always reported,
    /// never silent (E.5); a stderr note fires alongside it.
    pub truncated: bool,
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

    /// B.2 in JSON speaks words, and the porcelain letters must not leak into
    /// it — the letters are the *text* surface, and a script reading `--json`
    /// should never have to know that `?` means untracked.
    #[test]
    fn status_entry_serializes_words_not_letters() {
        let entry = StatusEntry {
            path: "src/lib.rs".into(),
            orig_path: None,
            kind: EntryKind::File,
            index: EntryStatus::Modified,
            worktree: EntryStatus::None,
            conflicted: false,
            porcelain: ['M', ' '],
        };
        let json = serde_json::to_value(&entry).expect("StatusEntry serializes");
        assert_eq!(json["index"], "modified");
        assert_eq!(json["worktree"], "none");
        assert_eq!(json["kind"], "file");
        assert!(json["orig_path"].is_null());
        assert!(
            json.get("porcelain").is_none(),
            "the porcelain letters must not appear in JSON: {json}"
        );
    }

    /// The report's key names are the B.2 wire contract, same as B.1's.
    #[test]
    fn status_report_serializes_with_the_documented_keys() {
        let report = StatusReport {
            head: Head {
                branch: Some("main".into()),
                oid: Some("0".repeat(40)),
                detached: false,
            },
            entries: vec![],
            totals: StatusTotals::default(),
            clean: true,
            truncated: false,
        };
        let json = serde_json::to_value(&report).expect("StatusReport serializes");
        for key in ["head", "entries", "totals", "clean", "truncated"] {
            assert!(json.get(key).is_some(), "B.2 key {key} missing: {json}");
        }
        for key in ["staged", "unstaged", "untracked", "ignored", "conflicted"] {
            assert!(
                json["totals"].get(key).is_some(),
                "totals.{key} missing: {json}"
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
