//! `--oneline` and `short_oid` abbreviate an oid the way git's
//! `core.abbrev=auto` does: the shortest prefix that no other object shares,
//! never shorter than a floor that grows with the number of *packed* objects.
//!
//! The oracle is `git log --oneline` on the same repository. Three facts are
//! held against it: the floor (7 for a small repository, 8 once the packs hold
//! 16384 objects, and loose objects are not counted, which an experiment with
//! git 2.56 showed), the uniqueness guarantee (a prefix that another object
//! shares is lengthened), and `core.abbrev`.

#[path = "support.rs"]
mod support;

use std::path::{Path, PathBuf};
use std::sync::Arc;

use kaish_kernel::{Kernel, KernelConfig};
use kaish_tool_api::KernelBackend;

use kaish_tools_git::GitConfig;

use support::{git, require_git, write_file, Fixture, StrictBackend};

async fn oneline(root: &Path, limit: usize) -> String {
    let backend: Arc<dyn KernelBackend> =
        Arc::new(StrictBackend::single(PathBuf::from("/mnt"), root.to_path_buf()));
    let git = kaish_tools_git::tool(GitConfig::read_only()).expect("valid config");
    let mut cfg = KernelConfig::transient();
    cfg.cwd = PathBuf::from("/mnt");
    let k = Kernel::with_backend(backend, cfg, |_| {}, |tools| tools.register(git))
        .expect("kernel assembles");
    let script = format!("git log --oneline --limit {limit}");
    let r = k.execute(&script).await.unwrap_or_else(|e| panic!("{script}: {e}"));
    assert_eq!(r.code, 0, "{script} failed: {}", r.err);
    r.text_out().trim_end().to_string()
}

fn oid_widths(text: &str) -> Vec<usize> {
    text.lines().map(|l| l.split(' ').next().unwrap().len()).collect()
}

fn init() -> (Fixture, PathBuf) {
    require_git();
    let fixture = Fixture::empty();
    let root = fixture.path("repo");
    std::fs::create_dir_all(&root).expect("create repo dir");
    git(&root, &["init", "--initial-branch=main", "--quiet"]);
    // A commit may start `gc --auto` in the background, which would collide
    // with the explicit `gc` below and change which objects are packed.
    git(&root, &["config", "gc.auto", "0"]);
    (fixture, root)
}

#[tokio::test]
async fn a_small_repository_abbreviates_to_seven() {
    let (_f, root) = init();
    write_file(&root, "a.txt", "one\n");
    git(&root, &["add", "."]);
    git(&root, &["commit", "-m", "first", "--quiet"]);
    let ours = oneline(&root, 5).await;
    assert_eq!(ours, git(&root, &["log", "--oneline", "--no-decorate"]));
    assert_eq!(oid_widths(&ours), vec![7]);
}

#[tokio::test]
async fn the_floor_grows_with_packed_objects_and_ignores_loose_ones() {
    let (_f, root) = init();
    // 16400 distinct blobs: past 2^14, where git's floor moves from 7 to 8.
    for dir in 0..16 {
        for n in 0..1025 {
            write_file(&root, &format!("d{dir}/f{n}.txt"), &format!("{dir}-{n}\n"));
        }
    }
    git(&root, &["add", "."]);
    git(&root, &["commit", "-m", "many", "--quiet"]);
    // All loose: git counts packed objects only, so the floor is still 7.
    let loose = oneline(&root, 5).await;
    assert_eq!(loose, git(&root, &["log", "--oneline", "--no-decorate"]));
    assert_eq!(oid_widths(&loose), vec![7], "negative control: loose objects do not raise the floor");
    git(&root, &["gc", "--quiet"]);
    let packed = oneline(&root, 5).await;
    assert_eq!(packed, git(&root, &["log", "--oneline", "--no-decorate"]));
    assert_eq!(oid_widths(&packed), vec![8], "16400+ packed objects raise the floor to 8");
}

#[tokio::test]
async fn a_prefix_another_object_shares_is_lengthened_as_git_does() {
    let (_f, root) = init();
    git(&root, &["config", "core.abbrev", "4"]);
    // Four hex characters leave 65536 prefixes, so a thousand commits share
    // some, and git lengthens those. The fixture is deterministic.
    for n in 0..1100 {
        git(&root, &["commit", "--allow-empty", "-m", &format!("c{n}"), "--quiet"]);
    }
    let theirs = git(&root, &["log", "--oneline", "--no-decorate", "-n", "1000"]);
    let widths = oid_widths(&theirs);
    assert!(
        widths.iter().any(|w| *w > 4) && widths.contains(&4),
        "negative control: git lengthened some prefixes and not all: {widths:?}"
    );
    let ours = oneline(&root, 1000).await;
    // Line by line, so a failure names one commit and not a thousand.
    assert_eq!(ours.lines().count(), theirs.lines().count());
    for (o, t) in ours.lines().zip(theirs.lines()) {
        assert_eq!(o, t, "this commit's abbreviation differs from git's");
    }
}

#[tokio::test]
async fn core_abbrev_sets_the_floor() {
    let (_f, root) = init();
    write_file(&root, "a.txt", "one\n");
    git(&root, &["add", "."]);
    git(&root, &["commit", "-m", "first", "--quiet"]);
    git(&root, &["config", "core.abbrev", "12"]);
    let ours = oneline(&root, 5).await;
    assert_eq!(ours, git(&root, &["log", "--oneline", "--no-decorate"]));
    assert_eq!(oid_widths(&ours), vec![12]);
}

/// `git log --oneline --limit 5` through the kernel, failures kept.
async fn oneline_result(root: &Path) -> (i64, String, String) {
    let backend: Arc<dyn KernelBackend> =
        Arc::new(StrictBackend::single(PathBuf::from("/mnt"), root.to_path_buf()));
    let git = kaish_tools_git::tool(GitConfig::read_only()).expect("valid config");
    let mut cfg = KernelConfig::transient();
    cfg.cwd = PathBuf::from("/mnt");
    let k = Kernel::with_backend(backend, cfg, |_| {}, |tools| tools.register(git))
        .expect("kernel assembles");
    let r = k.execute("git log --oneline --limit 5").await.expect("kernel runs it");
    (r.code, r.text_out().trim_end().to_string(), r.err)
}

fn one_commit() -> (Fixture, PathBuf) {
    let (f, root) = init();
    write_file(&root, "a.txt", "one\n");
    git(&root, &["add", "."]);
    git(&root, &["commit", "-m", "first", "--quiet"]);
    (f, root)
}

/// Every value below was run against git 2.56: the booleans for false and
/// `auto` in any case, numbers in decimal, hex or octal, and numbers past the
/// hash length all print; each is held against git's own output.
#[tokio::test]
async fn core_abbrev_values_git_accepts_print_what_git_prints() {
    let (_f, root) = one_commit();
    for value in ["no", "NO", "off", "Off", "false", "auto", "Auto", "AUTO", "4", "12", "0x10", "010", "40", "41", "100", "1k"] {
        git(&root, &["config", "core.abbrev", value]);
        let theirs = git(&root, &["log", "--oneline", "--no-decorate"]);
        let (code, ours, err) = oneline_result(&root).await;
        assert_eq!(code, 0, "core.abbrev={value}: {err}");
        assert_eq!(ours, theirs.trim_end(), "core.abbrev={value}");
    }
}

/// Git refuses these at read time. So do we, rather than guess a width.
#[tokio::test]
async fn core_abbrev_values_git_refuses_are_refused() {
    let (_f, root) = one_commit();
    for value in ["0", "1", "3", "-1", "true", "yes", "on", "12abc", "08", "1.5"] {
        git(&root, &["config", "core.abbrev", value]);
        let status = std::process::Command::new("git")
            .args(["log", "--oneline", "-1"])
            .current_dir(&root)
            .output()
            .expect("git runs");
        assert!(!status.status.success(), "negative control: git refuses core.abbrev={value}");
        let (code, _out, err) = oneline_result(&root).await;
        assert_ne!(code, 0, "core.abbrev={value} must be refused");
        assert!(err.contains("core.abbrev"), "core.abbrev={value}: {err}");
    }
}
