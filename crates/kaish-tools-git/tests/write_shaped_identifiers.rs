//! The grep-able invariant from architecture.md D.1, layer 1.
//!
//! "The code that could write does not exist" is the read profile's central
//! claim, and layer 1's enforcement is deliberately crude: scan the crate's
//! own source and assert that the write-shaped plumbing appears nowhere
//! outside `src/verbs/write/`. Crude, cheap, and it fails the day someone
//! reaches for one of these in the wrong place — which is exactly the day you
//! want to hear about it, long before the fingerprint test does.
//!
//! `src/verbs/write/` does not exist yet. That makes the assertion trivially
//! satisfiable today and non-trivially useful the moment the worktree or
//! commit profile lands, because the test is already here and already failing
//! if the first write-shaped call arrives in `repo.rs` instead.

use std::path::{Path, PathBuf};

/// Spellings of the gitoxide write surface, and what each one would mean.
///
/// Names, not regexes: a `&str` match is what makes this readable as a list of
/// the things a reviewer would grep for by hand.
const WRITE_SHAPED: &[(&str, &str)] = &[
    (
        "gix_object::Write",
        "the object-writing trait — anything implementing or calling it \
         creates objects",
    ),
    (
        ".transaction(",
        "gix-ref's transaction API, the entry point to every ref write",
    ),
    (
        ".prepare(",
        "gix-ref's transaction preparation, which takes the `.lock` files",
    ),
    (
        ".write_to(",
        "gix-index's encoder — writing the index back to disk",
    ),
    (
        "WriteReflog::Always",
        "reflog writing; the read path sets WriteReflog::Disable",
    ),
    (
        "WriteReflog::Normal",
        "reflog writing; the read path sets WriteReflog::Disable",
    ),
];

/// Directories under `src/` where write-shaped plumbing is expected to live.
///
/// Relative to `src/`. Everything else is read-only territory.
const WRITE_ALLOWED: &[&str] = &["verbs/write"];

fn crate_src() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("src")
}

/// Collect every `.rs` file under `dir`.
fn rust_sources(dir: &Path, out: &mut Vec<PathBuf>) {
    let entries = std::fs::read_dir(dir).unwrap_or_else(|e| panic!("read_dir {}: {e}", dir.display()));
    for entry in entries {
        let path = entry.expect("dir entry").path();
        if path.is_dir() {
            rust_sources(&path, out);
        } else if path.extension().is_some_and(|e| e == "rs") {
            out.push(path);
        }
    }
}

/// Strip `//` line comments so a comment *naming* one of these identifiers —
/// this file's own prose does it, and so does `repo.rs` — is not a hit.
/// A comment cannot write.
fn strip_line_comments(source: &str) -> String {
    source
        .lines()
        .map(|line| match line.find("//") {
            Some(idx) => &line[..idx],
            None => line,
        })
        .collect::<Vec<_>>()
        .join("\n")
}

#[test]
fn write_shaped_plumbing_appears_only_under_verbs_write() {
    let src = crate_src();
    let mut files = Vec::new();
    rust_sources(&src, &mut files);
    // The floor tracks the crate as it grows: a scan that silently stopped
    // walking would otherwise pass over the modules it never opened. Raise
    // it when modules are added, the way PR 5 did (worktree.rs, diffcore.rs,
    // verbs/diff.rs) and PR 7 did (reach.rs, verbs/branch.rs, verbs/tag.rs,
    // verbs/worktree.rs).
    assert!(
        files.len() >= 21,
        "found only {} source files under {} — the scan is not seeing the \
         crate",
        files.len(),
        src.display()
    );

    let mut violations = Vec::new();
    for file in &files {
        let rel = file.strip_prefix(&src).expect("under src");
        let rel_str = rel.to_string_lossy().replace('\\', "/");
        if WRITE_ALLOWED.iter().any(|dir| rel_str.starts_with(dir)) {
            continue;
        }
        let source = std::fs::read_to_string(file)
            .unwrap_or_else(|e| panic!("read {}: {e}", file.display()));
        let code = strip_line_comments(&source);
        for (needle, why) in WRITE_SHAPED {
            if code.contains(needle) {
                violations.push(format!("{rel_str}: '{needle}' — {why}"));
            }
        }
    }

    assert!(
        violations.is_empty(),
        "write-shaped gitoxide plumbing appears outside src/verbs/write/ \
         (architecture.md D.1). Either this call does not belong on the read \
         path, or it belongs behind a write feature:\n  {}",
        violations.join("\n  ")
    );
}

/// The scan must be able to fail. If `strip_line_comments` or the file walk
/// silently stopped matching, the test above would pass over a crate that had
/// grown a ref transaction.
#[test]
fn the_scan_detects_a_write_shaped_identifier() {
    let planted = "let tx = store.transaction().prepare(edits)?;";
    let code = strip_line_comments(planted);
    assert!(
        WRITE_SHAPED.iter().any(|(needle, _)| code.contains(needle)),
        "the needle list no longer matches an obvious ref transaction"
    );
}

/// Comments are stripped, but only comments. A line whose code precedes a
/// comment must still be scanned.
#[test]
fn stripping_comments_does_not_hide_code_on_the_same_line() {
    let line = "store.transaction(); // this is a write";
    assert!(
        strip_line_comments(line).contains(".transaction("),
        "code before a comment must survive stripping"
    );
    assert!(
        !strip_line_comments("// store.transaction()").contains(".transaction("),
        "a wholly commented line must not count as a hit"
    );
}

/// `src/verbs/write/` is the only exemption, and it must stay the only one —
/// an allowlist that grew a second entry would be the quiet way to defeat
/// this test.
#[test]
fn the_write_allowlist_is_exactly_verbs_write() {
    assert_eq!(
        WRITE_ALLOWED,
        ["verbs/write"],
        "widening the write allowlist changes what layer 1 claims; that is a \
         design decision, not a test fixup"
    );
}
