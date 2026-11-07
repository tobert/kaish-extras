//! Repository paths in the write verbs are bytes.
//!
//! Git stores a path as the bytes the filesystem gave it. A name that is not
//! UTF-8 (`caf\xe9.txt`) is ordinary there, and a verb that converted it to a
//! `String` on the way through would write a different name back: a plain
//! commit would rename the file to U+FFFD in the tree, `-a` would drop it, and
//! `reset` would add a second index entry. So every path the write verbs read
//! from the index, a tree or the working tree is a [`RelPath`] from the read to
//! the write, and only the text a person reads is converted, by [`show`].
//! `tests/write_shaped_identifiers.rs` fails on a lossy conversion anywhere in
//! this directory that is not marked as display-only.

use gix_object::bstr::BString;

/// A `/`-separated path relative to the repository root, as git stores it.
pub(crate) type RelPath = BString;

/// The path as text for a message. Display only: the text of a path that is
/// not UTF-8 is git's C-quoted form (`"caf\351.txt"`), which is not the path.
pub(crate) fn show(path: &[u8]) -> String {
    match std::str::from_utf8(path) {
        Ok(text) => text.to_string(),
        Err(_) => crate::render::quote_c_style_bytes("", path).unwrap_or_default(),
    }
}

/// [`show`] for a list, joined with `sep`.
pub(crate) fn show_all<'a>(paths: impl IntoIterator<Item = &'a RelPath>, sep: &str) -> String {
    paths.into_iter().map(|p| show(p)).collect::<Vec<_>>().join(sep)
}

/// The host path for `rel`, under no base: the same bytes on unix.
pub(crate) fn os(rel: &[u8]) -> &std::path::Path {
    crate::worktree::bytes_path(rel)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn text_shows_as_itself_and_other_bytes_as_git_quotes_them() {
        assert_eq!(show(b"d/a.txt"), "d/a.txt");
        assert_eq!(show("café.txt".as_bytes()), "café.txt");
        assert_eq!(show(b"caf\xe9.txt"), "\"caf\\351.txt\"");
    }
}
