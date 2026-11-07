//! Which conversions git would apply to a file on `add`, decided only far
//! enough to know whether storing the raw bytes is what git would store
//! (git-write-profile.md §5.3).
//!
//! The matcher is ours, built on `gix-glob`, and it errs toward "a conversion
//! may apply": a path this reads as converted that git would store raw is
//! refused, and the reverse never happens. Reading attributes to refuse is a
//! different act from reading them to run a driver, and it is the only use.
//!
//! Sources, lowest precedence first: `.gitattributes` at the working tree root
//! down to the file's directory, then `info/attributes`. User-level
//! `core.attributesFile` and `~/.config/git/attributes` are invisible; the
//! first is refused by name in `state::ensure_writable`.

use std::collections::BTreeMap;
use gix_object::bstr::ByteSlice as _;

use crate::error::GitError;
use crate::verbs::write::containment::Leaf;
use crate::verbs::write::refusal::{refuse, Override};
use crate::verbs::write::relpath::{os, show};
use crate::verbs::write::repo::WriteRepo;
use crate::verbs::write::state::{Autocrlf, StagePolicy};

/// An attribute file larger than this is refused, not skipped.
const MAX_ATTR_FILE: u64 = 1024 * 1024;

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum AttrState {
    Unspecified,
    Set,
    Unset,
    Value(String),
}

impl AttrState {
    fn is_on(&self) -> bool {
        matches!(self, AttrState::Set | AttrState::Value(_))
    }
}

/// The attributes that change bytes on `add`.
const RELEVANT: &[&str] = &["filter", "working-tree-encoding", "text", "eol", "crlf", "ident"];

#[derive(Debug)]
enum Matched {
    Yes,
    No,
    /// The pattern could not be read exactly.
    Maybe,
}

struct Line {
    pattern: Option<gix_glob::Pattern>,
    /// `true` when the pattern text could not be parsed exactly.
    unreadable: bool,
    /// The line is not UTF-8, so neither its pattern nor its attributes were
    /// read. It may set any conversion.
    undecodable: bool,
    attrs: Vec<(String, AttrState)>,
}

struct AttrFile {
    /// The directory the file's patterns are relative to, `""` for the root.
    dir: Vec<u8>,
    /// Where it was read, for refusals.
    label: String,
    lines: Vec<Line>,
}

/// The conversion-relevant attributes of one path.
#[derive(Debug, Default)]
pub(crate) struct Conversions {
    values: BTreeMap<&'static str, AttrState>,
    /// A line this reader could not match exactly mentioned a relevant
    /// attribute or a macro; the file holding it.
    pub(crate) unsure: Option<String>,
}

impl Conversions {
    fn get(&self, name: &str) -> AttrState {
        self.values.get(name).cloned().unwrap_or(AttrState::Unspecified)
    }
}

pub(crate) struct AttributeReader<'a> {
    repo: &'a WriteRepo,
    /// `.gitattributes` per directory, `None` when the directory has none.
    cache: BTreeMap<Vec<u8>, Option<AttrFile>>,
    info: Option<Option<AttrFile>>,
    command: String,
}

impl<'a> AttributeReader<'a> {
    pub(crate) fn new(repo: &'a WriteRepo, command: &str) -> Self {
        Self { repo, cache: BTreeMap::new(), info: None, command: command.to_string() }
    }

    fn unreadable<T>(&self, label: &str, why: &str) -> Result<T, GitError> {
        refuse(
            4,
            &self.command,
            format!("The attributes file '{label}' {why}, so this shell cannot tell which conversions git would apply."),
            "Stage the file with real git, outside this shell: git add <path>.",
            Override::Repository,
        )
    }

    fn load(&self, dir: &[u8]) -> Result<Option<AttrFile>, GitError> {
        let mut rel_bytes = dir.to_vec();
        if !dir.is_empty() {
            rel_bytes.push(b'/');
        }
        rel_bytes.extend_from_slice(b".gitattributes");
        let rel = show(&rel_bytes);
        let abs = match self.repo.check_worktree(os(&rel_bytes), Leaf::Entry) {
            Ok(p) => p,
            // Say what the walk found: a link, a directory, or a link above it.
            Err(GitError::WriteThroughLink { found, path, .. }) if path == rel => {
                return self.unreadable(&rel, &format!("cannot be read: the walk found {found} there"))
            }
            Err(GitError::WriteThroughLink { found, path, .. }) => {
                return self.unreadable(&rel, &format!("cannot be read: '{path}' above it is {found}"))
            }
            Err(e) => return Err(e),
        };
        let meta = match std::fs::symlink_metadata(&abs) {
            Ok(m) => m,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(_) => return self.unreadable(&rel, "cannot be read"),
        };
        if meta.file_type().is_symlink() {
            return self.unreadable(&rel, "is a symbolic link, which git does not read either");
        }
        if !meta.is_file() {
            return Ok(None);
        }
        if meta.len() > MAX_ATTR_FILE {
            return self.unreadable(&rel, "is over 1 MiB");
        }
        let Ok(bytes) = std::fs::read(&abs) else {
            return self.unreadable(&rel, "cannot be read");
        };
        Ok(Some(parse_file(dir, rel, &bytes)))
    }

    fn load_info(&self) -> Result<Option<AttrFile>, GitError> {
        let Some(info) = self.repo.read().contained_leaf("info directory", self.repo.common_dir(), "info")? else {
            return Ok(None);
        };
        let Some(path) = self.repo.read().contained_leaf("info/attributes", &info, "attributes")? else {
            return Ok(None);
        };
        match std::fs::metadata(&path) {
            Ok(m) if m.is_file() && m.len() <= MAX_ATTR_FILE => {}
            Ok(m) if m.is_file() => return self.unreadable("info/attributes", "is over 1 MiB"),
            _ => return Ok(None),
        }
        let Ok(bytes) = std::fs::read(&path) else {
            return self.unreadable("info/attributes", "cannot be read");
        };
        Ok(Some(parse_file(b"", "info/attributes".to_string(), &bytes)))
    }

    /// The conversion attributes of the file at repo-relative `rel`.
    pub(crate) fn conversions(&mut self, rel: &[u8]) -> Result<Conversions, GitError> {
        // Root first, then each directory down to the file's own.
        let mut dirs: Vec<Vec<u8>> = vec![Vec::new()];
        let mut acc: Vec<u8> = Vec::new();
        let parts: Vec<&[u8]> = rel.split(|b| *b == b'/').collect();
        for part in &parts[..parts.len().saturating_sub(1)] {
            if !acc.is_empty() {
                acc.push(b'/');
            }
            acc.extend_from_slice(part);
            dirs.push(acc.clone());
        }
        for d in &dirs {
            if !self.cache.contains_key(d) {
                let file = self.load(d)?;
                self.cache.insert(d.clone(), file);
            }
        }
        if self.info.is_none() {
            self.info = Some(self.load_info()?);
        }

        let mut files: Vec<&AttrFile> = dirs.iter().filter_map(|d| self.cache.get(d).and_then(Option::as_ref)).collect();
        if let Some(Some(info)) = &self.info {
            files.push(info);
        }

        // Macros: any file may define them; a later definition wins.
        let mut macros: BTreeMap<&str, &Vec<(String, AttrState)>> = BTreeMap::new();
        for f in &files {
            for l in &f.lines {
                if let Some(name) = macro_name(l) {
                    macros.insert(name, &l.attrs);
                }
            }
        }

        let mut out = Conversions::default();
        let mut resolved: BTreeMap<String, AttrState> = BTreeMap::new();
        for f in &files {
            let local: &[u8] = match rel.strip_prefix(f.dir.as_slice()) {
                Some(r) if f.dir.is_empty() => r,
                Some(r) => r.strip_prefix(b"/").unwrap_or(r),
                None => continue,
            };
            for line in &f.lines {
                if macro_name(line).is_some() {
                    continue;
                }
                match line_matches(line, local) {
                    Matched::No => {}
                    Matched::Maybe => {
                        if line.undecodable || line.attrs.iter().any(|(n, _)| RELEVANT.contains(&n.as_str()) || macros.contains_key(n.as_str()) || n == "binary") {
                            out.unsure.get_or_insert_with(|| f.label.clone());
                        }
                    }
                    Matched::Yes => apply(&mut resolved, &line.attrs, &macros, 0),
                }
            }
        }
        for name in RELEVANT {
            if let Some(v) = resolved.get(*name) {
                out.values.insert(name, v.clone());
            }
        }
        Ok(out)
    }
}

fn macro_name(line: &Line) -> Option<&str> {
    line.attrs.first().and_then(|(n, s)| n.strip_prefix("[attr]").filter(|_| *s == AttrState::Set))
}

fn apply(
    resolved: &mut BTreeMap<String, AttrState>,
    attrs: &[(String, AttrState)],
    macros: &BTreeMap<&str, &Vec<(String, AttrState)>>,
    depth: usize,
) {
    for (name, state) in attrs {
        if name.starts_with("[attr]") {
            continue;
        }
        resolved.insert(name.clone(), state.clone());
        if depth < 5 && *state == AttrState::Set {
            if name == "binary" {
                resolved.insert("text".into(), AttrState::Unset);
                resolved.insert("diff".into(), AttrState::Unset);
                resolved.insert("merge".into(), AttrState::Unset);
            } else if let Some(body) = macros.get(name.as_str()) {
                // Skip the leading `[attr]name` token itself.
                apply(resolved, &body[1..], macros, depth + 1);
            }
        }
    }
}

fn parse_attr_token(tok: &str) -> (String, AttrState) {
    if let Some(n) = tok.strip_prefix('-') {
        (n.to_string(), AttrState::Unset)
    } else if let Some(n) = tok.strip_prefix('!') {
        (n.to_string(), AttrState::Unspecified)
    } else if let Some((n, v)) = tok.split_once('=') {
        (n.to_string(), AttrState::Value(v.to_string()))
    } else {
        (tok.to_string(), AttrState::Set)
    }
}

fn parse_file(dir: &[u8], label: String, bytes: &[u8]) -> AttrFile {
    let mut lines = Vec::new();
    for raw_bytes in bytes.lines() {
        let Ok(raw) = std::str::from_utf8(raw_bytes) else {
            lines.push(Line { pattern: None, unreadable: true, undecodable: true, attrs: Vec::new() });
            continue;
        };
        let l = raw.trim_start_matches([' ', '\t']);
        if l.is_empty() || l.starts_with('#') {
            continue;
        }
        if let Some(rest) = l.strip_prefix("[attr]") {
            let mut toks = rest.split_whitespace();
            let Some(name) = toks.next() else { continue };
            let mut attrs = vec![(format!("[attr]{name}"), AttrState::Set)];
            attrs.extend(toks.map(parse_attr_token));
            lines.push(Line { pattern: None, unreadable: false, undecodable: false, attrs });
            continue;
        }
        let (pat, rest, quoted) = if let Some(after) = l.strip_prefix('"') {
            // A quoted pattern: not decoded here, so it is "maybe".
            match after.find('"') {
                Some(end) => (&l[..end + 2], &l[end + 2..], true),
                None => (l, "", true),
            }
        } else {
            match l.find([' ', '\t']) {
                Some(i) => (&l[..i], &l[i..], false),
                None => (l, "", false),
            }
        };
        let attrs: Vec<_> = rest.split_whitespace().map(parse_attr_token).collect();
        // Git ignores negative patterns in attribute files.
        let parsed = if quoted || pat.contains('\\') {
            None
        } else {
            gix_glob::Pattern::from_bytes_without_negation(pat.as_bytes())
        };
        let negative = !quoted && pat.starts_with('!');
        if negative {
            continue;
        }
        lines.push(Line { unreadable: parsed.is_none(), undecodable: false, pattern: parsed, attrs });
    }
    AttrFile { dir: dir.to_vec(), label, lines }
}

fn line_matches(line: &Line, local: &[u8]) -> Matched {
    if line.unreadable {
        return Matched::Maybe;
    }
    let Some(p) = &line.pattern else { return Matched::Maybe };
    let basename = local.iter().rposition(|b| *b == b'/').map(|i| i + 1);
    if p.matches_repo_relative_path(
        local.into(),
        basename,
        Some(false),
        gix_glob::pattern::Case::Sensitive,
        gix_glob::wildmatch::Mode::NO_MATCH_SLASH_LITERAL,
    ) {
        Matched::Yes
    } else {
        Matched::No
    }
}

/// `Some(reason)` when git would store different bytes from `bytes` for this
/// path, or when this reader cannot tell. `None` means the raw bytes are what
/// git stores.
pub(crate) fn conversion_reason(conv: &Conversions, policy: &StagePolicy, bytes: &[u8]) -> Option<String> {
    if let Some(file) = &conv.unsure {
        return Some(format!("a line in '{file}' that this shell cannot match exactly may set a conversion"));
    }
    let filter = conv.get("filter");
    if filter != AttrState::Unspecified && filter != AttrState::Unset {
        return Some(match filter {
            AttrState::Value(v) => format!("the attribute filter={v} names a clean filter"),
            _ => "the attribute filter names a clean filter".to_string(),
        });
    }
    let enc = conv.get("working-tree-encoding");
    if enc != AttrState::Unspecified && enc != AttrState::Unset {
        return Some("the attribute working-tree-encoding converts the file's encoding".to_string());
    }
    let has_cr = bytes.contains(&b'\r');
    let has_id = bytes.windows(3).any(|w| w == b"$Id");
    if !has_cr && !has_id {
        return None;
    }
    let (text, eol, crlf, ident) = (conv.get("text"), conv.get("eol"), conv.get("crlf"), conv.get("ident"));
    if ident.is_on() {
        return Some("the attribute ident rewrites $Id".to_string());
    }
    if text.is_on() {
        return Some("the attribute text converts line endings".to_string());
    }
    if eol.is_on() {
        return Some("the attribute eol converts line endings".to_string());
    }
    if crlf.is_on() {
        return Some("the attribute crlf converts line endings".to_string());
    }
    let off = text == AttrState::Unset || crlf == AttrState::Unset;
    if !off && matches!(policy.autocrlf, Autocrlf::True | Autocrlf::Input) {
        return Some("core.autocrlf converts line endings".to_string());
    }
    if !off && policy.eol_set {
        return Some("core.eol is set".to_string());
    }
    None
}
