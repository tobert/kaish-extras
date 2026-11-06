//! The commit profile's scaffolding (phase 1a): what the config types do, and
//! what `git info` reports. No write verb exists yet, so none of these tests
//! writes; `write_fingerprint.rs` holds the refusal table.

#[path = "support.rs"]
mod support;

use std::collections::BTreeSet;
use std::sync::Arc;

use kaish_tool_api::Tool;
use kaish_tools_git::{
    GitConfig, GitHooks, HookContext, HookError, HookOutput, Identity, Profile, RewritePolicy,
    Verb,
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

/// Phase 1a enables no verb: the commit profile's verb set is the read set.
/// Phase 1b changes this assertion on purpose, verb by verb.
#[test]
fn the_commit_profile_enables_no_write_verb_yet() {
    let read: BTreeSet<Verb> = GitConfig::read_only().verbs().collect();
    let writable: BTreeSet<Verb> =
        GitConfig::read_only().with_commit_profile().read_config().verbs().collect();
    assert_eq!(read, writable);
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

#[test]
fn rewrite_policy_has_the_three_designed_settings() {
    // Compile-time exhaustiveness is off (non_exhaustive), so pin the names.
    let all = [RewritePolicy::Refuse, RewritePolicy::Unpublished, RewritePolicy::Allow];
    assert_eq!(all.len(), 3);
    assert_eq!(RewritePolicy::default(), RewritePolicy::Unpublished);
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
