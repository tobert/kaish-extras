//! Unified patch text, assembled from the typed diff model
//! (architecture.md F.1).
//!
//! **The target is `git apply` compatibility, not byte-identity with
//! `git diff`.** The model is primary and this is a rendering of it, so the
//! patch says only what the model already knows. What that costs is written
//! down here rather than discovered later:
//!
//! - **No binary patch encoding.** A binary file renders as
//!   `Binary files a/x and b/x differ`, which is also what `git diff` prints
//!   without `--binary` — and a patch carrying that line is rejected by
//!   `git apply` ("cannot apply binary patch ... without full index line"),
//!   from git's own output as much as from ours.
//! - **No `index` line when a side is the working tree.** Working-tree
//!   content has no oid in the model ([`DiffFile::old_oid`]), so the line
//!   `git apply -3` reads is omitted rather than invented. An ordinary
//!   `git apply` does not need it.
//! - **Abbreviated oids are seven characters**, git's default, and are not
//!   grown to stay unique in a repository where seven is ambiguous.
//! - **Hunk text is UTF-8.** Content that holds no NUL byte but is not valid
//!   UTF-8 is text to git and to this build, and its patch carries U+FFFD
//!   where the invalid bytes were — so that patch will not apply.
//! - **No color, no `--word-diff`, and no whitespace-config rendering**
//!   (`diff.*.whitespace`, `core.autocrlf`): nothing here reads repository
//!   config for presentation (D.3).
//! - **Renames are exact-match only**, so `similarity index` is always
//!   `100%` and a file that was edited *and* moved renders as a delete plus
//!   an add ([`crate::DiffFile::similarity`]).

use crate::model::{DiffFile, DiffHunk, DiffOp, DiffReport, EntryStatus};

/// The whole report as one unified patch.
pub(crate) fn render(report: &DiffReport) -> String {
    let mut out = String::new();
    for file in &report.files {
        render_file(&mut out, file);
    }
    out
}

/// One file's patch fragment.
fn render_file(out: &mut String, file: &DiffFile) {
    let old_path = file.old_path.as_deref().unwrap_or(&file.path);
    let new_path = file.path.as_str();
    let old_label = Label::new("a/", old_path);
    let new_label = Label::new("b/", new_path);

    out.push_str(&format!(
        "diff --git {} {}\n",
        old_label.header, new_label.header
    ));

    match file.status {
        EntryStatus::Added => push_mode(out, "new file mode ", file.new_mode.as_deref()),
        EntryStatus::Deleted => push_mode(out, "deleted file mode ", file.old_mode.as_deref()),
        EntryStatus::Renamed => {
            // Exact match, so the two sides are byte-identical and 100 is the
            // only score this build can report.
            out.push_str("similarity index 100%\n");
            out.push_str(&format!("rename from {}\n", bare(old_path)));
            out.push_str(&format!("rename to {}\n", bare(new_path)));
        }
        _ => {
            if file.old_mode != file.new_mode {
                push_mode(out, "old mode ", file.old_mode.as_deref());
                push_mode(out, "new mode ", file.new_mode.as_deref());
            }
        }
    }

    push_index(out, file);

    if file.binary == Some(true) {
        // git's own wording, and git's own output without `--binary`: an
        // absent side is `/dev/null`, the same as in a `---` line.
        let old_side = match file.status {
            EntryStatus::Added => "/dev/null",
            _ => old_label.header.as_str(),
        };
        let new_side = match file.status {
            EntryStatus::Deleted => "/dev/null",
            _ => new_label.header.as_str(),
        };
        out.push_str(&format!("Binary files {old_side} and {new_side} differ\n"));
        return;
    }

    let Some(hunks) = file.hunks.as_ref().filter(|h| !h.is_empty()) else {
        // A rename or a mode change with no content change has no body, and
        // git prints none either. A file whose hunks a cap declined has none
        // to print: `lines_capped` is where that is stated, and a `---`/`+++`
        // pair with no `@@` under it would be a patch `git apply` refuses.
        return;
    };

    let minus = if file.status == EntryStatus::Added {
        "/dev/null".to_string()
    } else {
        old_label.file_line()
    };
    let plus = if file.status == EntryStatus::Deleted {
        "/dev/null".to_string()
    } else {
        new_label.file_line()
    };
    out.push_str(&format!("--- {minus}\n+++ {plus}\n"));
    for hunk in hunks {
        render_hunk(out, hunk);
    }
}

/// `<label> <mode>`, skipped when the model has no mode to name.
fn push_mode(out: &mut String, label: &str, mode: Option<&str>) {
    if let Some(mode) = mode {
        out.push_str(label);
        out.push_str(mode);
        out.push('\n');
    }
}

/// The `index <old>..<new>[ <mode>]` line `git apply -3` reads.
///
/// Omitted in two cases, both of them git's own:
///
/// - **Either side's oid is unknown.** Working-tree content has no oid in the
///   model, and an invented one would send `git apply -3` after an object
///   that is not in the store.
/// - **The two oids are the same**, which is every mode-only change and every
///   100% rename. There is no content transition to name.
fn push_index(out: &mut String, file: &DiffFile) {
    const ZERO: &str = "0000000";
    if file.old_oid.is_some() && file.old_oid == file.new_oid {
        return;
    }
    let old = match (&file.old_oid, file.status) {
        (Some(oid), _) => short(oid),
        (None, EntryStatus::Added) => ZERO.to_string(),
        (None, _) => return,
    };
    let new = match (&file.new_oid, file.status) {
        (Some(oid), _) => short(oid),
        (None, EntryStatus::Deleted) => ZERO.to_string(),
        (None, _) => return,
    };
    // git names the mode here only when it did not change; a mode change is
    // already stated by the `old mode` / `new mode` pair above.
    match (&file.old_mode, &file.new_mode) {
        (Some(a), Some(b)) if a == b => out.push_str(&format!("index {old}..{new} {a}\n")),
        _ => out.push_str(&format!("index {old}..{new}\n")),
    }
}

/// git's default seven-character abbreviation. Not grown for uniqueness.
fn short(oid: &str) -> String {
    oid.chars().take(7).collect()
}

/// One hunk: the `@@` header and its lines.
fn render_hunk(out: &mut String, hunk: &DiffHunk) {
    out.push_str(&format!(
        "@@ -{} +{} @@",
        range(hunk.old_start, hunk.old_lines),
        range(hunk.new_start, hunk.new_lines)
    ));
    if let Some(section) = &hunk.section {
        out.push(' ');
        out.push_str(section);
    }
    out.push('\n');
    for line in &hunk.lines {
        out.push(match line.op {
            DiffOp::Context => ' ',
            DiffOp::Delete => '-',
            DiffOp::Insert => '+',
        });
        out.push_str(&line.text);
        out.push('\n');
        if line.no_newline {
            out.push_str("\\ No newline at end of file\n");
        }
    }
}

/// `<start>,<count>`, with git's abbreviation of a single-line range.
fn range(start: u32, count: u32) -> String {
    if count == 1 {
        start.to_string()
    } else {
        format!("{start},{count}")
    }
}

/// A path as the two header forms need it.
struct Label {
    /// The form the `diff --git` line takes: `a/x`, or `"a/x\ty"` when the
    /// path needs C-style quoting.
    header: String,
    /// Whether an unquoted path holds a space, which the `---` / `+++` lines
    /// disambiguate with a trailing tab.
    spaced: bool,
}

impl Label {
    fn new(prefix: &str, path: &str) -> Self {
        match quote_c_style(prefix, path) {
            Some(quoted) => Label {
                header: quoted,
                spaced: false,
            },
            None => Label {
                header: format!("{prefix}{path}"),
                spaced: path.contains(' '),
            },
        }
    }

    /// The `---` / `+++` form: the same name, plus the tab git appends to an
    /// unquoted name holding a space so a reader can find where it ends.
    fn file_line(&self) -> String {
        if self.spaced {
            format!("{}\t", self.header)
        } else {
            self.header.clone()
        }
    }
}

/// A path with no `a/`/`b/` prefix, quoted if git would quote it — the form
/// the `rename from` / `rename to` lines take.
fn bare(path: &str) -> String {
    quote_c_style("", path).unwrap_or_else(|| path.to_string())
}

/// git's `quote_c_style`: `Some(quoted)` when the path holds a byte git
/// refuses to print raw — a quote, a backslash, a control character, or
/// anything above ASCII (`core.quotePath`, on by default) — and `None` when
/// the path prints as itself.
fn quote_c_style(prefix: &str, path: &str) -> Option<String> {
    let needs = path
        .bytes()
        .any(|b| b < 0x20 || b == 0x7f || b == b'"' || b == b'\\' || b >= 0x80);
    if !needs {
        return None;
    }
    let mut out = String::with_capacity(path.len() + prefix.len() + 4);
    out.push('"');
    out.push_str(prefix);
    for b in path.bytes() {
        match b {
            b'"' => out.push_str("\\\""),
            b'\\' => out.push_str("\\\\"),
            0x07 => out.push_str("\\a"),
            0x08 => out.push_str("\\b"),
            0x0c => out.push_str("\\f"),
            b'\n' => out.push_str("\\n"),
            b'\r' => out.push_str("\\r"),
            b'\t' => out.push_str("\\t"),
            0x0b => out.push_str("\\v"),
            b if !(0x20..0x7f).contains(&b) => out.push_str(&format!("\\{b:03o}")),
            b => out.push(b as char),
        }
    }
    out.push('"');
    Some(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// git prints a path as itself until a byte forces its hand, and then it
    /// prints the whole thing C-quoted with octal escapes for everything
    /// above ASCII. Both halves are load-bearing: quoting a plain path would
    /// make every ordinary patch differ from git's.
    #[test]
    fn paths_are_quoted_exactly_where_git_quotes_them() {
        assert_eq!(quote_c_style("a/", "plain.txt"), None);
        assert_eq!(quote_c_style("a/", "with space.txt"), None);
        assert_eq!(
            quote_c_style("a/", "quo\"te.txt").as_deref(),
            Some("\"a/quo\\\"te.txt\"")
        );
        assert_eq!(
            quote_c_style("a/", "tab\tinside.txt").as_deref(),
            Some("\"a/tab\\tinside.txt\"")
        );
        assert_eq!(
            quote_c_style("a/", "ünïcode.txt").as_deref(),
            Some("\"a/\\303\\274n\\303\\257code.txt\"")
        );
    }

    /// A space in an unquoted path is ambiguous in a `---` line, and git
    /// closes it with a trailing tab rather than by quoting.
    #[test]
    fn an_unquoted_space_gets_the_trailing_tab() {
        assert_eq!(Label::new("a/", "with space.txt").file_line(), "a/with space.txt\t");
        assert_eq!(Label::new("a/", "plain.txt").file_line(), "a/plain.txt");
        // Quoted already says where the name ends; no tab.
        assert_eq!(
            Label::new("a/", "sp ace\"q.txt").file_line(),
            "\"a/sp ace\\\"q.txt\""
        );
    }

    /// git writes `@@ -1 +1 @@` for a one-line range and `@@ -1,3 +1,4 @@`
    /// otherwise, and a zero-length side names the line it follows.
    #[test]
    fn hunk_ranges_use_gits_abbreviation() {
        assert_eq!(range(1, 1), "1");
        assert_eq!(range(1, 3), "1,3");
        assert_eq!(range(0, 0), "0,0");
    }
}
