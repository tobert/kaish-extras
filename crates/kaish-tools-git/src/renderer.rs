//! The patch renderer boundary: an embedder-supplied replacement for the
//! text `git diff --patch` prints (`textdiff` only).
//!
//! One call per invocation, with every changed file in it, so a renderer can
//! spend one output budget across the whole change set rather than one per
//! file. The renderer receives each file's two sides as **bytes** and decides
//! for itself what is text and what is binary; this build's own NUL-byte
//! heuristic is not applied to what it receives.
//!
//! What it does not receive is anything this build declined to read: a file
//! with a side over the embedder's `max_blob_bytes` arrives with
//! `before`/`after` both `None`, both sizes, and
//! [`ChangedFile::over_max_blob_bytes`] set — so installing a renderer can
//! never cause a read the caps would have refused.
//!
//! This module depends on nothing else of ours, for the same reason
//! `config.rs` does not: [`crate::GitConfig`] holds the renderer, and the
//! config must stay a leaf of the crate's dependency graph.

/// A replacement for this build's unified-patch text, installed with
/// [`crate::GitConfig::with_patch_renderer`].
///
/// Called once per `git diff --patch` invocation, never under `--json` (which
/// reports this build's own model and hunks) and never without `--patch`.
/// It runs on the thread that read the repository, inside the same blocking
/// section, so it may do CPU-bound work directly.
///
/// An `Err` fails the call with exit 1 and the error's message — and each of
/// its `source()`s — on stderr. There is no fallback to this build's own
/// renderer: an embedder that installed one asked for its output or none.
/// A panic is not caught.
///
/// ```no_run
/// use std::sync::Arc;
/// use kaish_tools_git::{
///     ChangedFile, GitConfig, PatchOptions, PatchRenderError, PatchRenderer, RenderedPatch,
/// };
///
/// struct Names;
///
/// impl PatchRenderer for Names {
///     fn render(
///         &self,
///         files: &[ChangedFile<'_>],
///         _opts: &PatchOptions,
///     ) -> Result<RenderedPatch, PatchRenderError> {
///         let mut text = String::new();
///         for file in files {
///             let path = file.new_path.or(file.old_path).unwrap_or_default();
///             text.push_str(&format!("{:?} {path}\n", file.kind));
///         }
///         Ok(RenderedPatch::new(text))
///     }
/// }
///
/// # fn demo() -> Result<(), Box<dyn std::error::Error>> {
/// let git = kaish_tools_git::tool(GitConfig::read_only().with_patch_renderer(Arc::new(Names)))?;
/// # let _ = git;
/// # Ok(()) }
/// ```
pub trait PatchRenderer: Send + Sync {
    /// Render the change set. `files` is in path order, already cut to
    /// `--limit`; the result's stderr says when it was cut.
    fn render(
        &self,
        files: &[ChangedFile<'_>],
        opts: &PatchOptions,
    ) -> Result<RenderedPatch, PatchRenderError>;
}

/// What a [`PatchRenderer`] may fail with. Boxed so a renderer can return its
/// own engine's error with `?`.
pub type PatchRenderError = Box<dyn std::error::Error + Send + Sync + 'static>;

/// What happened to one path.
///
/// The same five ideas `git diff`'s `status` field reports
/// ([`crate::EntryStatus`]), without the variants a diff never produces.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum ChangeKind {
    /// Absent on the old side.
    Added,
    /// Absent on the new side.
    Deleted,
    /// Content, mode, or both changed at the same path. A mode-only change
    /// has identical `before` and `after`.
    Modified,
    /// The same content at a new path. This build detects exact renames only,
    /// so `similarity` is always `100` and `before` equals `after`; a file
    /// that was moved *and* edited arrives as a `Deleted` plus an `Added`.
    Renamed {
        /// Percent of content shared by the two sides. Always `100` here.
        similarity: u8,
    },
    /// The path changed type: a file, a symlink, or a submodule became
    /// another of the three. A working-tree path that became a directory is
    /// one too, and has `new_mode`, `after` and `new_size` all `None`.
    TypeChanged,
}

/// One changed path, as a [`PatchRenderer`] receives it.
///
/// Paths are repository-relative and slash-separated, with no `a/` or `b/`
/// prefix. A path whose bytes are not UTF-8 carries U+FFFD where they were
/// (docs/issues.md, T1).
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct ChangedFile<'a> {
    /// The path on the old side, or `None` for [`ChangeKind::Added`].
    pub old_path: Option<&'a str>,
    /// The path on the new side, or `None` for [`ChangeKind::Deleted`]. A
    /// [`ChangeKind::TypeChanged`] path that became a working-tree directory
    /// keeps it, with no mode, content or size beside it.
    pub new_path: Option<&'a str>,
    /// What happened to the path.
    pub kind: ChangeKind,
    /// The six-digit octal mode on the old side (`100644`, `100755`,
    /// `120000`, `160000`), or `None` where the path is absent.
    pub old_mode: Option<&'a str>,
    /// The six-digit octal mode on the new side, or `None` where the path is
    /// absent.
    pub new_mode: Option<&'a str>,
    /// The full hex object id on the old side, or `None` where the path is
    /// absent or the side is the working tree — working-tree content has no
    /// object id. A `160000` side names a commit in the submodule.
    pub old_oid: Option<&'a str>,
    /// The full hex object id on the new side, under the same rule.
    pub new_oid: Option<&'a str>,
    /// The old side's content. `None` where the path is absent, where the
    /// side is a submodule (`160000`, no blob in this repository), and on
    /// both sides when [`Self::over_max_blob_bytes`] is set. A symlink's
    /// content is its target path.
    pub before: Option<&'a [u8]>,
    /// The new side's content, under the same rule as `before`.
    pub after: Option<&'a [u8]>,
    /// The old side's size in bytes, or `None` where the path is absent or
    /// the side is a submodule. Set even when the content was not read.
    pub old_size: Option<u64>,
    /// The new side's size in bytes, under the same rule as `old_size`.
    pub new_size: Option<u64>,
    /// Whether a side's blob is over the embedder's `max_blob_bytes`. When
    /// it is, **neither** side's content was passed — a renderer cannot diff
    /// half a file — and each present side's size still is. One exception
    /// to the cap: a working-tree symlink's target is read whatever its
    /// length, as `git diff`'s hashing pass reads it, so it arrives as
    /// content with this `false` even when its size is over the cap. A
    /// target is bounded by the platform's path length.
    pub over_max_blob_bytes: bool,
}

/// The `git diff --patch` options a [`PatchRenderer`] is asked to honor.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct PatchOptions {
    /// Context lines around each change: `--context <N>`, default 3.
    pub context: usize,
    /// The embedder's `max_hunk_bytes_per_file` — the budget this build's own
    /// renderer spends on one file's hunk text. A renderer that applies one
    /// budget across files may use it to size that budget, or ignore it.
    pub max_hunk_bytes_per_file: u64,
    /// Whether word-level changes were asked for. Always `false`: no flag
    /// sets it yet. Declared so a renderer can be written against it now.
    pub word_diff: bool,
}

impl Default for PatchOptions {
    /// What `git diff --patch` asks for with no options under the default
    /// `Limits`: 3 context lines, a 256 KiB hunk budget, no word diff.
    fn default() -> Self {
        Self {
            context: 3,
            // `Limits::default()`'s value, written out so this module stays a
            // leaf; `options_default_matches_the_limits_default` holds them
            // together.
            max_hunk_bytes_per_file: 256 * 1024,
            word_diff: false,
        }
    }
}

/// A [`PatchRenderer`]'s answer.
#[derive(Debug, Clone, PartialEq)]
#[non_exhaustive]
pub struct RenderedPatch {
    /// What stdout shows, verbatim.
    pub text: String,
    /// Structured data for the result's `data` slot: the pipeline's
    /// structured input to the next stage (`git diff --patch | jq …`).
    /// `x=$(git diff --patch)` still binds `text`, because this tool does not
    /// declare typed substitution, and `--json` never reaches a renderer.
    pub data: Option<serde_json::Value>,
}

impl RenderedPatch {
    /// A patch that is text only.
    pub fn new(text: impl Into<String>) -> Self {
        Self {
            text: text.into(),
            data: None,
        }
    }

    /// Attach structured data; see [`Self::data`].
    pub fn with_data(mut self, data: serde_json::Value) -> Self {
        self.data = Some(data);
        self
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn options_default_matches_the_limits_default() {
        assert_eq!(
            PatchOptions::default().max_hunk_bytes_per_file,
            crate::Limits::default().max_hunk_bytes_per_file
        );
    }
}
