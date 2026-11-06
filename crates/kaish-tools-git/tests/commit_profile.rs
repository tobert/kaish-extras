//! The commit profile's scaffolding (phase 1a): what the config types do, and
//! what `git info` reports. No write verb exists yet, so none of these tests
//! writes; `write_fingerprint.rs` holds the refusal table.

#[path = "support.rs"]
mod support;

use std::sync::Arc;

use kaish_tool_api::Tool;
use kaish_tools_git::{
    GitConfig, GitHooks, HookContext, HookError, HookOutput, Identity, Profile, RewritePolicy, Verb,
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

/// Phase 1a enables no verb. Compared against a literal list, not against
/// another config built the same way: a mutation that adds a verb to the
/// commit profile (or maps one to `Profile::Commit`) breaks this, and
/// comparing `read_only()` to itself would not. Phase 1b changes the list
/// on purpose, verb by verb.
#[test]
fn the_commit_profile_enables_exactly_the_read_verbs_for_now() {
    let writable = GitConfig::read_only().with_commit_profile();
    let names: Vec<&str> = writable.read_config().verbs().map(|v| v.as_str()).collect();
    assert_eq!(
        names,
        ["info", "status", "log", "ls", "show", "diff", "branch", "tag", "worktree list"]
    );
    for verb in writable.read_config().verbs() {
        assert_eq!(verb.profile(), Profile::Read, "{verb:?} is not a read verb");
    }
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
    assert_eq!(
        commit["capabilities"]["verbs"], read["capabilities"]["verbs"],
        "no write verb yet, so the verb list is unchanged"
    );

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
