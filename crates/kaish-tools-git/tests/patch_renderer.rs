//! The patch renderer boundary: an embedder's [`PatchRenderer`] takes over
//! `git diff --patch`'s text (docs/embedding-git.md, "Rendering `--patch`
//! yourself").
//!
//! Every case drives a real `kaish_kernel::Kernel` with the tool registered
//! the way an embedder registers it, because the two things this boundary
//! promises an embedder are both about what the kernel does with a result:
//! what stdout shows, and what `x=$(git diff --patch)` binds when the
//! renderer attached structured data. Real git is the oracle for every path,
//! mode, oid, size and byte the fake renderer is handed.
//!
//! The no-renderer half is not here: `tests/textdiff.rs` compares this
//! build's own patch text to `git diff --patch` byte for byte, and it runs
//! unchanged with a renderer-capable `GitConfig`, which is the check that the
//! default output did not move.

#![cfg(feature = "textdiff")]

#[path = "support.rs"]
mod support;

use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::{Arc, Mutex};

use kaish_kernel::{Kernel, KernelConfig};
use kaish_tool_api::{KernelBackend, Tool};
use kaish_types::{ExecResult, ToolSchema};

use kaish_tools_git::{
    ChangeKind, ChangedFile, GitConfig, Limits, PatchOptions, PatchRenderError, PatchRenderer,
    RenderedPatch, Withheld,
};

use support::{git, require_git, verb_words, write_file, Fixture, StrictBackend, TestCtx};

/// The `max_blob_bytes` every case here runs under: above every fixture file
/// but `big.txt`, so exactly one file is over the cap.
const CAP: u64 = 1024;

/// A `max_hunk_bytes_per_file` no default would produce, so a renderer that
/// received it got it from the embedder's `Limits` and nowhere else.
const HUNK_CAP: u64 = 4093;

// ---------------------------------------------------------------------------
// The fake renderer
// ---------------------------------------------------------------------------

/// One `ChangedFile`, copied out of the borrow so it outlives the call.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Seen {
    old_path: Option<String>,
    new_path: Option<String>,
    kind: ChangeKind,
    old_mode: Option<String>,
    new_mode: Option<String>,
    old_oid: Option<String>,
    new_oid: Option<String>,
    before: Option<Vec<u8>>,
    after: Option<Vec<u8>>,
    old_size: Option<u64>,
    new_size: Option<u64>,
    withheld: Option<Withheld>,
}

impl Seen {
    fn of(file: &ChangedFile<'_>) -> Self {
        Seen {
            old_path: file.old_path.map(str::to_string),
            new_path: file.new_path.map(str::to_string),
            kind: file.kind.clone(),
            old_mode: file.old_mode.map(str::to_string),
            new_mode: file.new_mode.map(str::to_string),
            old_oid: file.old_oid.map(str::to_string),
            new_oid: file.new_oid.map(str::to_string),
            before: file.before.map(<[u8]>::to_vec),
            after: file.after.map(<[u8]>::to_vec),
            old_size: file.old_size,
            new_size: file.new_size,
            withheld: file.withheld,
        }
    }

    /// The path a listing sorts this file by.
    fn key(&self) -> String {
        self.new_path.clone().or_else(|| self.old_path.clone()).unwrap_or_default()
    }
}

/// What the fake answers with.
enum Reply {
    Patch { text: String, data: Option<serde_json::Value> },
    Fail(String),
}

/// A renderer that records every call and answers with a fixed reply.
struct Recorder {
    calls: Mutex<Vec<(Vec<Seen>, PatchOptions)>>,
    reply: Reply,
}

impl Recorder {
    fn replying(text: &str) -> Arc<Self> {
        Arc::new(Recorder {
            calls: Mutex::new(Vec::new()),
            reply: Reply::Patch {
                text: text.to_string(),
                data: None,
            },
        })
    }

    fn with_data(text: &str, data: serde_json::Value) -> Arc<Self> {
        Arc::new(Recorder {
            calls: Mutex::new(Vec::new()),
            reply: Reply::Patch {
                text: text.to_string(),
                data: Some(data),
            },
        })
    }

    fn failing(message: &str) -> Arc<Self> {
        Arc::new(Recorder {
            calls: Mutex::new(Vec::new()),
            reply: Reply::Fail(message.to_string()),
        })
    }

    fn calls(&self) -> Vec<(Vec<Seen>, PatchOptions)> {
        self.calls.lock().expect("recorder lock").clone()
    }
}

/// A renderer error with one wrapped cause, the shape an engine error takes
/// once an adapter adds context to it.
#[derive(Debug)]
struct FakeFailure {
    message: String,
    cause: FakeCause,
}

#[derive(Debug)]
struct FakeCause;

impl std::fmt::Display for FakeFailure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.message)
    }
}

impl std::fmt::Display for FakeCause {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("the wrapped cause")
    }
}

impl std::error::Error for FakeFailure {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        Some(&self.cause)
    }
}

impl std::error::Error for FakeCause {}

impl PatchRenderer for Recorder {
    fn render(
        &self,
        files: &[ChangedFile<'_>],
        opts: &PatchOptions,
    ) -> Result<RenderedPatch, PatchRenderError> {
        self.calls
            .lock()
            .expect("recorder lock")
            .push((files.iter().map(Seen::of).collect(), opts.clone()));
        match &self.reply {
            Reply::Patch { text, data } => {
                let patch = RenderedPatch::new(text.clone());
                Ok(match data {
                    Some(data) => patch.with_data(data.clone()),
                    None => patch,
                })
            }
            Reply::Fail(message) => Err(Box::new(FakeFailure {
                message: message.clone(),
                cause: FakeCause,
            })),
        }
    }
}

// ---------------------------------------------------------------------------
// The fixture and the kernel
// ---------------------------------------------------------------------------

/// Bytes git's binary heuristic and ours both call binary: a NUL, and invalid
/// UTF-8 after it.
fn binary(tail: u8) -> Vec<u8> {
    vec![0x00, 0x01, b'h', b'i', 0x00, 0xFF, 0xFE, 0x80, tail]
}

/// `big.txt`: well over [`CAP`], so its content must never reach a renderer.
fn big(extra: &str) -> String {
    let mut out = String::new();
    for i in 0..300 {
        out.push_str(&format!("line {i:04}\n"));
    }
    out.push_str(extra);
    out
}

/// Two commits whose difference holds every change kind this build reports:
/// a modification, a delete, an add, an exact rename, a mode flip, a binary
/// change, a symlink that became a file, and a modification over the cap.
fn repo() -> (Fixture, PathBuf) {
    require_git();
    let fixture = Fixture::empty();
    let root = fixture.path("repo");
    std::fs::create_dir_all(&root).expect("create repo dir");
    git(&root, &["init", "--initial-branch=main", "--quiet"]);
    git(&root, &["config", "core.autocrlf", "false"]);

    write_file(&root, "keep.txt", "one\ntwo\nthree\n");
    write_file(&root, "gone.txt", "bye\n");
    write_file(&root, "mode.sh", "#!/bin/sh\necho hi\n");
    write_file(&root, "moved.txt", "l1\nl2\nl3\n");
    write_file(&root, "big.txt", &big(""));
    std::fs::write(root.join("data.bin"), binary(0x11)).expect("write data.bin");
    std::os::unix::fs::symlink("keep.txt", root.join("link")).expect("symlink");
    git(&root, &["add", "-A"]);
    git(&root, &["commit", "-m", "base", "--quiet"]);

    write_file(&root, "keep.txt", "one\nTWO\nthree\n");
    git(&root, &["rm", "--quiet", "gone.txt"]);
    write_file(&root, "added.txt", "brand new\n");
    git(&root, &["mv", "moved.txt", "renamed.txt"]);
    write_file(&root, "big.txt", &big("one more\n"));
    std::fs::write(root.join("data.bin"), binary(0x22)).expect("write data.bin");
    std::fs::remove_file(root.join("link")).expect("remove symlink");
    write_file(&root, "link", "a file now\n");
    {
        use std::os::unix::fs::PermissionsExt;
        let path = root.join("mode.sh");
        let mut perms = std::fs::metadata(&path).expect("stat mode.sh").permissions();
        perms.set_mode(0o755);
        std::fs::set_permissions(&path, perms).expect("chmod mode.sh");
    }
    git(&root, &["add", "-A"]);
    git(&root, &["commit", "-m", "everything", "--quiet"]);
    (fixture, root)
}

fn limits() -> Limits {
    Limits {
        max_blob_bytes: CAP,
        max_hunk_bytes_per_file: HUNK_CAP,
        ..Limits::default()
    }
}

fn config(renderer: Arc<dyn PatchRenderer>) -> GitConfig {
    GitConfig::read_only()
        .with_limits(limits())
        .with_patch_renderer(renderer)
}

/// A kernel whose `/mnt` is the fixture's scratch root, with its cwd in the
/// repository at `/mnt/repo`.
async fn kernel(root: &Path, config: GitConfig) -> Kernel {
    let scratch = root.parent().expect("the repository sits in the fixture root");
    let backend: Arc<dyn KernelBackend> =
        Arc::new(StrictBackend::single(PathBuf::from("/mnt"), scratch.to_path_buf()));
    let git = kaish_tools_git::tool(config).expect("valid config");
    let mut cfg = KernelConfig::transient();
    cfg.cwd = PathBuf::from("/mnt/repo");
    Kernel::with_backend(backend, cfg, |_| {}, |tools| tools.register(git))
        .expect("kernel assembles from a valid git tool")
}

async fn run(root: &Path, config: GitConfig, script: &str) -> ExecResult {
    kernel(root, config)
        .await
        .execute(script)
        .await
        .unwrap_or_else(|e| panic!("kaish refused to run {script:?}: {e}"))
}

/// The range every case renders.
const RANGE: &str = r#""HEAD~1" HEAD"#;

// ---------------------------------------------------------------------------
// The oracle
// ---------------------------------------------------------------------------

/// A blob's bytes, from real git.
fn blob(root: &Path, oid: &str) -> Vec<u8> {
    let out = Command::new("git")
        .args(["cat-file", "blob", oid])
        .current_dir(root)
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .output()
        .expect("run git cat-file");
    assert!(out.status.success(), "git cat-file blob {oid} failed");
    out.stdout
}

/// What real git says the renderer should have been handed for
/// `HEAD~1..HEAD`, from `git diff --raw -z` and `git cat-file`.
fn expected(root: &Path) -> Vec<Seen> {
    let raw = git(root, &["diff", "--raw", "-z", "--no-abbrev", "-M100%", "HEAD~1", "HEAD"]);
    let mut fields = raw.split('\0').filter(|s| !s.is_empty());
    let mut out = Vec::new();
    let absent = |s: &str| s.chars().all(|c| c == '0');
    while let Some(meta) = fields.next() {
        let meta: Vec<&str> = meta.trim_start_matches(':').split(' ').collect();
        let (old_mode, new_mode, old_oid, new_oid, status) = (meta[0], meta[1], meta[2], meta[3], meta[4]);
        let first = fields.next().expect("a path after the metadata").to_string();
        let (old_path, new_path, kind) = match &status[..1] {
            "A" => (None, Some(first), ChangeKind::Added),
            "D" => (Some(first), None, ChangeKind::Deleted),
            "M" => (Some(first.clone()), Some(first), ChangeKind::Modified),
            "T" => (Some(first.clone()), Some(first), ChangeKind::TypeChanged),
            "R" => {
                let second = fields.next().expect("a rename's second path").to_string();
                (
                    Some(first),
                    Some(second),
                    ChangeKind::Renamed {
                        similarity: status[1..].parse().expect("a rename score"),
                    },
                )
            }
            other => panic!("the fixture produced a status this test does not expect: {other}"),
        };
        let side = |mode: &str, oid: &str| -> (Option<String>, Option<String>, Option<u64>, Option<Vec<u8>>) {
            if absent(oid) {
                return (None, None, None, None);
            }
            let bytes = blob(root, oid);
            (Some(mode.to_string()), Some(oid.to_string()), Some(bytes.len() as u64), Some(bytes))
        };
        let (old_mode, old_oid, old_size, before) = side(old_mode, old_oid);
        let (new_mode, new_oid, new_size, after) = side(new_mode, new_oid);
        let over = old_size.unwrap_or(0) > CAP || new_size.unwrap_or(0) > CAP;
        out.push(Seen {
            old_path,
            new_path,
            kind,
            old_mode,
            new_mode,
            old_oid,
            new_oid,
            before: if over { None } else { before },
            after: if over { None } else { after },
            old_size,
            new_size,
            withheld: over.then_some(Withheld::OverMaxBlobBytes),
        });
    }
    out.sort_by_key(Seen::key);
    out
}

// ---------------------------------------------------------------------------
// What the renderer receives
// ---------------------------------------------------------------------------

/// The renderer is called once, with every changed file of the range: the
/// paths, kind, modes, oids, sizes and bytes real git reports for each, and
/// no content at all for the file over `max_blob_bytes`.
#[tokio::test]
async fn the_renderer_receives_every_changed_file_as_git_reports_it() {
    let (_f, root) = repo();
    let rec = Recorder::replying("rendered\n");
    let result = run(&root, config(rec.clone()), &format!("git diff --patch {RANGE}")).await;
    assert_eq!(result.code, 0, "git diff --patch failed: {}", result.err);

    let calls = rec.calls();
    assert_eq!(calls.len(), 1, "one call per invocation, with every file in it");
    let mut seen = calls[0].0.clone();
    let order: Vec<String> = seen.iter().map(Seen::key).collect();
    let mut sorted = order.clone();
    sorted.sort();
    assert_eq!(order, sorted, "files arrive in path order");
    seen.sort_by_key(Seen::key);
    let want = expected(&root);

    // Negative controls: the fixture holds each shape this test claims to
    // cover, so a renderer handed an empty list could not pass by agreeing
    // with an empty oracle.
    let kinds: Vec<&ChangeKind> = want.iter().map(|s| &s.kind).collect();
    for kind in [
        ChangeKind::Added,
        ChangeKind::Deleted,
        ChangeKind::Modified,
        ChangeKind::Renamed { similarity: 100 },
        ChangeKind::TypeChanged,
    ] {
        assert!(kinds.contains(&&kind), "the oracle must hold a {kind:?} file: {want:#?}");
    }
    assert!(
        want.iter().any(|s| s.withheld.is_some()),
        "the oracle must hold an over-cap file"
    );
    assert!(
        want.iter().any(|s| s.after.as_deref().is_some_and(|b| b.contains(&0))),
        "the oracle must hold a binary file"
    );

    assert_eq!(seen, want, "the renderer must receive exactly what git reports");
}

/// The cap rule, spelled out on the one file it applies to: no bytes on
/// either side, both real sizes, and the flag that says why.
#[tokio::test]
async fn a_file_over_max_blob_bytes_arrives_with_sizes_and_no_content() {
    let (_f, root) = repo();
    let rec = Recorder::replying("rendered\n");
    let result = run(&root, config(rec.clone()), &format!("git diff --patch {RANGE}")).await;
    assert_eq!(result.code, 0, "git diff --patch failed: {}", result.err);

    let calls = rec.calls();
    let big = calls[0]
        .0
        .iter()
        .find(|s| s.new_path.as_deref() == Some("big.txt"))
        .expect("big.txt is in the change set");
    assert_eq!(big.withheld, Some(Withheld::OverMaxBlobBytes));
    assert_eq!(big.before, None);
    assert_eq!(big.after, None);
    assert_eq!(big.old_size, Some(big_len("")));
    assert_eq!(big.new_size, Some(big_len("one more\n")));
    assert!(big.new_size.expect("size") > CAP, "control: the file really is over the cap");

    // The stderr note says the same thing in words.
    assert!(
        result.err.contains("max_blob_bytes"),
        "the over-cap note must name the cap: {}",
        result.err
    );
}

fn big_len(extra: &str) -> u64 {
    big(extra).len() as u64
}

/// The exact rename and the mode flip carry the same bytes on both sides —
/// the content did not change — and the rename says `similarity: 100`.
#[tokio::test]
async fn a_rename_and_a_mode_flip_carry_identical_sides() {
    let (_f, root) = repo();
    let rec = Recorder::replying("rendered\n");
    run(&root, config(rec.clone()), &format!("git diff --patch {RANGE}")).await;
    let calls = rec.calls();
    let files = &calls[0].0;

    let renamed = files
        .iter()
        .find(|s| s.new_path.as_deref() == Some("renamed.txt"))
        .expect("the rename is in the change set");
    assert_eq!(renamed.old_path.as_deref(), Some("moved.txt"));
    assert_eq!(renamed.kind, ChangeKind::Renamed { similarity: 100 });
    assert_eq!(renamed.before.as_deref(), Some(&b"l1\nl2\nl3\n"[..]));
    assert_eq!(renamed.before, renamed.after);

    let mode = files
        .iter()
        .find(|s| s.new_path.as_deref() == Some("mode.sh"))
        .expect("the mode flip is in the change set");
    assert_eq!(mode.kind, ChangeKind::Modified);
    assert_eq!(mode.old_mode.as_deref(), Some("100644"));
    assert_eq!(mode.new_mode.as_deref(), Some("100755"));
    assert_eq!(mode.before.as_deref(), Some(&b"#!/bin/sh\necho hi\n"[..]));
    assert_eq!(mode.before, mode.after);
}

/// `--context 5` reaches the renderer as 5, the default is git's 3, and the
/// per-file hunk cap is the embedder's own `max_hunk_bytes_per_file`.
/// `word_diff` is not parsed yet and is always false.
#[tokio::test]
async fn the_parsed_options_reach_the_renderer() {
    let (_f, root) = repo();

    let rec = Recorder::replying("rendered\n");
    let result = run(&root, config(rec.clone()), &format!("git diff --patch --context 5 {RANGE}")).await;
    assert_eq!(result.code, 0, "git diff --patch --context 5 failed: {}", result.err);
    let opts = rec.calls()[0].1.clone();
    assert_eq!(opts.context, 5);
    assert_eq!(opts.max_hunk_bytes_per_file, HUNK_CAP);
    assert!(!opts.word_diff);

    let rec = Recorder::replying("rendered\n");
    run(&root, config(rec.clone()), &format!("git diff --patch {RANGE}")).await;
    assert_eq!(rec.calls()[0].1.context, 3, "git's default context is 3");
}

// ---------------------------------------------------------------------------
// What the caller sees
// ---------------------------------------------------------------------------

/// The renderer's text is stdout, byte for byte, and nothing else is; the
/// endpoints go to stderr exactly as they do for this build's own patch.
#[tokio::test]
async fn the_renderer_text_is_stdout() {
    let (_f, root) = repo();
    let text = "RENDERED BY THE FAKE\n\tline two, tab kept\n";
    let rec = Recorder::replying(text);
    let result = run(&root, config(rec), &format!("git diff --patch {RANGE}")).await;
    assert_eq!(result.code, 0, "git diff --patch failed: {}", result.err);
    assert_eq!(result.text_out(), text);
    assert!(result.err.contains("HEAD~1"), "the endpoints move to stderr: {}", result.err);
}

/// A renderer error fails the call: non-zero, nothing on stdout, and the
/// renderer's own message on stderr. There is no fallback to this build's
/// renderer — an embedder that installed one asked for its output or none.
#[tokio::test]
async fn a_renderer_error_is_a_loud_failure_naming_its_message() {
    let (_f, root) = repo();
    let rec = Recorder::failing("the engine refused this on purpose");
    let result = run(&root, config(rec.clone()), &format!("git diff --patch {RANGE}")).await;
    assert_eq!(result.code, 1, "a renderer failure exits 1: {}", result.err);
    assert!(
        result.err.contains("the engine refused this on purpose"),
        "stderr must carry the renderer's message: {}",
        result.err
    );
    assert!(
        result.err.contains("the engine refused this on purpose: the wrapped cause"),
        "stderr must carry each source after the message: {}",
        result.err
    );
    assert!(result.err.contains("renderer"), "stderr must say which part failed: {}", result.err);
    assert_eq!(result.text_out(), "", "no patch, and in particular not ours");
    assert_eq!(rec.calls().len(), 1, "control: the renderer was asked");
}

/// `--json` is this build's model, renderer or not. The renderer is not asked
/// at all — this one would fail the call if it were.
#[tokio::test]
async fn json_output_is_unaffected_by_a_renderer() {
    let (_f, root) = repo();
    let rec = Recorder::failing("must not be called under --json");
    let result = run(&root, config(rec.clone()), &format!("git diff --patch --json {RANGE}")).await;
    assert_eq!(result.code, 0, "git diff --patch --json failed: {}", result.err);
    assert!(rec.calls().is_empty(), "the renderer must not be asked under --json");

    let json: serde_json::Value =
        serde_json::from_str(&result.text_out()).expect("--json output parses");
    let files = json["files"].as_array().expect("a files array");
    let keep = files
        .iter()
        .find(|f| f["path"] == "keep.txt")
        .expect("keep.txt is in the model");
    assert!(
        keep["hunks"].as_array().is_some_and(|h| !h.is_empty()),
        "--json --patch carries this build's own hunks: {keep}"
    );
}

/// Without `--patch` there is no patch to render, and the renderer is not
/// asked.
#[tokio::test]
async fn plain_diff_does_not_call_the_renderer() {
    let (_f, root) = repo();
    let rec = Recorder::failing("must not be called without --patch");
    let result = run(&root, config(rec.clone()), &format!("git diff {RANGE}")).await;
    assert_eq!(result.code, 0, "git diff failed: {}", result.err);
    assert!(rec.calls().is_empty());
    assert!(result.text_out().contains("keep.txt"), "control: the table is there");
}

/// A working-tree side over the cap fails the whole diff before anything is
/// rendered, as it does without a renderer: the working tree is hashed to
/// find what changed, and a file over the cap is not read even for that.
#[tokio::test]
async fn a_worktree_file_over_the_cap_fails_before_the_renderer_is_asked() {
    let (_f, root) = repo();
    let rec = Recorder::replying("rendered\n");
    let result = run(&root, config(rec.clone()), "git diff --patch").await;
    assert_eq!(result.code, 1, "{}", result.err);
    assert!(result.err.contains("big.txt"), "{}", result.err);
    assert!(result.err.contains("max_blob_bytes"), "{}", result.err);
    assert!(rec.calls().is_empty());
}

// ---------------------------------------------------------------------------
// The renderer's structured data, and what `$(…)` binds
// ---------------------------------------------------------------------------

/// A renderer's `data` is the result's structured data, and a substitution
/// still binds the **text** — AGENTS.md, "Command substitution binds text".
///
/// Three halves. The substitution binds a string holding the rendered text.
/// The pipeline's structured sideband carries the data, which is the positive
/// control that `.data` really was set — without it, the first half would
/// pass just as well with the data dropped on the floor. And `fromjson`
/// binding a record through the same kernel is the control that typed
/// substitution works here at all.
#[tokio::test]
async fn renderer_data_is_attached_and_a_substitution_still_binds_text() {
    let (_f, root) = repo();
    let data = serde_json::json!({"marker": "from-renderer", "files": 8});
    let rec = Recorder::with_data("patch text marker-xyz\n", data);

    let typed = run(&root, config(rec.clone()), &format!("x=$(git diff --patch {RANGE}); typeof $x")).await;
    assert_eq!(typed.code, 0, "{}", typed.err);
    assert_eq!(typed.text_out().trim(), "string", "a git substitution binds text");

    let bound = run(&root, config(rec.clone()), &format!("x=$(git diff --patch {RANGE}); echo $x")).await;
    assert_eq!(bound.code, 0, "{}", bound.err);
    assert_eq!(bound.text_out().trim(), "patch text marker-xyz");

    let piped = run(&root, config(rec.clone()), &format!("git diff --patch {RANGE} | jq -r .marker")).await;
    assert_eq!(piped.code, 0, "{}", piped.err);
    assert_eq!(
        piped.text_out().trim(),
        "from-renderer",
        "positive control: the renderer's data must reach the pipeline sideband, \
         or the substitution assertions above prove nothing about it"
    );

    let control = run(
        &root,
        config(rec),
        r#"y=$(echo '{"a":1}' | fromjson); typeof $y"#,
    )
    .await;
    assert_eq!(control.code, 0, "{}", control.err);
    assert_eq!(control.text_out().trim(), "record", "typed substitution works in this kernel");
}

// ---------------------------------------------------------------------------
// The published schema
// ---------------------------------------------------------------------------

fn patch_description(schema: &ToolSchema) -> String {
    let diff = schema
        .subcommands
        .iter()
        .find(|s| s.name == "diff")
        .expect("diff is in the schema");
    diff.params
        .iter()
        .find(|p| p.name == "patch")
        .expect("--patch is a diff param")
        .description
        .clone()
}

/// With a renderer installed, `--patch`'s published description stops
/// promising that `git apply` takes the output — that is the renderer's call,
/// not this build's — and says where the text comes from.
#[test]
fn the_patch_description_says_who_renders() {
    let ours = kaish_tools_git::tool(GitConfig::read_only()).expect("config").schema();
    let theirs = kaish_tools_git::tool(config(Recorder::replying("")))
        .expect("config")
        .schema();
    let ours = patch_description(&ours);
    let theirs = patch_description(&theirs);
    assert!(ours.contains("git apply"), "control: the default promises git apply: {ours}");
    assert!(!theirs.contains("| git apply` works"), "{theirs}");
    assert!(theirs.contains("renderer"), "{theirs}");
    assert!(theirs.contains("--json"), "{theirs}");
}

// ---------------------------------------------------------------------------
// The model behind a rendered patch
// ---------------------------------------------------------------------------

/// A gitlink at `path` naming `oid`, staged without a submodule checkout.
fn stage_gitlink(root: &Path, oid: &str, path: &str) {
    git(root, &["update-index", "--add", "--cacheinfo", &format!("160000,{oid},{path}")]);
}

/// The shapes where reading order decides the model: a renamed file over the
/// cap, a moved submodule, a submodule replaced by a file over the cap, and
/// an ordinary modification over the cap.
fn order_repo() -> (Fixture, PathBuf) {
    require_git();
    let fixture = Fixture::empty();
    let root = fixture.path("repo");
    std::fs::create_dir_all(&root).expect("create repo dir");
    git(&root, &["init", "--initial-branch=main", "--quiet"]);
    write_file(&root, "huge.txt", &big(""));
    write_file(&root, "bigedit.txt", &big(""));
    write_file(&root, "small.txt", "small\n");
    git(&root, &["add", "-A"]);
    stage_gitlink(&root, &"a".repeat(40), "g");
    stage_gitlink(&root, &"b".repeat(40), "sub");
    git(&root, &["commit", "-m", "base", "--quiet"]);

    git(&root, &["mv", "huge.txt", "huge2.txt"]);
    git(&root, &["update-index", "--force-remove", "sub"]);
    stage_gitlink(&root, &"b".repeat(40), "sub2");
    git(&root, &["update-index", "--force-remove", "g"]);
    write_file(&root, "g", &big("now a file\n"));
    write_file(&root, "bigedit.txt", &big("edited\n"));
    write_file(&root, "small.txt", "smaller\n");
    git(&root, &["add", "g", "bigedit.txt", "small.txt"]);
    git(&root, &["commit", "-m", "moves", "--quiet"]);
    (fixture, root)
}

/// `git <argv>` through `Tool::execute`, where the result's typed model is
/// readable whichever text it printed.
async fn direct(root: &Path, config: GitConfig, argv: &[&str]) -> ExecResult {
    let scratch = root.parent().expect("the repository sits in the fixture root");
    let backend = Arc::new(StrictBackend::single(PathBuf::from("/mnt"), scratch.to_path_buf()));
    let mut ctx = TestCtx::new(backend, "/mnt/repo");
    let tool = kaish_tools_git::tool(config).expect("config");
    tool.execute(verb_words("diff", argv), &mut ctx).await
}

fn model(result: &ExecResult) -> serde_json::Value {
    assert_eq!(result.code, 0, "diff failed: {}", result.err);
    result
        .output()
        .and_then(|o| o.rich_json.clone())
        .expect("the result carries the typed model")
}

/// The model built beside a rendered patch is the model plain `git diff`
/// builds for the same change — counts, `binary`, `lines_capped` and all —
/// so nothing an agent reads besides the patch text depends on whether a
/// renderer is installed. The three shapes in the fixture are the ones where
/// the order of "rename, submodule, cap" decides the answer.
#[tokio::test]
async fn the_renderer_path_builds_the_model_json_reports() {
    let (_f, root) = order_repo();
    let range = ["HEAD~1", "HEAD"];

    let stock = model(&direct(&root, GitConfig::read_only().with_limits(limits()), &range).await);
    let rec = Recorder::replying("rendered\n");
    let rendered = direct(&root, config(rec.clone()), &["--patch", range[0], range[1]]).await;
    assert_eq!(rec.calls().len(), 1, "control: the renderer was asked");
    let rendered = model(&rendered);

    // Negative controls: the shapes are really there.
    let files = stock["files"].as_array().expect("files");
    let find = |path: &str| {
        files
            .iter()
            .find(|f| f["path"] == path)
            .unwrap_or_else(|| panic!("{path} is in the model: {stock:#}"))
    };
    assert_eq!(find("huge2.txt")["status"], "renamed");
    assert_eq!(find("sub2")["status"], "renamed");
    assert_eq!(find("sub2")["new_mode"], "160000");
    assert_eq!(find("g")["status"], "typechange");
    assert_eq!(find("bigedit.txt")["lines_capped"], true);

    assert_eq!(rendered, stock, "the renderer path must build the model plain diff builds");
}

/// The stderr note counts the files whose content the renderer did not get,
/// which is not `lines_capped`: a renamed file over the cap is withheld from
/// the renderer and still counted `0/0` in the model.
#[tokio::test]
async fn the_note_counts_what_the_renderer_did_not_get() {
    let (_f, root) = order_repo();
    let rec = Recorder::replying("rendered\n");
    let result = direct(&root, config(rec.clone()), &["--patch", "HEAD~1", "HEAD"]).await;
    assert_eq!(result.code, 0, "{}", result.err);

    let withheld = rec.calls()[0]
        .0
        .iter()
        .filter(|s| s.withheld == Some(Withheld::OverMaxBlobBytes))
        .count();
    assert_eq!(withheld, 3, "control: huge2.txt, g and bigedit.txt are over the cap");
    assert!(
        result.err.contains(&format!("{withheld} file(s) have a side over max_blob_bytes")),
        "the note must count every withheld file: {}",
        result.err
    );
}

/// A working-tree symlink's target is read whatever the cap — the hashing
/// pass reads it the same way, and a path is bounded by the platform's path
/// limit — so it reaches a renderer as content, with its real size and
/// no `withheld` reason, even when that size is over the cap.
#[tokio::test]
async fn a_worktree_symlink_target_is_content_whatever_the_cap() {
    require_git();
    let fixture = Fixture::empty();
    let root = fixture.path("repo");
    std::fs::create_dir_all(&root).expect("create repo dir");
    git(&root, &["init", "--initial-branch=main", "--quiet"]);
    write_file(&root, "a", "x\n");
    std::os::unix::fs::symlink("ab", root.join("l")).expect("symlink");
    git(&root, &["add", "-A"]);
    git(&root, &["commit", "-m", "base", "--quiet"]);
    let long = "a-target-longer-than-the-cap";
    std::fs::remove_file(root.join("l")).expect("remove symlink");
    std::os::unix::fs::symlink(long, root.join("l")).expect("symlink");

    let cap = 8;
    let rec = Recorder::replying("rendered\n");
    let config = GitConfig::read_only()
        .with_limits(Limits {
            max_blob_bytes: cap,
            ..Limits::default()
        })
        .with_patch_renderer(rec.clone());
    let result = direct(&root, config, &["--patch"]).await;
    assert_eq!(result.code, 0, "{}", result.err);

    let calls = rec.calls();
    let link = calls[0]
        .0
        .iter()
        .find(|s| s.new_path.as_deref() == Some("l"))
        .expect("the symlink changed");
    assert_eq!(link.after.as_deref(), Some(long.as_bytes()));
    assert_eq!(link.new_size, Some(long.len() as u64));
    assert!(link.new_size.expect("size") > cap, "control: the target is over the cap");
    assert_eq!(link.withheld, None);
}

// ---------------------------------------------------------------------------
// The per-call content budget
// ---------------------------------------------------------------------------

/// [`limits`] with a content budget small enough to run out inside the
/// fixture: `added.txt` (10 bytes) fits, `big.txt` is over `max_blob_bytes`
/// and costs nothing, and `data.bin` (two 9-byte sides) does not fit in what
/// is left.
fn tight() -> Limits {
    Limits {
        max_patch_content_bytes: 20,
        ..limits()
    }
}

/// Files are filled in path order until the next one would not fit; from
/// there on every file arrives the way an over-cap file does — no content,
/// real sizes — with the budget named as the reason. The answer is the same
/// on every run.
#[tokio::test]
async fn a_spent_content_budget_withholds_every_later_file() {
    let (_f, root) = repo();
    let want = expected(&root);

    let rec = Recorder::replying("rendered\n");
    let config = GitConfig::read_only()
        .with_limits(tight())
        .with_patch_renderer(rec.clone());
    let result = run(&root, config.clone(), &format!("git diff --patch {RANGE}")).await;
    assert_eq!(result.code, 0, "{}", result.err);
    let seen = rec.calls()[0].0.clone();

    let keys: Vec<String> = seen.iter().map(Seen::key).collect();
    let spent_at = keys
        .iter()
        .position(|k| k == "data.bin")
        .expect("data.bin is in the change set");
    assert!(spent_at > 0 && spent_at < seen.len() - 1, "control: files on both sides of the boundary");

    for (file, oracle) in seen.iter().zip(&want) {
        assert_eq!(file.key(), oracle.key(), "the same files, in path order");
        // Sizes and everything but content are untouched by the budget.
        assert_eq!((file.old_size, file.new_size), (oracle.old_size, oracle.new_size));
        assert_eq!(file.kind, oracle.kind);
    }
    let added = &seen[keys.iter().position(|k| k == "added.txt").expect("added.txt")];
    assert_eq!(added.after.as_deref(), Some(&b"brand new\n"[..]), "before the boundary: content");
    assert_eq!(added.withheld, None);
    let big = &seen[keys.iter().position(|k| k == "big.txt").expect("big.txt")];
    assert_eq!(
        big.withheld,
        Some(Withheld::OverMaxBlobBytes),
        "a file over max_blob_bytes keeps that reason"
    );
    let mut budgeted = 0;
    for file in &seen[spent_at..] {
        assert_eq!(file.before, None, "{} must carry no content", file.key());
        assert_eq!(file.after, None, "{} must carry no content", file.key());
        assert_eq!(
            file.withheld,
            Some(Withheld::OverMaxPatchContentBytes),
            "{} names the budget as the reason",
            file.key()
        );
        budgeted += 1;
    }
    assert!(
        result.err.contains(&format!("{budgeted} file(s)")) && result.err.contains("max_patch_content_bytes"),
        "the note counts the files the budget withheld: {}",
        result.err
    );

    // Deterministic: a second run hands over exactly the same thing.
    let again = Recorder::replying("rendered\n");
    let config = GitConfig::read_only()
        .with_limits(tight())
        .with_patch_renderer(again.clone());
    run(&root, config, &format!("git diff --patch {RANGE}")).await;
    assert_eq!(again.calls()[0].0, seen);
}

/// The budget withholds content from the renderer and nothing from the
/// model: counts and `binary` are still the ones plain `git diff` reports.
#[tokio::test]
async fn a_spent_budget_leaves_the_model_alone() {
    let (_f, root) = repo();
    let stock = model(&direct(&root, GitConfig::read_only().with_limits(tight()), &["HEAD~1", "HEAD"]).await);
    let rec = Recorder::replying("rendered\n");
    let config = GitConfig::read_only().with_limits(tight()).with_patch_renderer(rec.clone());
    let rendered = model(&direct(&root, config, &["--patch", "HEAD~1", "HEAD"]).await);
    assert!(
        rec.calls()[0].0.iter().any(|s| s.withheld == Some(Withheld::OverMaxPatchContentBytes)),
        "control: the budget really ran out"
    );
    assert_eq!(rendered, stock);
}

/// `git info` reports the budget beside the other limits.
#[tokio::test]
async fn git_info_reports_the_content_budget() {
    let (_f, root) = repo();
    let result = run(&root, GitConfig::read_only().with_limits(tight()), "git info --json").await;
    assert_eq!(result.code, 0, "{}", result.err);
    let json: serde_json::Value = serde_json::from_str(&result.text_out()).expect("json");
    let limits = &json["capabilities"]["limits"];
    assert_eq!(limits["max_patch_content_bytes"], 20, "{limits}");
    assert_eq!(limits["max_blob_bytes"], CAP, "control: the other limits are this config's too");
}
