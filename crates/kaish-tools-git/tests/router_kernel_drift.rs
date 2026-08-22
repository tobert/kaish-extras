//! architecture.md E.1's router drift test: "the routing table and the
//! schema tree must not be able to disagree about which verbs exist."
//!
//! `src/tool.rs`'s `route()` is this crate's own re-implementation of the
//! rule the kernel applies when it dispatches a subcommand-aware tool: walk
//! leading positionals, descending while each one names a child of the
//! schema. Two independent readings of one rule can drift silently — the
//! schema could add, rename or fail to remove a verb in a way `route()`
//! still limps through while the real kernel routes differently (or vice
//! versa).
//!
//! The kernel's own routing function (`scheduler::pipeline::select_leaf`) is
//! `pub(crate)` to `kaish-kernel` and unreachable even as a dev-dependency —
//! consistent with this crate's production posture of depending on nothing
//! but `kaish-tool-api` + `kaish-types` (architecture.md G.2). So instead of
//! calling kernel internals, this test builds a real `kaish_kernel::Kernel`,
//! registers this crate's `Tool` exactly as an embedder would, and drives it
//! through `Kernel::execute` — the kernel's actual dispatch path, `select_leaf`
//! included, not a belief about what it does.
//!
//! Every case here is driven by `Verb::ALL`, never a hand-enumerated verb
//! list — the coordination rule with the sibling `git diff` PR: when
//! `Verb::Diff` lands, this test covers it with zero code changes.

#[path = "support.rs"]
mod support;

use std::path::PathBuf;
use std::sync::Arc;

use kaish_kernel::{Kernel, KernelConfig};
use kaish_tool_api::KernelBackend;

use kaish_tools_git::{GitConfig, Verb};

use support::{git, require_git, write_file, Fixture, StrictBackend};

/// A tiny one-commit repository — enough for every read verb to succeed with
/// no arguments, which is what lets a passing dispatch double as proof the
/// verb's own code ran (not just that no error surfaced).
fn simple_repo() -> (Fixture, PathBuf) {
    require_git();
    let fixture = Fixture::empty();
    let root = fixture.path("repo");
    std::fs::create_dir_all(&root).expect("create repo dir");
    git(&root, &["init", "--initial-branch=main", "--quiet"]);
    write_file(&root, "README.md", "hi\n");
    git(&root, &["add", "."]);
    git(&root, &["commit", "-m", "initial", "--quiet"]);
    (fixture, root)
}

/// A real kernel, mounting `mount_real` at `/mnt` and registering the git
/// tool under `cfg`. `KernelConfig::transient()`'s `vfs_mode` is ignored by
/// `Kernel::with_backend` (the embedder's backend owns path routing); `cwd`
/// and `skip_validation` are what carry over, so both are set explicitly.
async fn build_kernel(cfg: GitConfig, mount_real: PathBuf) -> Kernel {
    let backend: Arc<dyn KernelBackend> =
        Arc::new(StrictBackend::single(PathBuf::from("/mnt"), mount_real));
    let git = kaish_tools_git::tool(cfg).expect("valid config");

    let mut kernel_cfg = KernelConfig::transient();
    kernel_cfg.cwd = PathBuf::from("/mnt");
    // Validation runs its own schema walk ahead of dispatch; skipping it
    // isolates this test to `Kernel::execute`'s own routing rather than also
    // exercising the validator's.
    kernel_cfg.skip_validation = true;

    Kernel::with_backend(backend, kernel_cfg, |_| {}, |tools| tools.register(git))
        .expect("kernel assembles from a valid git tool")
}

/// For every verb `GitConfig::read_only()` enables, the kernel must actually
/// dispatch to it — not merely fail to error. A fresh one-commit repository
/// with no arguments is something every read verb answers successfully, so
/// `code == 0` here is only possible if the kernel's `select_leaf` walked
/// into the verb's own schema leaf and our `execute()` then matched the same
/// name in `route()`.
#[tokio::test]
async fn every_enabled_verb_is_dispatched_by_the_kernel() {
    let (_fixture, root) = simple_repo();

    for verb in Verb::ALL {
        let kernel = build_kernel(GitConfig::read_only(), root.clone()).await;
        let result = kernel
            .execute(&format!("git {}", verb.as_str()))
            .await
            .unwrap_or_else(|e| panic!("kernel failed to execute 'git {}': {e}", verb.as_str()));
        assert_eq!(
            result.code, 0,
            "git {} did not succeed through the kernel: {}",
            verb.as_str(),
            result.err
        );
    }
}

/// For every verb a config disables, the kernel must refuse it *before*
/// reaching our own belt-and-braces `VerbNotEnabled` guard (E.5, exit 5) —
/// the schema's absence, not a check inside the verb, is what makes it
/// unroutable. Alongside that refusal, every *other* verb this same config
/// still enables must keep dispatching correctly through the very same
/// kernel and schema: a check that only ever proves absence can pass
/// vacuously, so presence is asserted in the same breath ("Gates need
/// negative controls").
#[tokio::test]
async fn a_disabled_verb_is_refused_by_the_kernel_before_reaching_our_own_guard() {
    let (_fixture, root) = simple_repo();

    for disabled in Verb::ALL {
        let cfg = GitConfig::read_only().without_verb(*disabled);
        let kernel = build_kernel(cfg, root.clone()).await;

        let result = kernel
            .execute(&format!("git {}", disabled.as_str()))
            .await
            .unwrap_or_else(|e| {
                panic!("kernel failed to execute the disabled verb '{}': {e}", disabled.as_str())
            });
        assert_ne!(
            result.code, 0,
            "git {} unexpectedly succeeded with the verb disabled",
            disabled.as_str()
        );
        assert!(
            !result.err.contains("is not enabled by this build's profile"),
            "git {} was refused by our own VerbNotEnabled guard rather than \
             being genuinely unroutable — the schema still offered it to the \
             kernel's own dispatch: {}",
            disabled.as_str(),
            result.err
        );

        // Negative control: every other verb this config still enables must
        // keep working, in the same kernel, with the same schema.
        for enabled in Verb::ALL {
            if enabled == disabled {
                continue;
            }
            let result = kernel
                .execute(&format!("git {}", enabled.as_str()))
                .await
                .unwrap_or_else(|e| {
                    panic!("kernel failed to execute '{}': {e}", enabled.as_str())
                });
            assert_eq!(
                result.code, 0,
                "git {} should still dispatch with only {:?} disabled: {}",
                enabled.as_str(),
                disabled,
                result.err
            );
        }
    }
}

/// The other two surfaces the gate names: `tools --json` and `help git`. Both
/// are built by the kernel from the *same* `ToolSchema` `execute()` routes
/// against (`Kernel::assemble` calls `tools.schemas()` once at construction
/// and hands it to `help`'s renderer and to `BuiltinFs`'s `/v/bin` listing),
/// so this is a second, independent surface confirming what
/// `schema_carries_only_the_enabled_verbs` (src/tool.rs) already proves at
/// the schema level: a disabled verb is absent from what an agent is told
/// exists, not merely rejected once asked for by name.
#[tokio::test]
async fn a_disabled_verb_is_absent_from_help_and_a_negative_control_verb_is_present() {
    let (_fixture, root) = simple_repo();

    for disabled in Verb::ALL {
        let cfg = GitConfig::read_only().without_verb(*disabled);
        let kernel = build_kernel(cfg, root.clone()).await;

        let result = kernel
            .execute("help git")
            .await
            .expect("kernel executes 'help git'");
        assert_eq!(result.code, 0, "help git failed: {}", result.err);
        let text = result.text_out();

        // The needle is the command spelling, not the bare verb name. A bare
        // name is a substring of other text `help git` legitimately prints —
        // `--staged` contains "tag", which failed this test the day `Verb::Tag`
        // landed. Every example is `git <verb> ...`, so "git tag" is present
        // exactly when a tag example survived filtering, which is the property
        // this asserts.
        let spelling = |verb: &Verb| format!("git {}", verb.as_str());
        assert!(
            !text.contains(&spelling(disabled)),
            "help git must not mention the disabled verb {:?}: {text}",
            disabled
        );

        // Negative control: every other verb must still be named.
        for enabled in Verb::ALL {
            if enabled == disabled {
                continue;
            }
            assert!(
                text.contains(&spelling(enabled)),
                "help git dropped the still-enabled verb {:?} while {:?} was \
                 disabled: {text}",
                enabled,
                disabled
            );
        }
    }
}

/// `git info`'s `capabilities.verbs` (architecture.md B.1) is what an agent
/// reads to learn which verbs this build offers *without* discovering a
/// disabled one by being refused. That is only trustworthy if it cannot
/// diverge from the schema the kernel actually routes against — so this
/// pins the two against each other through a real kernel and real JSON
/// output, for both the full config and every one-verb-narrowed config, with
/// the same negative control as the other gates here.
#[tokio::test]
async fn info_capabilities_are_pinned_to_the_schema() {
    let (_fixture, root) = simple_repo();

    for disabled in std::iter::once(None).chain(Verb::ALL.iter().copied().map(Some)) {
        let cfg = match disabled {
            Some(v) => GitConfig::read_only().without_verb(v),
            None => GitConfig::read_only(),
        };
        // `info` itself must stay enabled to ask the question at all.
        if disabled == Some(Verb::Info) {
            continue;
        }
        let kernel = build_kernel(cfg, root.clone()).await;
        let result = kernel.execute("git info --json").await.expect("kernel executes git info");
        assert_eq!(result.code, 0, "git info --json failed: {}", result.err);

        let json: serde_json::Value =
            serde_json::from_str(&result.text_out()).expect("git info --json emits valid JSON");
        let reported: Vec<String> = json["capabilities"]["verbs"]
            .as_array()
            .expect("capabilities.verbs is an array")
            .iter()
            .map(|v| v.as_str().expect("verb names are strings").to_string())
            .collect();

        for verb in Verb::ALL {
            let should_be_present = Some(*verb) != disabled;
            assert_eq!(
                reported.contains(&verb.as_str().to_string()),
                should_be_present,
                "with {disabled:?} disabled, capabilities.verbs = {reported:?} \
                 disagrees with the schema about {verb:?}"
            );
        }
    }
}
