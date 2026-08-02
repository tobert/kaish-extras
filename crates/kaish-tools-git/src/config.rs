//! The embedder-supplied profile config (architecture.md C).
//!
//! No config file, deliberately. A config that decides what an agent may do
//! must not be reachable from inside the sandbox, and a file would add a
//! lookup path, a parse failure mode, and a tempting write target. This is a
//! Rust struct the embedder builds and hands to [`crate::tool`].
//!
//! The shape is **subtractive**: [`GitConfig::read_only`] is the only
//! constructor, and [`GitConfig::without_verb`] is the only way to change
//! which verbs exist. There is no `with_verb`, so no config edit anywhere can
//! widen the surface past what the constructor granted.
//!
//! This module depends on nothing else of ours and nothing of kaish's — the
//! dependency direction in A.1 that keeps a future `kaish-git-core` split
//! mechanical.

use std::collections::BTreeSet;

/// A capability profile: the set of verbs an embedder is opting into.
///
/// `#[non_exhaustive]` and, for now, single-variant. `Profile::Worktree` and
/// `Profile::Commit` do not exist yet — not as unconstructible variants, not
/// as `#[doc(hidden)]` placeholders. C.3 gates them behind a deliberate
/// opt-in constructor that returns a *different* type, and none of that
/// machinery is written, so the honest representation of "this build cannot
/// write" is that there is nothing here to name.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
#[non_exhaustive]
pub enum Profile {
    /// The read profile: every verb is proven read-only by the `.git`
    /// fingerprint test (architecture.md D.4).
    Read,
}

impl Profile {
    /// The profile's name, as it appears in `git info`'s capability report.
    pub fn as_str(&self) -> &'static str {
        match self {
            Profile::Read => "read",
        }
    }
}

/// A verb this build can execute.
///
/// `#[non_exhaustive]`, and it carries **only the verbs that are implemented**
/// — one, today. Architecture.md C.1 sketches the enum with all ten read
/// verbs, but a config that can enable `Verb::Status` in a build with no
/// status implementation would put a verb in `tools --json` that cannot run;
/// the schema is built from this enum (E.1), so an unimplemented variant is a
/// promise the tool cannot keep. Each phasing PR adds its own variant with
/// its implementation, and `#[non_exhaustive]` is what keeps that from being
/// a breaking change for embedders.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
#[non_exhaustive]
pub enum Verb {
    /// `git info` — what am I looking at, and what am I allowed to do.
    Info,
    /// `git status` — staged, unstaged, untracked, ignored, conflicted.
    Status,
}

impl Verb {
    /// Every verb this build knows how to execute.
    pub const ALL: &'static [Verb] = &[Verb::Info, Verb::Status];

    /// The verb's argv spelling — the word an agent types.
    pub fn as_str(&self) -> &'static str {
        match self {
            Verb::Info => "info",
            Verb::Status => "status",
        }
    }

    /// The profile that grants this verb.
    pub fn profile(&self) -> Profile {
        match self {
            Verb::Info | Verb::Status => Profile::Read,
        }
    }
}

/// Per-invocation caps on how much a verb may return.
///
/// Every field is a hard cap the embedder sets; a verb's own `--limit` may
/// only lower it. Truncation is always reported, never silent (E.5).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Limits {
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

impl Default for Limits {
    /// The C.2 defaults: an embedder that types nothing gets these.
    fn default() -> Self {
        Self {
            max_rows: 1000,
            max_diff_files: 500,
            max_blob_bytes: 8 * 1024 * 1024,
            max_hunk_bytes_per_file: 256 * 1024,
            submodule_depth: 1,
        }
    }
}

/// The embedder's decision about what this git tool may do.
///
/// Built by [`GitConfig::read_only`] and narrowed from there. Fields are
/// private so the only reachable transitions are the subtractive ones.
#[derive(Debug, Clone)]
#[non_exhaustive]
pub struct GitConfig {
    profiles: BTreeSet<Profile>,
    verbs: BTreeSet<Verb>,
    tool_name: String,
    limits: Limits,
}

impl GitConfig {
    /// The default, and the only constructor: the read profile, every read
    /// verb, tool name `git`, and the [`Limits`] defaults.
    ///
    /// Registering as `git` deliberately shadows external git — kaish
    /// resolves builtins before PATH, which is the point for an agent
    /// surface. Use [`GitConfig::with_tool_name`] if you want both.
    pub fn read_only() -> Self {
        Self {
            profiles: BTreeSet::from([Profile::Read]),
            verbs: Verb::ALL.iter().copied().collect(),
            tool_name: "git".to_string(),
            limits: Limits::default(),
        }
    }

    /// Subtract a verb from whatever the profiles allow.
    ///
    /// Subtraction only — there is no `with_verb`. Removing a verb removes it
    /// from the schema too, so it disappears from `tools --json`, from
    /// `help git`, and from completion, and the kernel has nothing to route
    /// to (E.1). Subtracting an already-absent verb is a no-op, not an error:
    /// an embedder narrowing a surface should not have to track which verbs
    /// this version happens to ship.
    pub fn without_verb(mut self, verb: Verb) -> Self {
        self.verbs.remove(&verb);
        self
    }

    /// Register under a name other than `git`.
    pub fn with_tool_name(mut self, name: impl Into<String>) -> Self {
        self.tool_name = name.into();
        self
    }

    /// Replace the output caps.
    pub fn with_limits(mut self, limits: Limits) -> Self {
        self.limits = limits;
        self
    }

    /// Whether this config enables `verb`.
    pub fn has(&self, verb: Verb) -> bool {
        self.verbs.contains(&verb)
    }

    /// The name this tool registers under.
    pub fn tool_name(&self) -> &str {
        &self.tool_name
    }

    /// The output caps.
    pub fn limits(&self) -> Limits {
        self.limits
    }

    /// The enabled profiles, in a stable order.
    pub fn profiles(&self) -> impl Iterator<Item = Profile> + '_ {
        self.profiles.iter().copied()
    }

    /// The enabled verbs, in a stable order.
    pub fn verbs(&self) -> impl Iterator<Item = Verb> + '_ {
        self.verbs.iter().copied()
    }
}

impl Default for GitConfig {
    fn default() -> Self {
        Self::read_only()
    }
}

/// A config an embedder built that cannot produce a working tool.
///
/// Loud at registration time rather than at the first invocation: an embedder
/// finds out while wiring the kernel, not when an agent runs a verb.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[non_exhaustive]
pub enum ConfigError {
    /// The tool name is empty or contains whitespace, so nothing could ever
    /// dispatch to it.
    #[error(
        "kaish-tools-git: tool name {name:?} is not a usable command word — \
         it must be non-empty and contain no whitespace"
    )]
    UnusableToolName {
        /// The rejected name.
        name: String,
    },

    /// Every verb was subtracted, leaving a tool with no schema leaves.
    #[error(
        "kaish-tools-git: every verb was subtracted from the config, so the \
         tool would register with nothing to run — drop the registration \
         instead"
    )]
    NoVerbsEnabled,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn read_only_enables_every_implemented_verb() {
        let cfg = GitConfig::read_only();
        for verb in Verb::ALL {
            assert!(cfg.has(*verb), "{verb:?} must be on by default");
        }
        assert_eq!(cfg.tool_name(), "git");
        assert_eq!(cfg.limits(), Limits::default());
    }

    /// C.2's numbers, spelled out. A silent drift in a cap is a silent change
    /// in how much an agent gets back.
    #[test]
    fn limits_defaults_match_the_design() {
        let l = Limits::default();
        assert_eq!(l.max_rows, 1000);
        assert_eq!(l.max_diff_files, 500);
        assert_eq!(l.max_blob_bytes, 8 * 1024 * 1024);
        assert_eq!(l.max_hunk_bytes_per_file, 256 * 1024);
        assert_eq!(l.submodule_depth, 1);
    }

    #[test]
    fn without_verb_subtracts_and_is_idempotent() {
        let cfg = GitConfig::read_only().without_verb(Verb::Info);
        assert!(!cfg.has(Verb::Info));
        let cfg = cfg.without_verb(Verb::Info);
        assert!(!cfg.has(Verb::Info), "subtracting twice must stay subtracted");
    }

    /// The subtractive property, asserted rather than assumed: no sequence of
    /// public calls may leave a config holding a verb it did not start with.
    #[test]
    fn no_public_method_can_widen_the_verb_set() {
        let start: BTreeSet<Verb> = GitConfig::read_only().verbs().collect();
        let narrowed = GitConfig::read_only()
            .without_verb(Verb::Info)
            .without_verb(Verb::Status)
            .with_tool_name("kgit")
            .with_limits(Limits {
                max_rows: 5,
                ..Limits::default()
            });
        let after: BTreeSet<Verb> = narrowed.verbs().collect();
        assert!(
            after.is_subset(&start),
            "config widened from {start:?} to {after:?}"
        );
        assert!(after.is_empty());
    }

    /// Every verb must name the profile that grants it, so `git info`'s
    /// capability report cannot claim a verb no enabled profile covers.
    #[test]
    fn every_verb_belongs_to_an_enabled_profile() {
        let cfg = GitConfig::read_only();
        let profiles: BTreeSet<Profile> = cfg.profiles().collect();
        for verb in cfg.verbs() {
            assert!(
                profiles.contains(&verb.profile()),
                "{verb:?} is enabled but its profile {:?} is not",
                verb.profile()
            );
        }
    }
}
