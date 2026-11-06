//! Test fixtures for the write scaffolding: a real repository with a linked
//! worktree, and a sentinel directory outside the mount.
//!
//! Layout, with only `mounted/` inside the mount:
//!
//! ```text
//! <scratch>/mounted/repo/      a repository with one commit
//! <scratch>/mounted/wt/        a linked worktree of it, on branch `side`
//! <scratch>/<SENTINEL>/        the directory every planted link points at
//! ```
//!
//! # The table phase 1b points at real verbs
//!
//! [`CASES`] is a table of (plant, target) pairs. In 1a the target is checked
//! by calling the containment function. Phase 1b adds, per case, the verb
//! spelling that would write to that target (for example `branch sub/x` for
//! `refs/heads/sub/x`), runs it through a kernel, and asserts the same
//! things: exit 4, the planted component named relative to its root, no host
//! path in the text, the outside directory unchanged.

use std::path::{Path, PathBuf};
use std::process::Command;

use crate::repo::ReadRepo;
use crate::verbs::write::containment::Leaf;
use crate::verbs::write::repo::{WriteRepo, WriteToken};

/// The directory name every planted link points into. It appears in no
/// refusal text; a test searches for it.
pub(crate) const SENTINEL: &str = "sentinel-outside-the-mount";

pub(crate) struct Fixture {
    scratch: tempfile::TempDir,
    pub(crate) mount: PathBuf,
    pub(crate) repo: PathBuf,
    pub(crate) linked: PathBuf,
    pub(crate) outside: PathBuf,
}

fn git(cwd: &Path, args: &[&str]) {
    let out = Command::new("git")
        .current_dir(cwd)
        .args(["-c", "user.name=Fixture", "-c", "user.email=fixture@example.com"])
        .args(args)
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .output()
        .expect("git must be on PATH: the fixture is built by real git");
    assert!(
        out.status.success(),
        "git {args:?} failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
}

/// Run real git in `cwd` and return its stdout, trimmed.
pub(crate) fn git_stdout(cwd: &Path, args: &[&str]) -> String {
    let out = Command::new("git")
        .current_dir(cwd)
        .args(args)
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .output()
        .expect("git must be on PATH: the oracle is real git");
    assert!(out.status.success(), "git {args:?} failed: {}", String::from_utf8_lossy(&out.stderr));
    String::from_utf8(out.stdout).expect("utf-8").trim_end().to_string()
}

impl Fixture {
    /// A repository with one commit, a linked worktree, and the sentinel.
    pub(crate) fn new() -> Self {
        let scratch = tempfile::tempdir().expect("tempdir");
        let root = scratch.path().canonicalize().expect("canonical scratch");
        let mount = root.join("mounted");
        let repo = mount.join("repo");
        let linked = mount.join("wt");
        let outside = root.join(SENTINEL);
        std::fs::create_dir_all(&repo).expect("repo dir");
        std::fs::create_dir_all(&outside).expect("outside dir");
        // Something in the sentinel, so "unchanged" compares a non-empty tree.
        std::fs::create_dir(outside.join("objects_ab")).expect("sentinel dir");
        std::fs::create_dir(outside.join("refs_sub")).expect("sentinel dir");
        std::fs::write(outside.join("objects_ab").join("keep"), "keep\n").expect("sentinel");
        std::fs::write(outside.join("refs_sub").join("keep"), "keep\n").expect("sentinel");
        std::fs::write(outside.join("logs_head"), "keep\n").expect("sentinel");

        git(&repo, &["init", "--quiet", "--initial-branch=main"]);
        std::fs::write(repo.join("README.md"), "fixture\n").expect("readme");
        git(&repo, &["add", "."]);
        git(&repo, &["commit", "--quiet", "-m", "initial"]);
        git(
            &repo,
            &["worktree", "add", "--quiet", "-b", "side", linked.to_str().expect("utf-8"), "HEAD"],
        );
        Self { scratch, mount, repo, linked, outside }
    }

    /// A bare repository at `repo`, for the no-work-tree refusal.
    pub(crate) fn bare() -> Self {
        let scratch = tempfile::tempdir().expect("tempdir");
        let root = scratch.path().canonicalize().expect("canonical scratch");
        let mount = root.join("mounted");
        let repo = mount.join("repo.git");
        std::fs::create_dir_all(&repo).expect("repo dir");
        git(&repo, &["init", "--quiet", "--bare"]);
        Self { scratch, mount, repo, linked: PathBuf::new(), outside: root.join(SENTINEL) }
    }

    /// The scratch root: a prefix of every host path in the fixture.
    pub(crate) fn scratch(&self) -> &Path {
        self.scratch.path()
    }

    /// Open the repository (or, with `linked`, its linked worktree) for
    /// writing, ceilinged at the mount.
    pub(crate) fn write_repo(&self, linked: bool) -> WriteRepo {
        self.write_repo_at(if linked { &self.linked } else { &self.repo })
    }

    pub(crate) fn write_repo_at(&self, at: &Path) -> WriteRepo {
        let ceiling = self.mount.canonicalize().expect("canonical mount");
        let read = ReadRepo::discover("commit", at, &ceiling).expect("discover");
        WriteRepo::open(read, WriteToken::test()).expect("open")
    }

    /// The common directory, whichever repository is opened.
    pub(crate) fn common(&self) -> PathBuf {
        self.repo.join(".git")
    }

    /// The linked worktree's private git directory.
    pub(crate) fn linked_git_dir(&self) -> PathBuf {
        self.common().join("worktrees").join("wt")
    }

    /// Replace `dir` (relative to the common dir) with a link to `target`.
    pub(crate) fn link_dir_in_common(&self, rel: &str, target: &Path) {
        let at = self.common().join(rel);
        if let Some(parent) = at.parent() {
            std::fs::create_dir_all(parent).expect("parent");
        }
        if at.is_dir() {
            std::fs::remove_dir_all(&at).expect("clear");
        } else if at.symlink_metadata().is_ok() {
            std::fs::remove_file(&at).expect("clear");
        }
        std::os::unix::fs::symlink(target, &at).expect("plant link");
    }
}

/// One planted-symlink case.
pub(crate) struct Case {
    pub(crate) name: &'static str,
    /// Plants the link.
    pub(crate) plant: fn(&Fixture),
    /// Open from the linked worktree rather than the main one.
    pub(crate) from_linked: bool,
    /// The path a write would land on, relative to its store root.
    pub(crate) rel: &'static str,
    pub(crate) leaf: Leaf,
    /// The component the refusal must name, relative to the root.
    pub(crate) expect_path: &'static str,
    /// The root the refusal must name.
    pub(crate) expect_store: &'static str,
}

fn plant_objects_ab(fx: &Fixture) {
    fx.link_dir_in_common("objects/ab", &fx.outside.join("objects_ab"));
}
fn plant_refs_heads_sub(fx: &Fixture) {
    fx.link_dir_in_common("refs/heads/sub", &fx.outside.join("refs_sub"));
}
fn plant_logs_head(fx: &Fixture) {
    let at = fx.common().join("logs/HEAD");
    std::fs::remove_file(&at).expect("main logs/HEAD exists after a commit");
    std::os::unix::fs::symlink(fx.outside.join("logs_head"), &at).expect("plant");
}
fn plant_linked_logs_head(fx: &Fixture) {
    let dir = fx.linked_git_dir().join("logs");
    std::fs::create_dir_all(&dir).expect("logs dir");
    let at = dir.join("HEAD");
    let _ = std::fs::remove_file(&at);
    std::os::unix::fs::symlink(fx.outside.join("logs_head"), &at).expect("plant");
}

/// Phase 1b: add `verb` (the argv that writes to `rel`) to each row.
pub(crate) const CASES: &[Case] = &[
    Case {
        name: "objects/ab symlinked out",
        plant: plant_objects_ab,
        from_linked: false,
        rel: "objects/ab/cdef0123456789012345678901234567890a",
        leaf: Leaf::File,
        expect_path: "objects/ab",
        expect_store: "the common git directory",
    },
    Case {
        name: "refs/heads/sub symlinked out",
        plant: plant_refs_heads_sub,
        from_linked: false,
        rel: "refs/heads/sub/x.lock",
        leaf: Leaf::File,
        expect_path: "refs/heads/sub",
        expect_store: "the common git directory",
    },
    Case {
        name: "logs/HEAD symlinked out",
        plant: plant_logs_head,
        from_linked: false,
        rel: "logs/HEAD",
        leaf: Leaf::File,
        expect_path: "logs/HEAD",
        expect_store: "the git directory",
    },
    Case {
        name: "linked: refs/heads/sub in the common dir",
        plant: plant_refs_heads_sub,
        from_linked: true,
        rel: "refs/heads/sub/x.lock",
        leaf: Leaf::File,
        expect_path: "refs/heads/sub",
        expect_store: "the common git directory",
    },
    Case {
        name: "linked: objects/ab in the common dir",
        plant: plant_objects_ab,
        from_linked: true,
        rel: "objects/ab/cdef0123456789012345678901234567890a",
        leaf: Leaf::File,
        expect_path: "objects/ab",
        expect_store: "the common git directory",
    },
    Case {
        name: "linked: the worktree's own logs/HEAD",
        plant: plant_linked_logs_head,
        from_linked: true,
        rel: "logs/HEAD",
        leaf: Leaf::File,
        expect_path: "logs/HEAD",
        expect_store: "the git directory",
    },
];
