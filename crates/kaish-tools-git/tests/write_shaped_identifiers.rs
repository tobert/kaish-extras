//! The grep-able invariant from architecture.md D.1, layer 1.
//!
//! "The code that could write does not exist" is the read profile's central
//! claim, and layer 1's enforcement is deliberately crude: scan the crate's
//! own source and assert that the write-shaped plumbing appears nowhere
//! outside `src/verbs/write/`. Crude, cheap, and it fails the day someone
//! reaches for one of these in the wrong place — which is exactly the day you
//! want to hear about it, long before the fingerprint test does.
//!
//! `src/verbs/write/` exists behind the `commit` cargo feature (phase 1a of the
//! commit profile). Write-shaped plumbing may appear there and nowhere else.
//! The needle list was widened in 1a with the holes feasibility §7 found: a
//! trait imported by brace list, method calls through an imported trait, lock
//! and tempfile crates, the `std::fs` primitives a hand-written reflog append
//! would use, and a second ref store. Every needle has a planted-source
//! control below that proves the scan can fail.

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
    (
        "gix_object::*",
        "a glob import of gix-object, which brings the object-writing trait \
         in without naming it",
    ),
    (
        ".write_buf(",
        "a method call through an already-imported object-writing trait",
    ),
    (
        ".write_stream(",
        "a method call through an already-imported object-writing trait",
    ),
    (
        "gix_lock",
        "gix-lock: taking a lock file is the first step of every write",
    ),
    (
        "gix_tempfile",
        "gix-tempfile: the file a rename-into-place write goes through",
    ),
    (
        "acquire_to_update_resource",
        "gix-lock's entry point for replacing a file",
    ),
    (
        ".persist(",
        "persisting a tempfile or lock moves it into place",
    ),
    (
        "fs::write",
        "std::fs::write: the primitive a hand-written file write would use",
    ),
    (
        "fs::rename",
        "std::fs::rename: moves a file into place",
    ),
    (
        "fs::remove_file",
        "std::fs::remove_file: deletes a file",
    ),
    (
        "fs::remove_dir",
        "std::fs::remove_dir / remove_dir_all: deletes a directory",
    ),
    (
        "fs::copy",
        "std::fs::copy: writes a file",
    ),
    (
        "OpenOptions",
        "OpenOptions opens for write or append, which is how a reflog line \
         would be written by hand",
    ),
    (
        "File::create",
        "File::create truncates and writes",
    ),
    (
        "create_dir",
        "create_dir / create_dir_all: makes a directory",
    ),
    (
        "set_permissions",
        "changes a file's mode",
    ),
    (
        "hard_link(",
        "creates a link",
    ),
    (
        "os::unix::fs::symlink",
        "creates a symlink",
    ),
    (
        "sync_all",
        "fsync: only a writer has anything to sync",
    ),
    (
        "sync_data",
        "fsync: only a writer has anything to sync",
    ),
];

/// Crates whose brace-list `use` can import the object-writing trait without
/// the literal `gix_object::Write` ever appearing
/// (`use gix_object::{FindExt, Write};`).
const TRAIT_SOURCE_CRATES: &[&str] = &["gix_object", "gix_odb"];

/// These build a ref store. The read path builds two in `repo.rs` (one per
/// layout: a plain repository and a linked worktree), both with
/// `WriteReflog::Disable`; any other construction outside `verbs/write/` is a
/// store whose options nothing here has checked.
const REF_STORE_CONSTRUCTORS: &[&str] = &["Store::at(", "Store::for_linked_worktree("];

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

/// Reduce a source file to the code that can run.
///
/// Drops `//` and `/* */` comments, blanks the contents of string and char
/// literals, and removes `#[cfg(test)] mod name { .. }` blocks.
///
/// - Comments cannot write, and this file's own prose names every needle.
/// - A `//` inside a string literal (`"http://..."`) is not a comment, so a
///   line-based `find("//")` truncated the line and hid code after it
///   (feasibility §7). Tracking literals fixes that, and a needle spelled
///   inside a string cannot write either.
/// - Unit tests in `src/` write fixtures with `std::fs`; that is test code,
///   not the shipped read path. Only a brace-balanced `#[cfg(test)] mod` is
///   dropped, so code after it is still scanned.
///
/// A scanner, not a Rust parser: a raw string with an embedded `"` can
/// desynchronize it.
fn code_of(source: &str) -> String {
    let b: Vec<char> = source.chars().collect();
    let mut out = String::with_capacity(source.len());
    let mut i = 0;
    while i < b.len() {
        let c = b[i];
        let next = b.get(i + 1).copied();
        if c == '/' && next == Some('/') {
            while i < b.len() && b[i] != '\n' {
                i += 1;
            }
        } else if c == '/' && next == Some('*') {
            let mut depth = 1;
            i += 2;
            while i < b.len() && depth > 0 {
                if b[i] == '/' && b.get(i + 1) == Some(&'*') {
                    depth += 1;
                    i += 2;
                } else if b[i] == '*' && b.get(i + 1) == Some(&'/') {
                    depth -= 1;
                    i += 2;
                } else {
                    if b[i] == '\n' {
                        out.push('\n');
                    }
                    i += 1;
                }
            }
        } else if c == '"' {
            out.push('"');
            i += 1;
            while i < b.len() && b[i] != '"' {
                if b[i] == '\\' {
                    i += 1;
                }
                if i < b.len() && b[i] == '\n' {
                    out.push('\n');
                }
                i += 1;
            }
            out.push('"');
            i += 1;
        } else if c == '\'' && b.get(i + 2) == Some(&'\'') && next != Some('\\') {
            // A one-char literal such as '"' or '{'.
            out.push_str("''");
            i += 3;
        } else if c == '\'' && next == Some('\\') {
            // An escaped char literal such as '\n' or '\''.
            let mut j = i + 2;
            while j < b.len() && b[j] != '\'' {
                j += 1;
            }
            out.push_str("''");
            i = j + 1;
        } else {
            out.push(c);
            i += 1;
        }
    }
    drop_cfg_test_mods(&out)
}

/// Remove each `#[cfg(test)]` + `mod name { .. }` block, brace-balanced.
/// Runs after literals are blanked, so braces in strings do not count.
fn drop_cfg_test_mods(code: &str) -> String {
    const MARK: &str = "#[cfg(test)]";
    let mut out = String::new();
    let mut rest = code;
    while let Some(at) = rest.find(MARK) {
        let after = &rest[at + MARK.len()..];
        let trimmed = after.trim_start();
        if let Some(body) = trimmed.strip_prefix("mod ") {
            if let Some(open) = body.find('{') {
                let header = &body[..open];
                if header.chars().all(|c| c.is_alphanumeric() || c == '_' || c == ' ') {
                    let mut depth = 0usize;
                    let mut end = None;
                    for (k, ch) in body[open..].char_indices() {
                        match ch {
                            '{' => depth += 1,
                            '}' => {
                                depth -= 1;
                                if depth == 0 {
                                    end = Some(open + k + 1);
                                    break;
                                }
                            }
                            _ => {}
                        }
                    }
                    if let Some(end) = end {
                        out.push_str(&rest[..at]);
                        rest = &body[end..];
                        continue;
                    }
                }
            }
        }
        // `#[cfg(test)]` on something else (a fn, a use): keep it scanned.
        out.push_str(&rest[..at + MARK.len()]);
        rest = after;
    }
    out.push_str(rest);
    out
}

/// Does a brace-list `use` of a trait-source crate import `Write`?
/// `use gix_object::{FindExt, Write};` carries no literal `gix_object::Write`.
fn imports_write_trait(code: &str) -> Option<String> {
    for krate in TRAIT_SOURCE_CRATES {
        let open = format!("{krate}::{{");
        let mut rest = code;
        while let Some(at) = rest.find(&open) {
            let body = &rest[at + open.len()..];
            let mut depth = 1usize;
            let mut end = body.len();
            for (k, ch) in body.char_indices() {
                match ch {
                    '{' => depth += 1,
                    '}' => {
                        depth -= 1;
                        if depth == 0 {
                            end = k;
                            break;
                        }
                    }
                    _ => {}
                }
            }
            let list = &body[..end];
            let has_write = list
                .split(|c: char| !(c.is_alphanumeric() || c == '_'))
                .any(|w| w == "Write");
            if has_write {
                return Some(format!("{krate}::{{.. Write ..}}"));
            }
            rest = &body[end..];
        }
    }
    None
}

/// Every write-shaped hit in one file's source, as `"<needle> — <why>"`.
fn hits_in(source: &str) -> Vec<String> {
    let code = code_of(source);
    let mut hits = Vec::new();
    for (needle, why) in WRITE_SHAPED {
        if code.contains(needle) {
            hits.push(format!("'{needle}' — {why}"));
        }
    }
    if let Some(what) = imports_write_trait(&code) {
        hits.push(format!(
            "'{what}' — a brace-list import of the object-writing trait"
        ));
    }
    hits
}

/// How many times `source` builds a ref store.
fn ref_store_constructions(source: &str) -> usize {
    let code = code_of(source);
    REF_STORE_CONSTRUCTORS
        .iter()
        .map(|c| code.matches(c).count())
        .sum()
}

#[test]
fn write_shaped_plumbing_appears_only_under_verbs_write() {
    let src = crate_src();
    let mut files = Vec::new();
    rust_sources(&src, &mut files);
    // The floor tracks the crate as it grows: a scan that silently stopped
    // walking would otherwise pass over the modules it never opened. Raise
    // it when modules are added.
    assert!(
        files.len() >= 22,
        "found only {} source files under {} — the scan is not seeing the \
         crate",
        files.len(),
        src.display()
    );

    let mut violations = Vec::new();
    let mut ref_stores: Vec<String> = Vec::new();
    for file in &files {
        let rel = file.strip_prefix(&src).expect("under src");
        let rel_str = rel.to_string_lossy().replace('\\', "/");
        if WRITE_ALLOWED.iter().any(|dir| rel_str.starts_with(dir)) {
            continue;
        }
        let source = std::fs::read_to_string(file)
            .unwrap_or_else(|e| panic!("read {}: {e}", file.display()));
        for hit in hits_in(&source) {
            violations.push(format!("{rel_str}: {hit}"));
        }
        for _ in 0..ref_store_constructions(&source) {
            ref_stores.push(rel_str.clone());
        }
    }

    assert!(
        violations.is_empty(),
        "write-shaped gitoxide plumbing appears outside src/verbs/write/ \
         (architecture.md D.1). Either this call does not belong on the read \
         path, or it belongs behind a write feature:\n  {}",
        violations.join("\n  ")
    );
    assert_eq!(
        ref_stores,
        ["repo.rs", "repo.rs"],
        "the read path builds exactly two ref stores (plain and linked \
         worktree), in repo.rs, with WriteReflog::Disable; another \
         construction outside verbs/write/ is a store nothing has checked"
    );
}

/// Each needle, planted in a synthetic source, must be flagged. A needle that
/// stopped matching, or a stripper that swallowed the line, fails here.
#[test]
fn every_needle_is_detected_in_a_planted_source() {
    for (needle, _) in WRITE_SHAPED {
        let planted = format!("fn f() {{ let _ = x{needle}y; }}");
        assert!(
            hits_in(&planted).iter().any(|h| h.contains(&format!("'{needle}'"))),
            "needle {needle:?} is not detected in {planted:?}"
        );
    }
}

/// The spellings feasibility §7 showed the old scan missing, each as real code.
#[test]
fn the_holes_feasibility_found_are_closed() {
    let cases = [
        ("use gix_object::{FindExt, Write};", "brace-list import"),
        ("use gix_odb::{Handle, Write as W};", "brace-list import via gix_odb"),
        ("use gix_object::*;", "glob import"),
        ("handle.write_buf(kind, &data)?;", "method through an imported trait"),
        ("odb.write_stream(kind, n, &mut r)?;", "write_stream"),
        ("let l = gix_lock::File::acquire_to_update_resource(p, f, d)?;", "lock"),
        ("tmp.persist(dest)?;", "persist"),
        ("std::fs::write(p, b)?;", "std::fs::write"),
        ("use std::fs; fs::rename(a, b)?;", "fs::rename"),
        ("fs::remove_file(p)?;", "fs::remove_file"),
        ("std::fs::OpenOptions::new().append(true).open(p)?;", "OpenOptions"),
        ("std::fs::create_dir_all(p)?;", "create_dir"),
        // Code after a string literal that contains `//` was hidden by the
        // old line-based comment stripper.
        (r#"let u = "http://x"; std::fs::write(p, b)?;"#, "code after // in a string"),
        // A cfg(test) mod is dropped, but code after it is still scanned.
        ("#[cfg(test)] mod tests { fn t() {} } fn w() { fs::write(p, b); }", "code after cfg(test) mod"),
        // A cfg(test) on a non-mod item must not hide it.
        ("#[cfg(test)] fn w() { fs::write(p, b); }", "cfg(test) on a fn"),
    ];
    for (src, why) in cases {
        assert!(!hits_in(src).is_empty(), "not flagged ({why}): {src}");
    }
}

/// The negative control for the controls above: comments, string contents and
/// test modules are not hits, and a read-only import list is not a hit.
#[test]
fn comments_strings_and_test_mods_are_not_hits() {
    let clean = [
        "// std::fs::write(p, b)",
        "/* fs::rename(a, b) */ fn f() {}",
        r#"let s = "fs::write is a primitive";"#,
        "use gix_object::{FindExt, Kind};",
        "#[cfg(test)] mod tests { fn t() { std::fs::write(p, b).unwrap(); } }",
        "let c = '\"'; let n = 1; // fs::write(p, b)",
    ];
    for src in clean {
        assert!(hits_in(src).is_empty(), "false positive on: {src}: {:?}", hits_in(src));
    }
}

#[test]
fn a_second_ref_store_is_counted() {
    assert_eq!(ref_store_constructions("let a = file::Store::at(d, o);"), 1);
    assert_eq!(
        ref_store_constructions("let a = file::Store::at(d, o); let b = file::Store::at(e, o);"),
        2
    );
    assert_eq!(ref_store_constructions("// Store::at( in prose"), 0);
    assert_eq!(
        ref_store_constructions("let a = Store::for_linked_worktree(g, c, o);"),
        1
    );
}

/// The old test, kept: a ref transaction is still caught.
#[test]
fn the_scan_detects_a_write_shaped_identifier() {
    let planted = "let tx = store.transaction().prepare(edits)?;";
    assert!(
        !hits_in(planted).is_empty(),
        "the needle list no longer matches an obvious ref transaction"
    );
}

/// Comments are stripped, but only comments. A line whose code precedes a
/// comment must still be scanned.
#[test]
fn stripping_comments_does_not_hide_code_on_the_same_line() {
    let line = "store.transaction(); // this is a write";
    assert!(
        code_of(line).contains(".transaction("),
        "code before a comment must survive stripping"
    );
    assert!(
        !code_of("// store.transaction()").contains(".transaction("),
        "a wholly commented line must not count as a hit"
    );
}

/// With the feature on, `src/verbs/write/` must exist and must itself contain
/// write-shaped plumbing, or the scan has stopped seeing that directory (or
/// the module quietly lost its job). Without the feature the directory is
/// still on disk, because the source scan reads files, not cfgs.
#[test]
fn the_write_directory_is_seen_and_is_where_the_plumbing_lives() {
    let dir = crate_src().join("verbs").join("write");
    assert!(dir.is_dir(), "src/verbs/write/ is missing");
    let mut files = Vec::new();
    rust_sources(&dir, &mut files);
    assert!(!files.is_empty(), "src/verbs/write/ has no .rs files");
    let mut needle_files = 0;
    for f in &files {
        let source = std::fs::read_to_string(f).expect("read");
        if !hits_in(&source).is_empty() {
            needle_files += 1;
        }
    }
    assert!(
        needle_files >= EXPECTED_WRITE_NEEDLE_FILES,
        "src/verbs/write/ contains {needle_files} files with write-shaped \
         plumbing, expected at least {EXPECTED_WRITE_NEEDLE_FILES}"
    );
}

/// `WriteRepo` builds the writing ref store (`WriteReflog::Normal`), so at
/// least `verbs/write/repo.rs` carries a needle from phase 1a on.
const EXPECTED_WRITE_NEEDLE_FILES: usize = 1;

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
