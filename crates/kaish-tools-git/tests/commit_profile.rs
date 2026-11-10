//! The commit profile's scaffolding (phase 1a): what the config types do, and
//! what `git info` reports. No write verb exists yet, so none of these tests
//! writes; `write_fingerprint.rs` holds the refusal table.

#[path = "support.rs"]
mod support;

use std::collections::BTreeSet;
use std::sync::Arc;

use kaish_tool_api::Tool;
use kaish_tools_git::{
    GitConfig, GitHooks, HookContext, HookError, HookOutput, Identity, Profile, RefTransactionState, RefUpdate, RewritePolicy,
    RewrittenCommit, Verb,
};

use support::{RichRepo, StrictBackend, TestCtx};

struct NoHooks;

impl GitHooks for NoHooks {
    fn pre_commit(&self, _: &HookContext<'_>) -> Result<HookOutput, HookError> {
        Ok(HookOutput::none())
    }
    fn commit_msg(&self, _: &HookContext<'_>, _: &mut String) -> Result<HookOutput, HookError> {
        Ok(HookOutput::none())
    }
    fn post_commit(&self, _: &HookContext<'_>, _: &str) -> HookOutput {
        HookOutput::none()
    }

    // This suite is about commit's own hooks; the ref-update hooks are pinned
    // in tests/write_hooks.rs, so here they only agree to everything.
    fn reference_transaction(&self, _: &HookContext<'_>, _: RefTransactionState, _: &[RefUpdate]) -> Result<HookOutput, HookError> {
        Ok(HookOutput::none())
    }
    fn post_checkout(&self, _: &HookContext<'_>, _: &str, _: &str, _: bool) -> HookOutput {
        HookOutput::none()
    }
    fn post_rewrite(&self, _: &HookContext<'_>, _: &str, _: &[RewrittenCommit]) -> HookOutput {
        HookOutput::none()
    }
}

#[test]
fn the_commit_profile_is_added_by_name_and_only_by_name() {
    let read: Vec<Profile> = GitConfig::read_only().profiles().collect();
    assert_eq!(read, [Profile::Read]);

    let writable = GitConfig::read_only().with_commit_profile();
    let profiles: Vec<Profile> = writable.read_config().profiles().collect();
    assert_eq!(profiles, [Profile::Read, Profile::Commit]);
    assert_eq!(Profile::Commit.as_str(), "commit");
}

/// The commit profile adds exactly the write verbs, and the read profile
/// never has them. Each later phase changes this assertion on purpose.
#[test]
fn the_commit_profile_adds_exactly_the_write_verbs() {
    let read: BTreeSet<Verb> = GitConfig::read_only().verbs().collect();
    assert_eq!(read, Verb::ALL.iter().copied().collect::<BTreeSet<_>>(), "the read profile publishes exactly Verb::ALL");
    for v in Verb::WRITE {
        assert!(!read.contains(v), "{v:?} must not be in the read profile");
    }
    let writable: BTreeSet<Verb> =
        GitConfig::read_only().with_commit_profile().read_config().verbs().collect();
    let want: BTreeSet<Verb> = read.iter().chain(Verb::WRITE.iter()).copied().collect();
    assert_eq!(writable, want);
    let names: Vec<&str> = Verb::WRITE.iter().map(|v| v.as_str()).collect();
    assert_eq!(names, ["add", "rm", "mv", "restore", "reset", "commit", "var", "switch", "checkout"]);
    for v in Verb::WRITE {
        assert_eq!(v.profile(), Profile::Commit);
    }
}

/// A verb subtracted before the profile is installed stays subtracted, and
/// installing the profile does not bring back a read verb.
#[test]
fn installing_the_profile_does_not_restore_a_subtracted_verb() {
    let w = GitConfig::read_only().without_verb(Verb::Log).with_commit_profile();
    assert!(!w.read_config().has(Verb::Log));
    assert!(w.read_config().has(Verb::Add));
    let w = w.without_verb(Verb::Add);
    assert!(!w.read_config().has(Verb::Add));
}

#[test]
fn the_write_settings_default_and_set() {
    let w = GitConfig::read_only().with_commit_profile();
    assert_eq!(w.rewrite_policy(), RewritePolicy::Unpublished, "the default is Unpublished");
    assert!(w.identity().is_none());

    let a = Identity::new("Ada", "ada@example.com").expect("identity");
    let c = Identity::new("Bot", "bot@example.com").expect("identity");
    let w = w
        .with_rewrite_policy(RewritePolicy::Refuse)
        .with_identity(a.clone(), c.clone())
        .with_hooks(Arc::new(NoHooks));
    assert_eq!(w.rewrite_policy(), RewritePolicy::Refuse);
    assert_eq!(w.identity(), Some(&(a, c)));
    let debug = format!("{w:?}");
    assert!(debug.contains("<installed>"), "hooks are not Debug, and must not be required to be: {debug}");
}

#[test]
fn without_verb_is_still_subtractive_on_the_writable_config() {
    let w = GitConfig::read_only().with_commit_profile().without_verb(Verb::Log);
    assert!(!w.read_config().has(Verb::Log));
    assert!(w.read_config().has(Verb::Status));
}

#[test]
fn an_identity_git_would_rewrite_is_refused() {
    for (name, email) in [
        ("", "a@b"),
        ("  ", "a@b"),
        ("A <B>", "a@b"),
        ("A", "a@b>"),
        ("A\nB", "a@b"),
        ("A", "a\r@b"),
    ] {
        assert!(Identity::new(name, email).is_err(), "{name:?} {email:?}");
    }
    assert!(Identity::new("Ada Lovelace", "ada@example.com").is_ok());
}

async fn info(repo: &RichRepo, config: kaish_tools_git::ToolConfig) -> serde_json::Value {
    let backend = Arc::new(StrictBackend::single(std::path::PathBuf::from("/mnt"), repo.scratch()));
    let mut ctx = TestCtx::new(backend, "/mnt/main");
    let git = kaish_tools_git::tool(config).expect("config builds");
    let result = git.execute(support::verb_words("info", &["--json"]), &mut ctx).await;
    assert_eq!(result.code, 0, "{}", result.err);
    result.output().and_then(|o| o.rich_json.clone()).expect("--json carries the model")
}

#[tokio::test]
async fn git_info_shows_the_profile() {
    let repo = RichRepo::build();

    let read = info(&repo, GitConfig::read_only().into()).await;
    assert_eq!(read["capabilities"]["profiles"], serde_json::json!(["read"]));

    let commit = info(&repo, GitConfig::read_only().with_commit_profile().into()).await;
    assert_eq!(commit["capabilities"]["profiles"], serde_json::json!(["read", "commit"]));
    let verbs = |j: &serde_json::Value| -> Vec<String> {
        j["capabilities"]["verbs"]
            .as_array()
            .expect("verbs")
            .iter()
            .map(|v| v.as_str().expect("name").to_string())
            .collect()
    };
    let (read_verbs, commit_verbs) = (verbs(&read), verbs(&commit));
    for v in Verb::WRITE {
        assert!(!read_verbs.contains(&v.as_str().to_string()), "{v:?} in the read report");
        assert!(commit_verbs.contains(&v.as_str().to_string()), "{v:?} missing from the commit report");
    }
    for v in &read_verbs {
        assert!(commit_verbs.contains(v), "the commit profile lost {v}");
    }

    let features = |j: &serde_json::Value| -> Vec<String> {
        j["capabilities"]["features"]
            .as_array()
            .expect("features")
            .iter()
            .map(|v| v.as_str().expect("name").to_string())
            .collect()
    };
    assert!(features(&commit).contains(&"commit".to_string()), "{commit}");
}

/// F4: the profile `git info` reports comes from the same source a write
/// would be gated on, the presence of the write settings, not from the
/// profile set on the read config. `read_config()` hands out a `GitConfig`
/// that lists `Commit`; a tool built from a clone of it has no write
/// settings, so it cannot write and must not say it can.
#[tokio::test]
async fn a_tool_built_from_a_cloned_read_config_reports_read_only() {
    let repo = RichRepo::build();
    let writable = GitConfig::read_only().with_commit_profile();
    let clone = writable.read_config().clone();
    assert!(
        clone.profiles().any(|p| p == Profile::Commit),
        "the premise: the clone's own profile set lists Commit"
    );

    let from_clone = info(&repo, clone.into()).await;
    assert_eq!(
        from_clone["capabilities"]["profiles"],
        serde_json::json!(["read"]),
        "no write settings behind this tool, so it is a read tool"
    );
    // The control: the tool built from the writable config itself says commit.
    let from_writable = info(&repo, writable.into()).await;
    assert_eq!(from_writable["capabilities"]["profiles"], serde_json::json!(["read", "commit"]));
}

/// `tool()` takes either config, and a bad tool name is refused for both.
#[test]
fn tool_accepts_both_configs_and_validates_both() {
    kaish_tools_git::tool(GitConfig::read_only()).expect("read-only");
    kaish_tools_git::tool(GitConfig::read_only().with_commit_profile()).expect("writable");
    assert!(kaish_tools_git::tool(GitConfig::read_only().with_tool_name("a b")).is_err());
    assert!(
        kaish_tools_git::tool(GitConfig::read_only().with_tool_name("a b").with_commit_profile())
            .is_err()
    );
}

fn leaf<'a>(schema: &'a kaish_types::ToolSchema, name: &str) -> &'a kaish_types::ToolSchema {
    schema.subcommands.iter().find(|c| c.name == name).unwrap_or_else(|| panic!("no leaf {name}"))
}

/// Each write leaf declares the effect ids of design §2, the root lists their
/// union, and the read profile declares none anywhere.
#[test]
fn write_leaves_declare_operations_and_the_root_lists_the_union() {
    let schema = kaish_tools_git::tool(GitConfig::read_only().with_commit_profile()).expect("tool").schema();
    let ops = |name: &str| leaf(&schema, name).operations.clone();
    assert_eq!(ops("add"), ["git.index"]);
    assert_eq!(ops("restore"), ["git.index"]);
    assert_eq!(ops("reset"), ["git.index", "git.ref", "git.rewrite"]);
    assert_eq!(ops("rm"), ["git.index", "fs.remove"]);
    assert_eq!(ops("mv"), ["git.index", "fs.rename"]);
    assert_eq!(ops("commit"), ["git.index", "git.commit", "git.ref", "git.rewrite"]);
    assert!(ops("var").is_empty(), "var reads an identity and writes nothing");
    // The leaves that also list declare what their creating spellings do.
    for creating in ["branch", "tag", "switch", "checkout"] {
        assert_eq!(ops(creating), ["git.ref"], "{creating}");
    }
    let mut root = schema.operations.clone();
    root.sort();
    assert_eq!(root, ["fs.remove", "fs.rename", "git.commit", "git.index", "git.ref", "git.rewrite"]);
    for read in ["info", "status", "log", "ls", "show", "diff"] {
        assert!(ops(read).is_empty(), "{read} must declare no effect");
    }

    let read_only = kaish_tools_git::tool(GitConfig::read_only()).expect("tool").schema();
    assert!(read_only.operations.is_empty());
    assert!(read_only.subcommands.iter().all(|c| c.operations.is_empty()));
}

/// A subtracted write verb takes its operations out of the root union.
#[test]
fn a_subtracted_write_verb_leaves_the_union() {
    let schema = kaish_tools_git::tool(GitConfig::read_only().with_commit_profile().without_verb(Verb::Rm).without_verb(Verb::Mv))
        .expect("tool")
        .schema();
    let mut root = schema.operations.clone();
    root.sort();
    assert_eq!(root, ["git.commit", "git.index", "git.ref", "git.rewrite"]);
    assert!(schema.subcommands.iter().all(|c| c.name != "rm" && c.name != "mv"));
    // With commit gone as well, reset still moves the branch.
    let schema = kaish_tools_git::tool(
        GitConfig::read_only().with_commit_profile().without_verb(Verb::Rm).without_verb(Verb::Mv).without_verb(Verb::Commit),
    )
    .expect("tool")
    .schema();
    let mut root = schema.operations.clone();
    root.sort();
    assert_eq!(root, ["git.index", "git.ref", "git.rewrite"]);
    // With reset gone too, the ref-creating leaves still declare git.ref.
    let without_reset = GitConfig::read_only()
        .with_commit_profile()
        .without_verb(Verb::Rm)
        .without_verb(Verb::Mv)
        .without_verb(Verb::Commit)
        .without_verb(Verb::Reset);
    let schema = kaish_tools_git::tool(without_reset.clone()).expect("tool").schema();
    let mut root = schema.operations.clone();
    root.sort();
    assert_eq!(root, ["git.index", "git.ref"]);
    // And with those gone as well, only the index verbs' id is left.
    let schema = kaish_tools_git::tool(
        without_reset
            .without_verb(Verb::Branch)
            .without_verb(Verb::Tag)
            .without_verb(Verb::Switch)
            .without_verb(Verb::Checkout),
    )
    .expect("tool")
    .schema();
    assert_eq!(schema.operations, ["git.index"]);
}

/// The description a tool with write verbs publishes must not claim to be
/// read-only, and must say what it never does.
#[test]
fn the_commit_profile_description_is_not_the_read_only_one() {
    let w = kaish_tools_git::tool(GitConfig::read_only().with_commit_profile()).expect("tool").schema();
    assert!(!w.description.contains("read-only by construction"), "{}", w.description);
    for must in ["never runs hooks", "user-level git configuration", "add, rm, mv, restore --staged, reset and commit", "branch <name>, tag <name>, switch -c and checkout -b", "rewrite policy", "reset --hard, --merge and --keep are refused", "git var GIT_AUTHOR_IDENT"] {
        assert!(w.description.contains(must), "{must:?} missing: {}", w.description);
    }
    let r = kaish_tools_git::tool(GitConfig::read_only()).expect("tool").schema();
    assert!(r.description.contains("read-only by construction"));
}
