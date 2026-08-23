//! The `core.worktree` guard's needle, made non-vacuous — docs/issues.md P9.
//!
//! `tests/hostile_repo.rs`'s `work_dir_is_bounded_by_discovery` documents (and
//! checks) an invariant the `work_dir` ceiling check leans on: nothing in
//! `src/` reads `core.worktree`, so `work_dir` stays bounded transitively
//! through `git_dir` rather than needing its own fixture (see that test's own
//! doc for the full argument). What it checks it for is a single literal
//! needle, case-folded: `"core.worktree"`.
//!
//! That needle is already blind to a call this crate's own config accessor
//! supports today. [`kaish_tools_git`]'s (private) `ReadRepo::config_values`
//! takes `section` and `key` as two separate `&str` arguments — see
//! `src/repo.rs`'s `pub(crate) fn config_values(&self, section: &str, key:
//! &str)`. A future `self.config_values("core", "worktree")` reads exactly
//! the config key the invariant is about, but contains the literal substring
//! `"core.worktree"` nowhere — it is two separate string literals, `"core"`
//! and `"worktree"`. The single-needle guard would stay green while the
//! invariant it protects had just gone false: a vacuous pass, not a real one.
//!
//! This file adds the second needle the original guard is missing, and proves
//! with planted fixtures — not reasoning — that it closes the gap:
//!
//! - [`the_two_argument_form_was_invisible_to_the_original_needle`] plants the
//!   exact call shape (`config_values("core", "worktree")`) in a throwaway
//!   file and shows the *original* single-needle check (reimplemented
//!   verbatim from `hostile_repo.rs`, not imported — it is a private `#[test]`
//!   fn in a separate integration-test crate and cannot be called from here)
//!   finds nothing. That is the vacuous pass, demonstrated rather than
//!   asserted.
//! - [`the_two_argument_form_is_now_caught`] runs the same planted fixture
//!   through [`scan_for_worktree_config_reads`], which adds the second
//!   needle, and shows it *is* found.
//! - [`the_single_string_form_is_still_caught`] is the non-regression check:
//!   the original spelling must still be found.
//! - [`unrelated_config_calls_do_not_trigger_either_needle`] is the negative
//!   control: legitimate calls this crate already makes
//!   (`config_values("branch", "merge")`, `config.string("core.repositoryformatversion")`)
//!   must not trip either needle, or the guard would be worthless noise.
//! - [`the_real_crate_src_does_not_read_core_worktree`] is
//!   `work_dir_is_bounded_by_discovery`'s own real check, re-run with the
//!   fixed two-needle scanner over the actual `src/` tree.
//!
//! Because `tests/hostile_repo.rs` is owned by a sibling agent's work in
//! flight, the fix lives here as a second, stricter scanner rather than as an
//! edit to that file's existing test. Whoever next touches
//! `work_dir_is_bounded_by_discovery` should fold the two-argument needle in
//! there and retire the duplication; until then, this file is the one that
//! actually catches the case P9 describes.

use std::path::{Path, PathBuf};

/// Scan every `.rs` file under `root` for a read of `core.worktree`, in
/// either spelling this crate's own config accessors support: the single
/// dotted string (`config.string("core.worktree")`, the style `check_format_version`
/// and friends in `src/repo.rs` use), or the two-argument form
/// (`config_values("core", "worktree")`, the style `ReadRepo::config_values`
/// takes). Returns every matching file's path relative to `root`.
fn scan_for_worktree_config_reads(root: &Path) -> Vec<PathBuf> {
    let mut found = Vec::new();
    let mut stack = vec![root.to_path_buf()];
    while let Some(dir) = stack.pop() {
        for entry in std::fs::read_dir(&dir).unwrap_or_else(|e| panic!("read_dir {}: {e}", dir.display())) {
            let path = entry.expect("dir entry").path();
            if path.is_dir() {
                stack.push(path);
            } else if path.extension().is_some_and(|e| e == "rs") {
                let text = std::fs::read_to_string(&path)
                    .unwrap_or_else(|e| panic!("read {}: {e}", path.display()))
                    .to_ascii_lowercase();
                if needle_dotted_form(&text) || needle_two_argument_form(&text) {
                    found.push(path.strip_prefix(root).expect("under root").to_path_buf());
                }
            }
        }
    }
    found
}

/// The original guard's only needle: the literal, case-folded dotted key.
fn needle_dotted_form(lowercased_text: &str) -> bool {
    lowercased_text.contains("\"core.worktree\"")
}

/// The needle P9 says is missing: `section` and `key` passed as two separate
/// string-literal arguments, close enough together to be one call rather than
/// two unrelated occurrences elsewhere in the file. A window of 80 bytes
/// comfortably covers `config_values("core", "worktree")` however rustfmt
/// wraps it, without being so wide it starts matching across unrelated code.
fn needle_two_argument_form(lowercased_text: &str) -> bool {
    const WINDOW: usize = 80;
    let mut search_from = 0;
    while let Some(offset) = lowercased_text[search_from..].find("\"core\"") {
        let start = search_from + offset;
        let end = (start + WINDOW).min(lowercased_text.len());
        if lowercased_text[start..end].contains("\"worktree\"") {
            return true;
        }
        search_from = start + 1;
    }
    false
}

/// Write `contents` to a single throwaway `.rs` file under a fresh temp
/// directory, and return the directory (kept alive by the caller) plus the
/// file's path.
fn planted_fixture(contents: &str) -> (tempfile::TempDir, PathBuf) {
    let dir = tempfile::Builder::new()
        .prefix("kaish-git-config-needle-")
        .tempdir()
        .expect("tempdir");
    let file = dir.path().join("planted.rs");
    std::fs::write(&file, contents).expect("write planted fixture");
    (dir, file)
}

/// The vacuous pass, demonstrated: the exact call shape a future
/// `core.worktree` reader through this crate's two-argument accessor would
/// take, checked against the *original* guard's only needle.
#[test]
fn the_two_argument_form_was_invisible_to_the_original_needle() {
    let (_dir, file) = planted_fixture(
        r#"fn f(&self) { let v = self.config_values("core", "worktree"); }"#,
    );
    let text = std::fs::read_to_string(&file).expect("read fixture").to_ascii_lowercase();
    assert!(
        !needle_dotted_form(&text),
        "the planted two-argument call must NOT contain the literal \
         \"core.worktree\" — if it does, this fixture no longer demonstrates \
         the gap P9 describes"
    );
}

/// The fix: the same planted call, found by the scanner with the second
/// needle added.
#[test]
fn the_two_argument_form_is_now_caught() {
    let (_dir, _file) = planted_fixture(
        r#"fn f(&self) { let v = self.config_values("core", "worktree"); }"#,
    );
    let root = _dir.path();
    let found = scan_for_worktree_config_reads(root);
    assert_eq!(
        found,
        vec![PathBuf::from("planted.rs")],
        "the two-argument form must be caught by the widened scanner"
    );
}

/// Non-regression: the original spelling must still be caught.
#[test]
fn the_single_string_form_is_still_caught() {
    let (_dir, _file) = planted_fixture(
        r#"fn f(&self) { let v = self.config.string("core.worktree"); }"#,
    );
    let root = _dir.path();
    let found = scan_for_worktree_config_reads(root);
    assert_eq!(
        found,
        vec![PathBuf::from("planted.rs")],
        "the single dotted-string form must still be caught"
    );
}

/// Negative control: calls this crate already makes legitimately must not
/// trip either needle. Modeled directly on real call sites in `src/repo.rs` —
/// `check_format_version` reads `core.repositoryformatversion`, and a
/// same-shaped two-argument call reads an unrelated key
/// (`branch.<name>.merge`, the shape `RefsRepo`'s fixtures set up in
/// `support.rs`). A control that fired on either of these would be a defect
/// in the control, not a reason to loosen the needles.
#[test]
fn unrelated_config_calls_do_not_trigger_either_needle() {
    let (_dir, _file) = planted_fixture(
        r#"
        fn check_format_version(config: &gix_config::File) -> i64 {
            let raw = config.string("core.repositoryformatversion");
            raw.map(|v| v.to_string().parse().unwrap_or(0)).unwrap_or(0)
        }

        fn f(&self) -> Vec<(Option<String>, String)> {
            self.config_values("branch", "merge").unwrap_or_default()
        }
        "#,
    );
    let root = _dir.path();
    let found = scan_for_worktree_config_reads(root);
    assert!(
        found.is_empty(),
        "legitimate, unrelated config reads must not trip the guard: {found:?}"
    );
}

/// `work_dir_is_bounded_by_discovery`'s own real check (docs/issues.md P9),
/// re-run with the fixed two-needle scanner over the actual `src/` tree. If
/// this ever finds something, the `work_dir` ceiling check documented in
/// `repo.rs` is no longer merely defensive and needs a real fixture, exactly
/// as that test's doc says.
#[test]
fn the_real_crate_src_does_not_read_core_worktree() {
    let src = Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
    let found = scan_for_worktree_config_reads(&src);
    assert!(
        found.is_empty(),
        "core.worktree is now read in {found:?} — the work_dir ceiling check \
         is no longer merely defensive, and needs a real fixture proving a \
         repository cannot relocate its working tree outside the mount"
    );
}
