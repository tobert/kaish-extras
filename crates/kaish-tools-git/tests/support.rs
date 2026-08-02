//! Fixture repositories, a `.git` fingerprint, and a deliberately hostile
//! test `ToolCtx`.
//!
//! **Real git on PATH is the oracle.** Fixtures are built by shelling out to
//! `git`, which is fine *in tests* and forbidden in the crate: it means a
//! fixture is whatever real git actually produces (a real pack, a real
//! `worktrees/` entry, a real `packed-refs`) rather than whatever we believed
//! git produces. The crate itself links no spawn machinery at all — that is
//! what the CI tripwires assert.
//!
//! Included with `#[path = "support.rs"] mod support;` rather than compiled as
//! its own test target (see `autotests = false` in Cargo.toml).

#![allow(dead_code)] // each test binary uses a subset of the harness

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Arc;
use std::time::SystemTime;

use async_trait::async_trait;
use sha2::{Digest, Sha256};

use kaish_tool_api::{KernelBackend, ToolCtx};
use kaish_types::backend::{
    BackendResult, MountInfo, PatchOp, ReadRange, ToolInfo, ToolResult, WriteMode,
};
use kaish_types::{DirEntry, OutputFormat, ToolArgs, Value};

// ═══════════════════════════════════════════════════════════════════════════
// Fixture repositories
// ═══════════════════════════════════════════════════════════════════════════

/// A temporary directory holding one or more fixture repositories.
///
/// Keeps the `TempDir` alive; dropping the fixture removes everything.
pub struct Fixture {
    dir: tempfile::TempDir,
}

impl Fixture {
    /// An empty scratch directory to build repositories under.
    pub fn empty() -> Self {
        Self {
            dir: tempfile::Builder::new()
                .prefix("kaish-tools-git-")
                .tempdir()
                .expect("tempdir"),
        }
    }

    /// The fixture's root on the host filesystem, with symlinks resolved.
    ///
    /// Resolved because macOS hands out `/var/...` temp dirs that are really
    /// `/private/var/...`; gix normalizes discovered paths, so an unresolved
    /// root would make every path comparison in these tests a false failure.
    pub fn root(&self) -> PathBuf {
        std::fs::canonicalize(self.dir.path()).expect("canonicalize fixture root")
    }

    /// Absolute path to `rel` under the fixture root.
    pub fn path(&self, rel: impl AsRef<Path>) -> PathBuf {
        self.root().join(rel)
    }
}

/// Run `git` in `cwd` with a hermetic environment, asserting success.
///
/// Every ambient input git would otherwise read is pinned or removed: no user
/// config, no system config, no host identity, a fixed clock. A fixture that
/// depended on the developer's `~/.gitconfig` would pass on one machine and
/// fail in CI.
pub fn git(cwd: &Path, args: &[&str]) -> String {
    let out = Command::new("git")
        .args(args)
        .current_dir(cwd)
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_SYSTEM", "/dev/null")
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_TERMINAL_PROMPT", "0")
        .env("GIT_AUTHOR_NAME", "Fixture Author")
        .env("GIT_AUTHOR_EMAIL", "author@example.invalid")
        .env("GIT_COMMITTER_NAME", "Fixture Committer")
        .env("GIT_COMMITTER_EMAIL", "committer@example.invalid")
        .env("GIT_AUTHOR_DATE", "2026-08-01T10:00:00+00:00")
        .env("GIT_COMMITTER_DATE", "2026-08-01T10:00:00+00:00")
        .output()
        .unwrap_or_else(|e| panic!("git {args:?} in {}: {e}", cwd.display()));
    assert!(
        out.status.success(),
        "git {args:?} in {} failed ({}): {}",
        cwd.display(),
        out.status,
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8_lossy(&out.stdout).trim().to_string()
}

/// Whether real git is on PATH.
///
/// The oracle is a hard requirement, not an optional extra: a suite that
/// silently skipped its own fixtures would report green while proving
/// nothing. Tests call [`require_git`] instead of branching on this.
pub fn have_git() -> bool {
    Command::new("git")
        .arg("--version")
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}

/// Panic with an actionable message when git is missing.
pub fn require_git() {
    assert!(
        have_git(),
        "these tests use real git on PATH as the fixture oracle; install git \
         (CI's ubuntu-latest already has it)"
    );
}

/// Write `contents` to `rel` under `root`, creating parents.
pub fn write_file(root: &Path, rel: &str, contents: &str) {
    let path = root.join(rel);
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).expect("create parent dir");
    }
    std::fs::write(&path, contents).unwrap_or_else(|e| panic!("write {}: {e}", path.display()));
}

/// The repository the `.git` fingerprint test runs against.
///
/// Everything D.4 names, so the fingerprint has something to catch:
/// packed objects (the `gc` that builds a pack and a pack index), multiple
/// branches, `packed-refs`, a dirty working tree (staged, unstaged and
/// untracked all at once), and a registered linked worktree.
pub struct RichRepo {
    fixture: Fixture,
    /// The main working tree's root.
    pub root: PathBuf,
    /// The linked worktree's root.
    pub linked_worktree: PathBuf,
    /// The linked worktree's registered name.
    pub linked_name: String,
}

impl RichRepo {
    /// Build the fixture. Requires real git on PATH.
    pub fn build() -> Self {
        require_git();
        let fixture = Fixture::empty();
        let root = fixture.path("main");
        std::fs::create_dir_all(&root).expect("create repo dir");

        git(&root, &["init", "--initial-branch=main", "--quiet"]);
        // Off by default in git ≥ 2.34's `gc`, but a repository that grew a
        // commit-graph between the two fingerprints would look like a write
        // caused by us. Pin it off so a failure means what it says.
        git(&root, &["config", "gc.writeCommitGraph", "false"]);

        write_file(&root, "README.md", "fixture repository\n");
        write_file(&root, "src/lib.rs", "pub fn one() -> u32 { 1 }\n");
        git(&root, &["add", "."]);
        git(&root, &["commit", "-m", "initial commit", "--quiet"]);

        write_file(&root, "src/lib.rs", "pub fn one() -> u32 { 1 }\npub fn two() -> u32 { 2 }\n");
        git(&root, &["add", "src/lib.rs"]);
        git(&root, &["commit", "-m", "add two", "--quiet"]);

        // A second branch with its own commit, so refs are not a single line.
        git(&root, &["branch", "feature/side"]);
        git(&root, &["checkout", "--quiet", "feature/side"]);
        write_file(&root, "src/side.rs", "pub fn side() {}\n");
        git(&root, &["add", "src/side.rs"]);
        git(&root, &["commit", "-m", "side branch work", "--quiet"]);
        git(&root, &["checkout", "--quiet", "main"]);
        git(&root, &["tag", "v0.1.0"]);

        // Pack the objects and write packed-refs: the fingerprint must cover
        // a repository whose objects are NOT all loose, because pack-index
        // rebuilds are one of the write classes D.4 exists to catch.
        git(&root, &["gc", "--quiet", "--aggressive"]);
        git(&root, &["pack-refs", "--all"]);

        // A linked worktree, registered under $GIT_DIR/worktrees/.
        let linked_name = "wt-side".to_string();
        let linked_worktree = fixture.path(&linked_name);
        git(
            &root,
            &[
                "worktree",
                "add",
                "--quiet",
                linked_worktree.to_str().expect("utf-8 path"),
                "feature/side",
            ],
        );

        // Dirty the working tree in all three ways at once.
        write_file(&root, "README.md", "fixture repository\nstaged line\n");
        git(&root, &["add", "README.md"]);
        write_file(&root, "src/lib.rs", "pub fn one() -> u32 { 1 }\n// unstaged edit\n");
        write_file(&root, "untracked.txt", "not tracked\n");

        Self {
            fixture,
            root,
            linked_worktree,
            linked_name,
        }
    }

    /// The main repository's `.git` directory.
    pub fn git_dir(&self) -> PathBuf {
        self.root.join(".git")
    }

    /// The fixture's scratch root — the parent of both working trees.
    pub fn scratch(&self) -> PathBuf {
        self.fixture.root()
    }

    /// The oid real git reports for `rev`, for oracle comparisons.
    pub fn rev_parse(&self, rev: &str) -> String {
        git(&self.root, &["rev-parse", rev])
    }
}

// ═══════════════════════════════════════════════════════════════════════════
// The `.git` fingerprint (architecture.md D.4)
// ═══════════════════════════════════════════════════════════════════════════

/// One entry in a `.git` fingerprint.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Entry {
    /// Byte length, or `None` for a directory.
    pub size: Option<u64>,
    /// Last modification time as the host reports it.
    pub mtime: Option<SystemTime>,
    /// SHA-256 of the file's bytes (of the link target, for a symlink), or
    /// `None` for a directory.
    pub hash: Option<String>,
}

/// A recursive fingerprint of a `.git` directory: every path, its size, its
/// mtime, and its content hash.
///
/// Directories are recorded too, with their mtime — a lock file that is
/// created and then removed leaves no file behind but does move its
/// directory's mtime, and that is a write.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Fingerprint {
    /// Entries keyed by path relative to the fingerprinted root.
    pub entries: BTreeMap<PathBuf, Entry>,
}

impl Fingerprint {
    /// Fingerprint `root` recursively.
    pub fn take(root: &Path) -> Self {
        let mut entries = BTreeMap::new();
        walk(root, root, &mut entries);
        Self { entries }
    }

    /// Every difference from `before` to `self`, as human-readable lines.
    ///
    /// Returning the differences rather than a bool is what makes a failure
    /// actionable: "`.git/index` mtime changed" names the bug, "not equal"
    /// starts an investigation.
    pub fn differences_from(&self, before: &Fingerprint) -> Vec<String> {
        let mut diffs = Vec::new();
        for (path, old) in &before.entries {
            match self.entries.get(path) {
                None => diffs.push(format!("{} was REMOVED", path.display())),
                Some(new) => {
                    if new.hash != old.hash {
                        diffs.push(format!(
                            "{} content changed ({:?} -> {:?})",
                            path.display(),
                            old.hash,
                            new.hash
                        ));
                    }
                    if new.size != old.size {
                        diffs.push(format!(
                            "{} size changed ({:?} -> {:?})",
                            path.display(),
                            old.size,
                            new.size
                        ));
                    }
                    if new.mtime != old.mtime {
                        diffs.push(format!(
                            "{} mtime changed ({:?} -> {:?})",
                            path.display(),
                            old.mtime,
                            new.mtime
                        ));
                    }
                }
            }
        }
        for path in self.entries.keys() {
            if !before.entries.contains_key(path) {
                diffs.push(format!("{} is a NEW path", path.display()));
            }
        }
        diffs
    }
}

fn walk(root: &Path, dir: &Path, out: &mut BTreeMap<PathBuf, Entry>) {
    let read = std::fs::read_dir(dir).unwrap_or_else(|e| panic!("read_dir {}: {e}", dir.display()));
    for entry in read {
        let entry = entry.expect("dir entry");
        let path = entry.path();
        let rel = path
            .strip_prefix(root)
            .expect("entry is under root")
            .to_path_buf();
        let meta = std::fs::symlink_metadata(&path)
            .unwrap_or_else(|e| panic!("lstat {}: {e}", path.display()));
        let mtime = meta.modified().ok();

        if meta.is_dir() {
            out.insert(
                rel,
                Entry {
                    size: None,
                    mtime,
                    hash: None,
                },
            );
            walk(root, &path, out);
        } else {
            let bytes = if meta.is_symlink() {
                std::fs::read_link(&path)
                    .unwrap_or_else(|e| panic!("readlink {}: {e}", path.display()))
                    .into_os_string()
                    .into_encoded_bytes()
            } else {
                std::fs::read(&path).unwrap_or_else(|e| panic!("read {}: {e}", path.display()))
            };
            let mut hasher = Sha256::new();
            hasher.update(&bytes);
            out.insert(
                rel,
                Entry {
                    size: Some(meta.len()),
                    mtime,
                    hash: Some(format!("{:x}", hasher.finalize())),
                },
            );
        }
    }
}

// ═══════════════════════════════════════════════════════════════════════════
// A test `ToolCtx` that refuses to do I/O
// ═══════════════════════════════════════════════════════════════════════════

/// A `KernelBackend` that answers only the two path-resolution questions the
/// git tool is allowed to ask, and panics on everything else.
///
/// The panics are the point. D.5 says the native git path goes around the VFS
/// entirely and reaches the host through `resolve_real_path`; this backend
/// turns that sentence into something a test can falsify. A verb that ever
/// reached for `backend().read()` would fail the suite loudly instead of
/// quietly acquiring a second, unaudited I/O path.
pub struct StrictBackend {
    mounts: Vec<MountInfo>,
    /// (vfs mount path, real root) pairs, longest VFS path first.
    mapping: Vec<(PathBuf, PathBuf)>,
}

impl StrictBackend {
    /// Build a backend whose mounts map VFS paths onto real roots.
    pub fn new(mounts: impl IntoIterator<Item = (PathBuf, PathBuf)>) -> Self {
        let mut mapping: Vec<(PathBuf, PathBuf)> = mounts.into_iter().collect();
        // Longest VFS prefix first, so nested mounts resolve to the innermost.
        mapping.sort_by_key(|(vfs, _)| std::cmp::Reverse(vfs.components().count()));
        let mounts = mapping
            .iter()
            .map(|(vfs, _)| MountInfo {
                path: vfs.clone(),
                read_only: false,
                resident_bytes: None,
            })
            .collect();
        Self { mounts, mapping }
    }

    /// A single mount of `real_root` at `vfs_root`.
    pub fn single(vfs_root: impl Into<PathBuf>, real_root: impl Into<PathBuf>) -> Self {
        Self::new([(vfs_root.into(), real_root.into())])
    }
}

/// Panic naming the method a git verb must never have called.
macro_rules! forbidden {
    ($what:literal) => {
        panic!(
            "kaish-tools-git called KernelBackend::{} — the native git path \
             reaches the host through resolve_real_path only (architecture.md \
             D.5), so no verb may do VFS I/O",
            $what
        )
    };
}

#[async_trait]
impl KernelBackend for StrictBackend {
    async fn read(&self, _path: &Path, _range: Option<ReadRange>) -> BackendResult<Vec<u8>> {
        forbidden!("read")
    }
    async fn write(&self, _path: &Path, _content: &[u8], _mode: WriteMode) -> BackendResult<()> {
        forbidden!("write")
    }
    async fn append(&self, _path: &Path, _content: &[u8]) -> BackendResult<()> {
        forbidden!("append")
    }
    async fn patch(&self, _path: &Path, _ops: &[PatchOp]) -> BackendResult<()> {
        forbidden!("patch")
    }
    async fn list(&self, _path: &Path) -> BackendResult<Vec<DirEntry>> {
        forbidden!("list")
    }
    async fn stat(&self, _path: &Path) -> BackendResult<DirEntry> {
        forbidden!("stat")
    }
    async fn mkdir(&self, _path: &Path) -> BackendResult<()> {
        forbidden!("mkdir")
    }
    async fn set_mtime(&self, _path: &Path, _mtime: SystemTime) -> BackendResult<()> {
        forbidden!("set_mtime")
    }
    async fn remove(&self, _path: &Path, _recursive: bool) -> BackendResult<()> {
        forbidden!("remove")
    }
    async fn rename(&self, _from: &Path, _to: &Path) -> BackendResult<()> {
        forbidden!("rename")
    }
    async fn exists(&self, _path: &Path) -> bool {
        forbidden!("exists")
    }
    async fn lstat(&self, _path: &Path) -> BackendResult<DirEntry> {
        forbidden!("lstat")
    }
    async fn read_link(&self, _path: &Path) -> BackendResult<PathBuf> {
        forbidden!("read_link")
    }
    async fn symlink(&self, _target: &Path, _link: &Path) -> BackendResult<()> {
        forbidden!("symlink")
    }
    async fn call_tool(
        &self,
        _name: &str,
        _args: ToolArgs,
        _ctx: &mut dyn ToolCtx,
    ) -> BackendResult<ToolResult> {
        forbidden!("call_tool")
    }
    async fn list_tools(&self) -> BackendResult<Vec<ToolInfo>> {
        forbidden!("list_tools")
    }
    async fn get_tool(&self, _name: &str) -> BackendResult<Option<ToolInfo>> {
        forbidden!("get_tool")
    }

    fn read_only(&self) -> bool {
        false
    }

    fn backend_type(&self) -> &str {
        "strict-test"
    }

    fn mounts(&self) -> Vec<MountInfo> {
        self.mounts.clone()
    }

    fn resolve_real_path(&self, path: &Path) -> Option<PathBuf> {
        for (vfs, real) in &self.mapping {
            if let Ok(rest) = path.strip_prefix(vfs) {
                return Some(real.join(rest));
            }
        }
        None
    }
}

/// A backend where nothing maps to a real path — a memory mount, in effect.
///
/// `resolve_real_path` always returns `None`, which is what E.2's exit-4
/// `NotRealPath` refusal is for.
pub struct VirtualBackend {
    mounts: Vec<MountInfo>,
}

impl VirtualBackend {
    /// A single virtual mount at `vfs_root`.
    pub fn single(vfs_root: impl Into<PathBuf>) -> Self {
        Self {
            mounts: vec![MountInfo {
                path: vfs_root.into(),
                read_only: false,
                resident_bytes: None,
            }],
        }
    }
}

#[async_trait]
impl KernelBackend for VirtualBackend {
    async fn read(&self, _path: &Path, _range: Option<ReadRange>) -> BackendResult<Vec<u8>> {
        forbidden!("read")
    }
    async fn write(&self, _path: &Path, _content: &[u8], _mode: WriteMode) -> BackendResult<()> {
        forbidden!("write")
    }
    async fn append(&self, _path: &Path, _content: &[u8]) -> BackendResult<()> {
        forbidden!("append")
    }
    async fn patch(&self, _path: &Path, _ops: &[PatchOp]) -> BackendResult<()> {
        forbidden!("patch")
    }
    async fn list(&self, _path: &Path) -> BackendResult<Vec<DirEntry>> {
        forbidden!("list")
    }
    async fn stat(&self, _path: &Path) -> BackendResult<DirEntry> {
        forbidden!("stat")
    }
    async fn mkdir(&self, _path: &Path) -> BackendResult<()> {
        forbidden!("mkdir")
    }
    async fn set_mtime(&self, _path: &Path, _mtime: SystemTime) -> BackendResult<()> {
        forbidden!("set_mtime")
    }
    async fn remove(&self, _path: &Path, _recursive: bool) -> BackendResult<()> {
        forbidden!("remove")
    }
    async fn rename(&self, _from: &Path, _to: &Path) -> BackendResult<()> {
        forbidden!("rename")
    }
    async fn exists(&self, _path: &Path) -> bool {
        forbidden!("exists")
    }
    async fn lstat(&self, _path: &Path) -> BackendResult<DirEntry> {
        forbidden!("lstat")
    }
    async fn read_link(&self, _path: &Path) -> BackendResult<PathBuf> {
        forbidden!("read_link")
    }
    async fn symlink(&self, _target: &Path, _link: &Path) -> BackendResult<()> {
        forbidden!("symlink")
    }
    async fn call_tool(
        &self,
        _name: &str,
        _args: ToolArgs,
        _ctx: &mut dyn ToolCtx,
    ) -> BackendResult<ToolResult> {
        forbidden!("call_tool")
    }
    async fn list_tools(&self) -> BackendResult<Vec<ToolInfo>> {
        forbidden!("list_tools")
    }
    async fn get_tool(&self, _name: &str) -> BackendResult<Option<ToolInfo>> {
        forbidden!("get_tool")
    }

    fn read_only(&self) -> bool {
        true
    }

    fn backend_type(&self) -> &str {
        "virtual-test"
    }

    fn mounts(&self) -> Vec<MountInfo> {
        self.mounts.clone()
    }

    fn resolve_real_path(&self, _path: &Path) -> Option<PathBuf> {
        None
    }
}

/// A minimal `ToolCtx` over a backend and a cwd.
pub struct TestCtx {
    backend: Arc<dyn KernelBackend>,
    cwd: PathBuf,
    vars: BTreeMap<String, Value>,
    /// The output format the tool asked for, if any — how a test observes
    /// `--json` having been honored.
    pub output_format: Option<OutputFormat>,
}

impl TestCtx {
    /// A context with `backend` mounted and the given VFS cwd.
    pub fn new(backend: Arc<dyn KernelBackend>, cwd: impl Into<PathBuf>) -> Self {
        Self {
            backend,
            cwd: cwd.into(),
            vars: BTreeMap::new(),
            output_format: None,
        }
    }
}

impl ToolCtx for TestCtx {
    fn backend(&self) -> &Arc<dyn KernelBackend> {
        &self.backend
    }

    fn cwd(&self) -> &Path {
        &self.cwd
    }

    fn resolve_path(&self, path: &str) -> PathBuf {
        // Lexical only, like the kernel's: never touches the real filesystem.
        let joined = if path.starts_with('/') {
            PathBuf::from(path)
        } else {
            self.cwd.join(path)
        };
        let mut out = PathBuf::new();
        for component in joined.components() {
            match component {
                std::path::Component::ParentDir => {
                    out.pop();
                }
                std::path::Component::CurDir => {}
                other => out.push(other),
            }
        }
        out
    }

    fn var(&self, name: &str) -> Option<Value> {
        self.vars.get(name).cloned()
    }

    fn set_var(&mut self, name: &str, value: Value) {
        self.vars.insert(name.to_string(), value);
    }

    fn set_output_format(&mut self, format: OutputFormat) {
        self.output_format = Some(format);
    }

    fn as_any(&self) -> &dyn std::any::Any {
        self
    }

    fn as_any_mut(&mut self) -> &mut dyn std::any::Any {
        self
    }
}
