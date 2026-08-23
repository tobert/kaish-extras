//! A canary for kaish behaviors this crate's *callers* depend on, pinned so a
//! dependency bump that changes one fails here instead of surfacing as a
//! mystery in an embedder (docs/issues.md, X3).
//!
//! This is a different job from `router_kernel_drift.rs`. That test asks
//! whether the kernel dispatches to the verb we think it does. This one asks
//! whether the kernel still *treats a verb's result* the way the embedding
//! guide says it does — questions a compiler cannot ask, because nothing in
//! them is a type.
//!
//! The 0.16 bump is why the file exists, though not for the reason it first
//! looked like. 0.16 rewrote the rule for what `$(cmd)` binds, and reading the
//! changelog it seemed to move `x=$(git status)` from the typed model to the
//! text. Checking the source instead: this crate builds every result with
//! `ExecResult::with_output`, which leaves `.data` at `None`, so there was
//! never anything typed to bind and the answer was the same before and after.
//! The bump built clean, 25 test binaries green — a behavior change of that
//! shape would have arrived with no signal at all, which is the argument for
//! pinning the ones an embedder can see.
//!
//! Every case here drives a real `kaish_kernel::Kernel` with this crate's tool
//! registered, exactly as an embedder does, and every case carries a negative
//! control — an assertion that the *other* answer is reachable through the
//! same kernel in the same test. Without one, a case that pins an absence
//! passes just as well when the mechanism it queries is broken. That earned
//! its keep here immediately: the first draft wrote `typeof x` for `typeof $x`
//! and so typed the literal string `"x"`, which answers `string` no matter
//! what the kernel does. The git assertion passed for the wrong reason and
//! only the control caught it.

#[path = "support.rs"]
mod support;

use std::path::{Path, PathBuf};
use std::sync::Arc;

use kaish_kernel::{Kernel, KernelConfig};
use kaish_tool_api::KernelBackend;

use kaish_tools_git::GitConfig;

use support::{git, require_git, write_file, Fixture, StrictBackend};

/// A one-commit repository with a second, uncommitted file, so `git status`
/// has a non-empty text surface *and* a non-empty typed model. Both halves
/// matter: the binding rule turns on which of the two a substitution takes.
fn repo_with_a_change() -> (Fixture, PathBuf) {
    require_git();
    let fixture = Fixture::empty();
    let root = fixture.path("repo");
    std::fs::create_dir_all(&root).expect("create repo dir");
    git(&root, &["init", "--initial-branch=main", "--quiet"]);
    write_file(&root, "README.md", "hi\n");
    git(&root, &["add", "."]);
    git(&root, &["commit", "-m", "initial", "--quiet"]);
    write_file(&root, "untracked.txt", "new\n");
    (fixture, root)
}

async fn build_kernel(mount_real: PathBuf) -> Kernel {
    let backend: Arc<dyn KernelBackend> =
        Arc::new(StrictBackend::single(PathBuf::from("/mnt"), mount_real));
    let git = kaish_tools_git::tool(GitConfig::read_only()).expect("valid config");

    let mut kernel_cfg = KernelConfig::transient();
    kernel_cfg.cwd = PathBuf::from("/mnt");

    Kernel::with_backend(backend, kernel_cfg, |_| {}, |tools| tools.register(git))
        .expect("kernel assembles from a valid git tool")
}

/// Run a script through a kernel with the git tool registered, returning
/// trimmed stdout. A non-zero exit is a test failure, not a value to assert
/// on — every script here is expected to run.
async fn run(root: &Path, script: &str) -> String {
    let kernel = build_kernel(root.to_path_buf()).await;
    let result = kernel
        .execute(script)
        .await
        .unwrap_or_else(|e| panic!("kernel failed to execute {script:?}: {e}"));
    assert_eq!(
        result.code, 0,
        "{script:?} exited {}: {}",
        result.code, result.err
    );
    result.text_out().trim().to_string()
}

/// `x=$(git status)` binds the **text** git printed, not its typed model.
///
/// Two independent things hold this up, and it takes *both* to break it:
/// every verb builds its result with `ExecResult::with_output`, which leaves
/// `.data` at `None`, and the schema leaves `typed_substitution` at its
/// default `false`. Verified by mutation — setting the schema flag alone
/// changes nothing, because a substitution binds `.data` and there is none.
/// Attaching `.data` alone changes nothing either, since 0.16 binds it typed
/// only when the schema says so or the tool printed nothing else. Both
/// together turn this test red, which is the state it is guarding against.
///
/// Neither half is unthinkable: `success_with_data` is the natural reach for a
/// verb that wants a pipeline sideband (`git log | jq …`), and
/// `with_typed_substitution()` reads like an upgrade to anyone who has not
/// read why it was left off. `.data` here is a structured *view* of text the
/// verb already printed, which is kaish 0.16's stated test for leaving it off:
/// `git` names a real program, and an agent that wants the model asks `--json`.
///
/// The `fromjson` half is the negative control. It proves `typeof` can report
/// something other than `string` through this very kernel; without it, a kaish
/// that had broken typed substitution outright would pass this test.
#[tokio::test]
async fn a_git_substitution_binds_text_while_a_typed_one_still_binds_its_data() {
    let (_fixture, root) = repo_with_a_change();

    let git_bound = run(&root, "x=$(git status); typeof $x").await;
    assert_eq!(
        git_bound, "string",
        "a git substitution must bind the text git printed, not its typed \
         model — see AGENTS.md, \"Command substitution binds text\""
    );

    let typed_bound = run(&root, r#"y=$(echo '{"a":1}' | fromjson); typeof $y"#).await;
    assert_eq!(
        typed_bound, "record",
        "negative control: typed substitution must still reach a non-string \
         through this kernel, or the assertion above proves nothing"
    );
}

/// The text a `$(git …)` binds is the text the verb printed — not an empty
/// string, and not a JSON blob that happens to be typed `string`.
///
/// `a_git_substitution_binds_text_…` pins the *type*; a kaish that bound the
/// serialized model as a string would satisfy it and still hand an agent
/// something it did not ask for. This pins the *content* on both sides: the
/// bound text names the untracked file the way the table does, and does not
/// carry the JSON punctuation `--json` would.
#[tokio::test]
async fn the_bound_text_is_what_the_verb_printed_and_not_its_json() {
    let (_fixture, root) = repo_with_a_change();

    let bound = run(&root, "x=$(git status); echo $x").await;
    assert!(
        bound.contains("untracked.txt"),
        "the bound text must be the report git printed, which names the \
         untracked file; got: {bound:?}"
    );
    assert!(
        !bound.contains("\"entries\""),
        "the bound text must not be the JSON model — that is what --json is \
         for; got: {bound:?}"
    );

    // The control for the pair above: `--json` on the same verb in the same
    // kernel *does* produce the JSON, so `contains`/`!contains` are reading a
    // real difference rather than two spellings of the same output.
    let as_json = run(&root, "git status --json").await;
    assert!(
        as_json.contains("\"entries\""),
        "control: git status --json must carry the model, or the assertion \
         that the bound text lacks it is vacuous; got: {as_json:?}"
    );
}
