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
    (
        "File::options",
        "File::options() is OpenOptions by another name",
    ),
    (
        "std::process",
        "std::process spawns a program; layer 3 says no build links spawn \
         machinery, and this is the source-level half of that claim",
    ),
    (
        "process::Command",
        "process::Command, however it was imported, spawns a program",
    ),
    (
        "tokio::process",
        "tokio::process spawns a program",
    ),
    (
        "use gix_object as ",
        "an alias for gix-object, under which `Write` carries a name the scan \
         does not look for",
    ),
    (
        "use gix_odb as ",
        "an alias for gix-odb, under which the object-writing trait carries a \
         name the scan does not look for",
    ),
    (
        "use gix_ref as ",
        "an alias for gix-ref, under which the transaction API carries a \
         name the scan does not look for",
    ),
    (
        "use gix_index as ",
        "an alias for gix-index, under which its writer carries a name the \
         scan does not look for",
    ),
    (
        "use gix_lock as ",
        "an alias for gix-lock, under which a lock file carries a name the \
         scan does not look for",
    ),
    (
        "extern crate gix_",
        "an extern crate alias for a gix crate, which can rename it past the scan",
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

/// Where write-shaped plumbing is expected to live, relative to `src/`, as
/// path components: the `verbs/write/` directory and the `verbs/write.rs`
/// module file beside it. Matched by component, not by string prefix, so
/// `verbs/write_extra.rs` is scanned like any other file (F3).
const WRITE_DIR: [&str; 2] = ["verbs", "write"];
const WRITE_MODULE_FILE: [&str; 2] = ["verbs", "write.rs"];

/// Is `rel` (relative to `src/`) in the write territory?
fn is_write_territory(rel: &Path) -> bool {
    let parts: Vec<&str> = rel.components().filter_map(|c| c.as_os_str().to_str()).collect();
    parts == WRITE_MODULE_FILE || (parts.len() > WRITE_DIR.len() && parts[..2] == WRITE_DIR)
}

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
        } else if let Some((body, hashes)) = raw_string_start(&b, i) {
            // A raw string: no escapes, ends at `"` plus the same number of
            // `#`. Its contents are not code, and an embedded `"` must not
            // end it early.
            out.push_str("\"\"");
            let mut j = body;
            while j < b.len() {
                if b[j] == '"' && b[j + 1..].iter().take(hashes).filter(|&&h| h == '#').count() == hashes {
                    j += 1 + hashes;
                    break;
                }
                if b[j] == '\n' {
                    out.push('\n');
                }
                j += 1;
            }
            i = j;
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

/// If a raw string literal (`r"`, `r#"`, `br##"`, `cr"`) starts at `b[i]`,
/// return the index of its first content character and its `#` count. The
/// previous character must not be part of an identifier, or `bar"x"` would
/// read as a raw string.
fn raw_string_start(b: &[char], i: usize) -> Option<(usize, usize)> {
    if i > 0 && (b[i - 1].is_alphanumeric() || b[i - 1] == '_') {
        return None;
    }
    let mut j = i;
    if matches!(b.get(j), Some('b' | 'c')) {
        j += 1;
    }
    if b.get(j) != Some(&'r') {
        return None;
    }
    j += 1;
    let mut hashes = 0;
    while b.get(j) == Some(&'#') {
        hashes += 1;
        j += 1;
    }
    (b.get(j) == Some(&'"')).then_some((j + 1, hashes))
}

/// Collapse whitespace so a needle split across lines or spaced out matches:
/// runs become one space, and the space is dropped next to `.`, `:` and a
/// following `(`. `fs :: write (` and `store\n .transaction ()` both match.
fn normalize(code: &str) -> String {
    let chars: Vec<char> = code.chars().collect();
    let mut out = String::with_capacity(code.len());
    let mut i = 0;
    while i < chars.len() {
        if chars[i].is_whitespace() {
            let mut j = i;
            while j < chars.len() && chars[j].is_whitespace() {
                j += 1;
            }
            let prev = out.chars().last();
            let next = chars.get(j).copied();
            let drop = prev.is_none_or(|p| matches!(p, '.' | ':'))
                || next.is_none_or(|n| matches!(n, '.' | ':' | '('));
            if !drop {
                out.push(' ');
            }
            i = j;
        } else {
            out.push(chars[i]);
            i += 1;
        }
    }
    out
}

/// Every `#[path = ..]` attribute and `include!` in one file, as a reason.
///
/// Either can pull code from outside the scanned tree, so neither may appear
/// anywhere in `src/` (the write directory included), and there is no
/// allowlist. `include_str!` and `include_bytes!` embed text and are fine.
fn redirects_in(source: &str) -> Vec<String> {
    let code = code_of(source);
    let b: Vec<char> = code.chars().collect();
    let is_ident = |c: char| c.is_alphanumeric() || c == '_';
    let skip_ws = |mut k: usize| {
        while b.get(k).is_some_and(|c| c.is_whitespace()) {
            k += 1;
        }
        k
    };
    let mut hits = Vec::new();

    // Attributes: `#[..]` and `#![..]`, bracket-balanced; a `path` word
    // followed by `=` anywhere inside (so `cfg_attr(unix, path = ..)` counts).
    for (at, &c) in b.iter().enumerate() {
        if c != '#' {
            continue;
        }
        let mut k = skip_ws(at + 1);
        if b.get(k) == Some(&'!') {
            k = skip_ws(k + 1);
        }
        if b.get(k) != Some(&'[') {
            continue;
        }
        let mut depth = 0usize;
        let mut end = b.len();
        for (m, &ch) in b.iter().enumerate().skip(k) {
            match ch {
                '[' => depth += 1,
                ']' => {
                    depth -= 1;
                    if depth == 0 {
                        end = m;
                        break;
                    }
                }
                _ => {}
            }
        }
        let body = &b[k + 1..end];
        for (m, w) in body.windows(4).enumerate() {
            let word_start = m == 0 || !is_ident(body[m - 1]);
            if w.iter().collect::<String>() == "path" && word_start {
                let mut n = m + 4;
                while body.get(n).is_some_and(|c| c.is_whitespace()) {
                    n += 1;
                }
                if body.get(n) == Some(&'=') {
                    hits.push("'#[path = ..]' — a module file chosen by path, possibly outside src/".to_string());
                }
            }
        }
    }

    // `include!`, however spaced or qualified.
    let name: Vec<char> = "include".chars().collect();
    for m in 0..b.len().saturating_sub(name.len()) {
        if b[m..m + name.len()] != name[..] || (m > 0 && is_ident(b[m - 1])) {
            continue;
        }
        let k = skip_ws(m + name.len());
        if b.get(k) == Some(&'!') && matches!(b.get(skip_ws(k + 1)), Some('(' | '[' | '{')) {
            hits.push("'include!' — source text included from a path the scan does not read".to_string());
        }
    }
    hits
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
    let code = normalize(&code_of(source));
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
    let code = normalize(&code_of(source));
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
        let source = std::fs::read_to_string(file)
            .unwrap_or_else(|e| panic!("read {}: {e}", file.display()));
        // No allowlist: `#[path]` and `include!` are refused in the write
        // territory too, since either can pull in code the scan never reads.
        for hit in redirects_in(&source) {
            violations.push(format!("{rel_str}: {hit}"));
        }
        if is_write_territory(rel) {
            continue;
        }
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


// ---------------------------------------------------------------------------
// F2 / F3 (PR 39 review): controls for the scan's second round. Each one
// fails against the scan as it stood before the fix.
// ---------------------------------------------------------------------------

/// Layer 3's "no spawn" claim rested on `cargo tree` alone. The source scan
/// now says it too.
#[test]
fn process_spawning_spellings_are_detected() {
    for src in [
        "std::process::Command::new(\"git\").spawn()?;",
        "use std::process::Command;",
        "use std::process::{Command, Stdio};",
        "tokio::process::Command::new(\"git\");",
        "let f = std::fs::File::options().append(true).open(p)?;",
        "let f = File::options();",
    ] {
        assert!(!hits_in(src).is_empty(), "not flagged: {src}");
    }
    // Negative control: clap's `Command::new` builds a CLI, it does not spawn.
    assert!(hits_in("let c = clap::Command::new(\"git\");").is_empty());
}

/// A needle split by whitespace or a line break is the same code.
#[test]
fn split_needles_are_detected() {
    for src in [
        "std::fs :: write(p, b)?;",
        "std::fs::\n    write(p, b)?;",
        "store\n    .transaction()\n    .prepare(e, a, b)?;",
        "store . transaction ( )",
        "let t = store.transaction  ();",
        "fs  ::  rename(a, b)?;",
        "use   gix_object   as   o;",
    ] {
        assert!(!hits_in(src).is_empty(), "not flagged: {src:?}");
    }
    assert!(hits_in("let n = a\n    .len();").is_empty());
}

/// `use gix_object as g;` renames the crate, so `g::Write` carries no
/// `gix_object::Write`.
#[test]
fn crate_aliases_are_detected() {
    for krate in ["gix_object", "gix_odb", "gix_ref", "gix_index", "gix_lock"] {
        let src = format!("use {krate} as g;");
        assert!(!hits_in(&src).is_empty(), "alias not flagged: {src}");
    }
    assert!(!hits_in("extern crate gix_object as g;").is_empty());
    // Negative control: a path import with a rename inside it is not a crate
    // alias; `repo.rs` does `use gix_index::hash as gix_hash;`.
    assert!(hits_in("use gix_index::hash as gix_hash;").is_empty());
}

/// A raw string with an embedded `"` desynchronized the stripper, hiding code
/// after it; and its contents are not code.
#[test]
fn raw_strings_are_stripped_not_scanned_and_do_not_hide_code() {
    // Contents are not code.
    assert!(hits_in("let s = r#\"std::fs::write(p, b)\"#;").is_empty());
    assert!(hits_in("let s = r\"fs::write\";").is_empty());
    assert!(hits_in("let s = br##\"fs::write \"# still inside\"##;").is_empty());
    // An embedded quote does not end the literal early or late.
    assert!(hits_in("let s = r#\"say \"fs::write\" aloud\"#;").is_empty());
    // Code after the literal is still scanned.
    assert!(!hits_in("let s = r#\"say \"hi\" \"#; std::fs::write(p, b)?;").is_empty());
    assert!(!hits_in("let s = r#\"a\"#; let t = r#\"b\"#; fs::rename(a, b)?;").is_empty());
    // An identifier ending in `r` before a quote is not a raw string.
    assert!(!hits_in("let bar = foo(\"x\"); fs::write(p, b)?;").is_empty());
}

/// `#[path = ".."]` and `include!` can pull a file in from outside the scanned
/// tree. Neither may appear anywhere in `src/`, `verbs/write/` included.
#[test]
fn module_redirection_is_detected() {
    for src in [
        "#[path = \"../elsewhere.rs\"] mod m;",
        "#[ path=\"x.rs\" ]\nmod m;",
        "#[cfg_attr(unix, path = \"x.rs\")] mod m;",
        "#![path = \"x.rs\"]",
        "include!(\"../elsewhere.rs\");",
        "include ! (\"x.rs\");",
        "std::include!(\"x.rs\");",
    ] {
        assert!(!redirects_in(src).is_empty(), "not flagged: {src:?}");
    }
    // Negative controls: `include_str!` embeds text, not code; a field or
    // fn named `path` is not an attribute.
    for src in [
        "let s = include_str!(\"x.md\");",
        "let b = include_bytes!(\"x.bin\");",
        "#[derive(Debug)] struct S { path: PathBuf }",
        "#[cfg(unix)] fn f(path: &Path) {}",
        "// #[path = \"x\"]",
    ] {
        assert!(redirects_in(src).is_empty(), "false positive: {src:?}");
    }
}

/// F3: the allowlist is a path component match, not a string prefix.
#[test]
fn the_write_allowlist_matches_path_components_not_prefixes() {
    for exempt in ["verbs/write.rs", "verbs/write/repo.rs", "verbs/write/deeper/x.rs"] {
        assert!(is_write_territory(Path::new(exempt)), "{exempt} should be exempt");
    }
    for scanned in [
        "verbs/write_extra.rs",
        "verbs/writer/x.rs",
        "verbs/write_extra/x.rs",
        "verbs/writes.rs",
        "write.rs",
        "repo.rs",
        "verbs/branch.rs",
        "other/verbs/write/x.rs",
    ] {
        assert!(!is_write_territory(Path::new(scanned)), "{scanned} must be scanned");
    }
}

/// `src/verbs/write/` and `src/verbs/write.rs` are the only exemptions, and
/// they must stay the only ones: an allowlist that grew a third entry would be
/// the quiet way to defeat this test.
#[test]
fn the_write_allowlist_is_exactly_verbs_write() {
    assert_eq!(
        (WRITE_DIR, WRITE_MODULE_FILE),
        (["verbs", "write"], ["verbs", "write.rs"]),
        "widening the write allowlist changes what layer 1 claims; that is a \
         design decision, not a test fixup"
    );
}
