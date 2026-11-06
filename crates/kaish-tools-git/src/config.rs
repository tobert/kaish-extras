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
//! With the `commit` feature, [`GitConfig::with_commit_profile`] returns a
//! [`WritableGitConfig`] — a different type, which is the only way to reach
//! the write settings. It is still subtractive for verbs.
//!
//! One knob is not subtractive and is not about verbs:
//! [`GitConfig::with_patch_renderer`] (`textdiff` only) replaces the text
//! `git diff --patch` prints. It adds no verb, no flag, and no read the caps
//! would refuse: the renderer receives content read under `max_blob_bytes`.
//!
//! This module depends on nothing of kaish's, and on nothing else of ours
//! but `renderer.rs`, which depends on nothing — the dependency direction in
//! A.1 that keeps a future `kaish-git-core` split mechanical.

use std::collections::BTreeSet;
#[cfg(any(feature = "textdiff", feature = "commit"))]
use std::sync::Arc;

#[cfg(feature = "commit")]
use crate::hooks::GitHooks;

#[cfg(feature = "textdiff")]
use crate::renderer::PatchRenderer;

/// A capability profile: the set of verbs an embedder is opting into.
///
/// `#[non_exhaustive]`. `Profile::Commit` exists only in a build with the
/// `commit` cargo feature, and only a [`WritableGitConfig`] (from
/// [`GitConfig::with_commit_profile`]) carries it. `Profile::Worktree` does
/// not exist yet. In a build without `commit` there is nothing to name for
/// "this build can write".
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
#[non_exhaustive]
pub enum Profile {
    /// The read profile: every verb is proven read-only by the `.git`
    /// fingerprint test (architecture.md D.4).
    Read,
    /// The commit profile (docs/design/git-write-profile.md). Phase 1b
    /// enables the index verbs: `add`, `rm`, `mv`, `restore --staged` and
    /// `reset` with no revision. `commit` is not built yet.
    #[cfg(feature = "commit")]
    Commit,
}

impl Profile {
    /// The profile's name, as it appears in `git info`'s capability report.
    pub fn as_str(&self) -> &'static str {
        match self {
            Profile::Read => "read",
            #[cfg(feature = "commit")]
            Profile::Commit => "commit",
        }
    }
}

/// A verb this build can execute.
///
/// `#[non_exhaustive]`, and it carries **only the verbs that are implemented**
/// — nine, as of PR 7. Architecture.md C.1 sketches the enum with all ten read
/// verbs (`blame` is the one still unbuilt — see `docs/issues.md`), but a config that can enable `Verb::Status` in a build with no
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
    /// `git log` — commit history from a starting revision.
    Log,
    /// `git ls` — a tree listing at a revision.
    Ls,
    /// `git show` — a commit, tag, tree, or blob at a revision.
    Show,
    /// `git diff` — the typed change model between two ends of the repository.
    Diff,
    /// `git branch` — the branch listing, local and remote-tracking.
    Branch,
    /// `git tag` — the tag listing, lightweight and annotated.
    Tag,
    /// `git worktree list` — every working tree this repository has.
    ///
    /// The read half of `worktree`, and the only half in the read profile:
    /// enumeration reads the registrations under `<common>/worktrees/` and
    /// writes nothing. Create, remove, lock and prune wait on the ledger
    /// (architecture.md B.11).
    WorktreeList,
    /// `git add` — stage working-tree content (commit profile).
    #[cfg(feature = "commit")]
    Add,
    /// `git rm` — remove paths from the index, and from the working tree
    /// unless `--cached` (commit profile).
    #[cfg(feature = "commit")]
    Rm,
    /// `git mv` — rename a tracked path in the working tree and the index
    /// (commit profile).
    #[cfg(feature = "commit")]
    Mv,
    /// `git restore --staged` — put the index entries for paths back to
    /// HEAD's (commit profile). `restore` without `--staged` would write file
    /// content into the working tree and refuses (git-write-profile.md W6).
    #[cfg(feature = "commit")]
    Restore,
    /// `git reset` with no revision — unstage paths (commit profile).
    #[cfg(feature = "commit")]
    Reset,
}

impl Verb {
    /// Every verb this build knows how to execute.
    pub const ALL: &'static [Verb] = &[
        Verb::Info,
        Verb::Status,
        Verb::Log,
        Verb::Ls,
        Verb::Show,
        Verb::Diff,
        Verb::Branch,
        Verb::Tag,
        Verb::WorktreeList,
    ];

    /// The verbs the commit profile adds, in the order phase 1b built them.
    /// Not part of [`Verb::ALL`]: the read profile never has them, and
    /// `GitConfig::read_only` publishes exactly `ALL`.
    #[cfg(feature = "commit")]
    pub const WRITE: &'static [Verb] = &[Verb::Add, Verb::Rm, Verb::Mv, Verb::Restore, Verb::Reset];

    /// Every verb this build knows, read and write.
    pub fn known() -> impl Iterator<Item = Verb> {
        #[cfg(feature = "commit")]
        let write = Verb::WRITE.iter().copied();
        #[cfg(not(feature = "commit"))]
        let write = std::iter::empty();
        Verb::ALL.iter().copied().chain(write)
    }

    /// The verb's argv spelling — the words an agent types after `git`.
    ///
    /// Two words for `worktree list`, because `worktree` is a schema node
    /// with subcommands rather than a leaf: the read profile has one of them
    /// and B.11's write profiles add the rest under the same node.
    pub fn as_str(&self) -> &'static str {
        match self {
            Verb::Info => "info",
            Verb::Status => "status",
            Verb::Log => "log",
            Verb::Ls => "ls",
            Verb::Show => "show",
            Verb::Diff => "diff",
            Verb::Branch => "branch",
            Verb::Tag => "tag",
            Verb::WorktreeList => "worktree list",
            #[cfg(feature = "commit")]
            Verb::Add => "add",
            #[cfg(feature = "commit")]
            Verb::Rm => "rm",
            #[cfg(feature = "commit")]
            Verb::Mv => "mv",
            #[cfg(feature = "commit")]
            Verb::Restore => "restore",
            #[cfg(feature = "commit")]
            Verb::Reset => "reset",
        }
    }

    /// The profile that grants this verb.
    pub fn profile(&self) -> Profile {
        match self {
            Verb::Info
            | Verb::Status
            | Verb::Log
            | Verb::Ls
            | Verb::Show
            | Verb::Diff
            | Verb::Branch
            | Verb::Tag
            | Verb::WorktreeList => Profile::Read,
            #[cfg(feature = "commit")]
            Verb::Add | Verb::Rm | Verb::Mv | Verb::Restore | Verb::Reset => Profile::Commit,
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
    /// Maximum bytes of file content one `git diff --patch` call hands an
    /// embedder's patch renderer, summed over every file (`textdiff` only;
    /// not consulted without a renderer, whose built-in counterpart holds one
    /// file at a time).
    ///
    /// Files are filled in path order. The first file whose content would
    /// take the total past this budget, and every file after it, arrives
    /// with no content and with [`crate::Withheld::OverMaxPatchContentBytes`]
    /// as the reason. Two sides that are the same blob count once.
    ///
    /// The default, 64 MiB, is eight `max_blob_bytes`: room for eight files
    /// at the per-file cap, or a change set of several hundred ordinary
    /// source files, whole. Without it the bound was `max_diff_files` × 2 ×
    /// `max_blob_bytes` — 8 GiB at the defaults — for one call.
    pub max_patch_content_bytes: u64,
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
            max_patch_content_bytes: 64 * 1024 * 1024,
            submodule_depth: 1,
        }
    }
}

/// The embedder's decision about what this git tool may do.
///
/// Built by [`GitConfig::read_only`] and narrowed from there. Fields are
/// private so the only reachable transitions are the subtractive ones.
#[derive(Clone)]
#[non_exhaustive]
pub struct GitConfig {
    profiles: BTreeSet<Profile>,
    verbs: BTreeSet<Verb>,
    tool_name: String,
    limits: Limits,
    #[cfg(feature = "textdiff")]
    patch_renderer: Option<Arc<dyn PatchRenderer>>,
}

// By hand because a renderer is not `Debug`: the trait asks only for
// `Send + Sync`, and an embedder's engine should not have to grow a `Debug`
// impl to be installed.
impl std::fmt::Debug for GitConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let mut s = f.debug_struct("GitConfig");
        s.field("profiles", &self.profiles)
            .field("verbs", &self.verbs)
            .field("tool_name", &self.tool_name)
            .field("limits", &self.limits);
        #[cfg(feature = "textdiff")]
        s.field(
            "patch_renderer",
            &self.patch_renderer.as_ref().map(|_| "<installed>"),
        );
        s.finish()
    }
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
            #[cfg(feature = "textdiff")]
            patch_renderer: None,
        }
    }

    /// Enable the commit profile.
    ///
    /// Returns a different type, [`WritableGitConfig`], so a program that
    /// never names it cannot hold a config that writes. The name says what it
    /// enables and carries no authorization: per-command approval stays with
    /// the embedder, through kaish `plan` (git-write-profile.md §2).
    ///
    /// Phase 1b adds the index verbs (`Verb::WRITE`): `add`, `rm`, `mv`,
    /// `restore --staged` and `reset` with no revision. A read verb the
    /// config already subtracted stays subtracted.
    #[cfg(feature = "commit")]
    pub fn with_commit_profile(mut self) -> WritableGitConfig {
        self.profiles.insert(Profile::Commit);
        self.verbs.extend(Verb::WRITE.iter().copied());
        WritableGitConfig {
            read: self,
            write: WriteSettings {
                rewrite_policy: RewritePolicy::default(),
                hooks: None,
                identity: None,
            },
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

    /// Render `git diff --patch`'s text with `renderer` instead of this
    /// build's unified-patch renderer. Installing a second one replaces the
    /// first.
    ///
    /// Changes the text only. `--json` still reports this build's own model
    /// and hunks and never calls the renderer; plain `git diff` never calls
    /// it either. The renderer receives content read under
    /// `max_blob_bytes` and within the call's `max_patch_content_bytes`,
    /// and nothing past either — see [`crate::ChangedFile::withheld`]. It
    /// costs one read the built-in renderer skips: a renamed file's blob.
    /// With no renderer installed, `--patch` prints this build's own patch
    /// text.
    #[cfg(feature = "textdiff")]
    pub fn with_patch_renderer(mut self, renderer: Arc<dyn PatchRenderer>) -> Self {
        self.patch_renderer = Some(renderer);
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

    /// The installed patch renderer, if any.
    #[cfg(feature = "textdiff")]
    pub(crate) fn patch_renderer(&self) -> Option<&Arc<dyn PatchRenderer>> {
        self.patch_renderer.as_ref()
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

/// When a write may replace a commit that exists
/// (git-write-profile.md §3).
///
/// Phase 1a defines the setting and nothing reads it yet: no verb moves a
/// branch to a commit that is not a descendant of where it was.
#[cfg(feature = "commit")]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
#[non_exhaustive]
pub enum RewritePolicy {
    /// Never replace a commit that exists. `--amend` exits 2 (usage).
    Refuse,
    /// Replace a commit only when no `refs/remotes/*` ref reaches it. This
    /// is the case that can never need a force push. The default.
    #[default]
    Unpublished,
    /// Replace any commit on the current branch.
    Allow,
}

/// A name and email for a commit's author or committer line.
///
/// The time is not part of it: git takes it from the clock or from
/// `GIT_AUTHOR_DATE` / `GIT_COMMITTER_DATE` (git-write-profile.md §5.4).
#[cfg(feature = "commit")]
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Identity {
    name: String,
    email: String,
}

#[cfg(feature = "commit")]
impl Identity {
    /// An identity. Neither field may contain `<`, `>`, or a newline: git
    /// strips them from a commit line, and this crate refuses them instead of
    /// writing a different identity from the one given.
    pub fn new(name: impl Into<String>, email: impl Into<String>) -> Result<Self, ConfigError> {
        let (name, email) = (name.into(), email.into());
        let bad = |s: &str| s.chars().any(|c| matches!(c, '<' | '>' | '\n' | '\r' | '\0'));
        if name.trim().is_empty() || bad(&name) || bad(&email) {
            return Err(ConfigError::UnusableIdentity { name, email });
        }
        Ok(Self { name, email })
    }

    /// The name.
    pub fn name(&self) -> &str {
        &self.name
    }

    /// The email.
    pub fn email(&self) -> &str {
        &self.email
    }
}

/// A [`GitConfig`] with the commit profile enabled.
///
/// Built only by [`GitConfig::with_commit_profile`]. It wraps the read config
/// it came from, so the read verbs, tool name and limits are the ones already
/// chosen, and adds the settings only the write side reads. The only change
/// to the verb set is still subtraction.
#[cfg(feature = "commit")]
#[derive(Clone)]
#[non_exhaustive]
pub struct WritableGitConfig {
    read: GitConfig,
    write: WriteSettings,
}

/// The settings only the write side reads.
#[cfg(feature = "commit")]
#[derive(Clone)]
pub(crate) struct WriteSettings {
    pub(crate) rewrite_policy: RewritePolicy,
    pub(crate) hooks: Option<Arc<dyn GitHooks>>,
    pub(crate) identity: Option<(Identity, Identity)>,
}

#[cfg(feature = "commit")]
impl std::fmt::Debug for WritableGitConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("WritableGitConfig")
            .field("read", &self.read)
            .field("rewrite_policy", &self.write.rewrite_policy)
            .field("hooks", &self.write.hooks.as_ref().map(|_| "<installed>"))
            .field("identity", &self.write.identity)
            .finish()
    }
}

#[cfg(feature = "commit")]
impl WritableGitConfig {
    /// Set when a write may replace a commit that exists. The default is
    /// [`RewritePolicy::Unpublished`].
    pub fn with_rewrite_policy(mut self, policy: RewritePolicy) -> Self {
        self.write.rewrite_policy = policy;
        self
    }

    /// Install a hook provider. Without one, a write refuses when the
    /// repository has a hook git would run for it (git-write-profile.md §5.1).
    /// Installing a second replaces the first.
    pub fn with_hooks(mut self, hooks: Arc<dyn GitHooks>) -> Self {
        self.write.hooks = Some(hooks);
        self
    }

    /// Pin the author and committer. When set, kernel variables are not read
    /// for identity (git-write-profile.md §5.4).
    pub fn with_identity(mut self, author: Identity, committer: Identity) -> Self {
        self.write.identity = Some((author, committer));
        self
    }

    /// Subtract a verb. Same rule as [`GitConfig::without_verb`]: there is no
    /// way to add one back.
    pub fn without_verb(mut self, verb: Verb) -> Self {
        self.read = self.read.without_verb(verb);
        self
    }

    /// The read config this was built from, with `Profile::Commit` added.
    pub fn read_config(&self) -> &GitConfig {
        &self.read
    }

    /// The rewrite policy.
    pub fn rewrite_policy(&self) -> RewritePolicy {
        self.write.rewrite_policy
    }

    /// The pinned identity, `(author, committer)`, if any.
    pub fn identity(&self) -> Option<&(Identity, Identity)> {
        self.write.identity.as_ref()
    }

    /// Split into the read config and the write settings.
    pub(crate) fn into_parts(self) -> (GitConfig, WriteSettings) {
        (self.read, self.write)
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

    /// An identity with an empty name, or a name or email that contains `<`,
    /// `>`, or a line break. Git would write a different identity from the
    /// one given, so this crate refuses it.
    #[cfg(feature = "commit")]
    #[error(
        "kaish-tools-git: identity name {name:?} / email {email:?} is not \
         usable — the name must be non-empty and neither may contain '<', \
         '>', or a line break"
    )]
    UnusableIdentity {
        /// The rejected name.
        name: String,
        /// The rejected email.
        email: String,
    },
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(feature = "textdiff")]
    struct NullRenderer;

    #[cfg(feature = "textdiff")]
    impl PatchRenderer for NullRenderer {
        fn render(
            &self,
            _files: &[crate::ChangedFile<'_>],
            _opts: &crate::PatchOptions,
        ) -> Result<crate::RenderedPatch, crate::PatchRenderError> {
            Ok(crate::RenderedPatch::new(""))
        }
    }

    /// A renderer on a config that had every verb leaves every verb, and one
    /// on a narrowed config leaves it narrowed: the knob is about text, not
    /// about the verb set.
    #[cfg(feature = "textdiff")]
    #[test]
    fn a_patch_renderer_does_not_change_the_verb_set() {
        let full: BTreeSet<Verb> = GitConfig::read_only().verbs().collect();
        let with: BTreeSet<Verb> = GitConfig::read_only()
            .with_patch_renderer(std::sync::Arc::new(NullRenderer))
            .verbs()
            .collect();
        assert_eq!(full, with);
        let narrowed: BTreeSet<Verb> = GitConfig::read_only()
            .without_verb(Verb::Diff)
            .with_patch_renderer(std::sync::Arc::new(NullRenderer))
            .verbs()
            .collect();
        assert!(!narrowed.contains(&Verb::Diff), "installing a renderer must not restore diff");
    }

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
        assert_eq!(l.max_patch_content_bytes, 64 * 1024 * 1024);
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
            .without_verb(Verb::Log)
            .without_verb(Verb::Ls)
            .without_verb(Verb::Show)
            .without_verb(Verb::Diff)
            .without_verb(Verb::Branch)
            .without_verb(Verb::Tag)
            .without_verb(Verb::WorktreeList)
            .with_tool_name("kgit")
            .with_limits(Limits {
                max_rows: 5,
                ..Limits::default()
            });
        // Every non-subtractive knob goes through the chain too: a renderer
        // replaces text, and must not be a side door to a verb.
        #[cfg(feature = "textdiff")]
        let narrowed = narrowed.with_patch_renderer(std::sync::Arc::new(NullRenderer));
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
