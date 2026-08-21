//! A from-scratch, non-recursive read of just enough of the on-disk
//! `.git/index` format to bound the nesting depth of its `TREE` extension
//! (git's "cache-tree") before [`gix_index::State::from_bytes`] ever sees the
//! bytes — see docs/issues.md, **R4**.
//!
//! `gix_index::extension::tree::decode::one_recursive` (gix-index 0.54.0,
//! `src/extension/tree/decode.rs:36`) recurses once per cache-tree subtree
//! with no depth bound. A `.git/index` whose cache-tree nests deep enough
//! overflows the stack *inside that call*, before any code in this crate runs
//! — which is exactly the shape [`crate::repo::ReadRepo::open_index`] cannot
//! guard against by checking `from_bytes`'s return value: by the time it
//! returns (or doesn't), the stack is already blown. The only place left to
//! stand is in front of the call, reading the bytes ourselves.
//!
//! This module re-derives a *minimal* slice of the git index binary format
//! (`Documentation/technical/index-format.txt`) — the fixed header, the
//! variable-length entry records, and the `TREE` extension's own node
//! encoding — just precisely enough to walk past all of it once with an
//! explicit heap stack (a `Vec`) standing in for the call stack, so no input
//! can make *this* walk recurse.
//!
//! **Fail closed, not open.** This module and `gix_index::State::from_bytes`
//! are two independently written readings of the same bytes, and where they
//! disagree is exactly the case that matters: an input this module cannot
//! walk to completion is refused, not waved through on the theory that gix
//! would "probably" reject it too. "gix will reject a malformed index" is not
//! the same claim as "gix will refuse to recurse" — an attacker gets to hunt
//! for the one input where this module's parser gives up early while gix's
//! real decoder reads on and recurses arbitrarily deep on the very same
//! bytes. A check that cannot tell "found nothing dangerous" apart from "the
//! check itself broke" is a check that fails open, so every parse failure
//! below that could plausibly hide a deeper structure is a refusal, not a
//! pass-through. The three exceptions, and why they really are exceptions,
//! are called out at their own definitions below (`extensions_region`'s
//! `NothingToCheck`).
//!
//! Only versions 2, 3 and 4 are understood (the only ones `gix_index` accepts
//! at all).

use crate::error::GitError;
use crate::verbs::status::MAX_STATUS_TREE_DEPTH;

/// This crate reads SHA-1 indexes only — [`crate::repo::ReadRepo::open_index`]
/// hardcodes `gix_index::hash::Kind::Sha1` at its one `from_bytes` call site,
/// so a cache-tree object id is always 20 raw bytes here.
const HASH_LEN: usize = 20;

/// Refuse before `gix_index::State::from_bytes` would recurse too deep — or
/// read something this module could not itself account for — decoding
/// `index_bytes`'s `TREE` extension.
///
/// `index_bytes` must be exactly the bytes about to be handed to
/// `from_bytes`. Returns `Ok(())` only when this module positively confirms
/// there is nothing to refuse: no `TREE` extension is present, or the `TREE`
/// extension it found nests no deeper than [`MAX_STATUS_TREE_DEPTH`] levels —
/// the same bound `verbs::status` uses for its own tree walk, reused rather
/// than duplicated. Everything this module cannot positively confirm is
/// refused with [`GitError::IndexTreeUnreadable`], not passed through.
pub(crate) fn refuse_if_cache_tree_too_deep(operation: &'static str, index_bytes: &[u8]) -> Result<(), GitError> {
    let extensions = match extensions_region(index_bytes) {
        ExtensionsRegion::NothingToCheck => return Ok(()),
        ExtensionsRegion::Unreadable => return Err(unreadable(operation)),
        ExtensionsRegion::Extensions(bytes) => bytes,
    };
    let mut rest = extensions;
    while !rest.is_empty() {
        let Some((signature, payload, after)) = next_extension(rest) else {
            // Not the clean "no more extensions" case (that is `rest.is_empty()`,
            // handled by the loop condition) — fewer than 8 bytes remain, or a
            // declared size overruns what is left. A dangling extension header
            // is not proof there is nothing past it; refuse rather than assume so.
            return Err(unreadable(operation));
        };
        if &signature == b"TREE" {
            match cache_tree_depth(payload, MAX_STATUS_TREE_DEPTH) {
                CacheTreeDepth::Fine => {}
                CacheTreeDepth::TooDeep => {
                    return Err(GitError::IndexTreeTooDeep {
                        operation,
                        limit: MAX_STATUS_TREE_DEPTH,
                    });
                }
                CacheTreeDepth::Unreadable => return Err(unreadable(operation)),
            }
        }
        rest = after;
    }
    Ok(())
}

fn unreadable(operation: &'static str) -> GitError {
    GitError::IndexTreeUnreadable { operation }
}

/// Where the header + entries walk landed, between the last entry and the
/// trailing checksum.
enum ExtensionsRegion<'d> {
    /// The header alone already rules out any recursion risk: a bad
    /// signature, an unrecognized version, or too few bytes even for the
    /// header. Safe to wave through — and not merely "likely" safe, unlike
    /// everything else in this module: `gix_index`'s own header check
    /// (`decode::header::decode`) makes the identical, trivial comparisons
    /// (`data.len() < 3*4 + hash_len`, `signature != b"DIRC"`, `version not in
    /// {2,3,4}`) as its very first step, before entries or extensions are
    /// touched at all, so there is no room for these two readings to
    /// diverge — a plain equality/range check on fixed bytes has no
    /// interesting edge cases the way variable-length entry parsing does.
    NothingToCheck,
    /// This module's own entries walk could not account for all `num_entries`
    /// records. Unlike the header gate above, this *is* real parsing logic —
    /// flags, extended flags, per-version path encoding, padding — with room
    /// to diverge from gix's own reading of the same bytes. Refused, not
    /// waved through; see the module doc.
    Unreadable,
    /// Landed cleanly on the bytes between the last entry and the trailing
    /// checksum — zero or more extensions, or none at all.
    Extensions(&'d [u8]),
}

/// The header + entries walk. See [`ExtensionsRegion`] for what each outcome
/// means and why `NothingToCheck` is the one case here that is safe to leave
/// to `gix_index::State::from_bytes` rather than refuse.
fn extensions_region(bytes: &[u8]) -> ExtensionsRegion<'_> {
    if bytes.len() < 12 + HASH_LEN || &bytes[0..4] != b"DIRC" {
        return ExtensionsRegion::NothingToCheck;
    }
    let version = u32::from_be_bytes(bytes[4..8].try_into().expect("checked above"));
    if !(2..=4).contains(&version) {
        return ExtensionsRegion::NothingToCheck;
    }
    let num_entries = u32::from_be_bytes(bytes[8..12].try_into().expect("checked above"));
    let mut data = &bytes[12..];
    for _ in 0..num_entries {
        data = match skip_one_entry(data, version) {
            Some(rest) => rest,
            None => return ExtensionsRegion::Unreadable,
        };
    }
    // The last HASH_LEN bytes are always the trailing index checksum, never
    // part of an extension. Too little left even for that is vanishingly
    // unlikely for a real index (it would mean `num_entries` consumed nearly
    // the whole file) and is not a shape worth reasoning about further:
    // refuse rather than guess.
    let Some(split) = data.len().checked_sub(HASH_LEN) else {
        return ExtensionsRegion::Unreadable;
    };
    ExtensionsRegion::Extensions(&data[..split])
}

/// Skip exactly one on-disk index entry, returning what follows it.
///
/// Mirrors the entry layout `gix_index` 0.54.0 decodes in
/// `src/decode/entries.rs`'s `load_one` closely enough to land on the same
/// byte offset it would — but only to *skip* the bytes, never to trust or
/// reconstruct their content (the path is never read as text).
fn skip_one_entry(data: &[u8], version: u32) -> Option<&[u8]> {
    let start_len = data.len();
    // ctime(8) + mtime(8) + dev/ino/mode/uid/gid/size (6*4=24) + hash + flags(2).
    let fixed = 8 + 8 + 24 + HASH_LEN + 2;
    if data.len() < fixed {
        return None;
    }
    let flags = u16::from_be_bytes(data[fixed - 2..fixed].try_into().ok()?);
    let mut data = &data[fixed..];

    const EXTENDED: u16 = 1 << 14;
    if flags & EXTENDED != 0 {
        data = data.get(2..)?;
    }

    let data = if version == 4 {
        // Delta-compressed path: a varint strip-length (whose value we don't
        // need — only its on-disk byte length, to skip past it), then a
        // NUL-terminated suffix. No padding in this version.
        let data = skip_varint(data)?;
        let nul_at = data.iter().position(|&b| b == 0)?;
        &data[nul_at + 1..]
    } else {
        const PATH_LEN_MASK: u16 = 0x0fff;
        let path_len = (flags & PATH_LEN_MASK) as usize;
        let data = if path_len == PATH_LEN_MASK as usize {
            // Name is NUL-terminated instead of length-prefixed (>= 4095 bytes).
            let nul_at = data.iter().position(|&b| b == 0)?;
            &data[nul_at + 1..]
        } else {
            data.get(path_len..)?
        };
        // V2/V3 entries are padded, from the entry's own start, to a multiple
        // of 8 bytes — and always by *at least* one byte, even when the
        // unpadded length already lands on a boundary, because git always
        // writes a terminating NUL after the name (gix's `skip_padding`).
        let consumed = start_len - data.len();
        let padded = (consumed + 8) & !7;
        data.get(padded - consumed..)?
    };
    Some(data)
}

/// Skip one variable-length (LEB-ish) integer: a git "offset" varint, one
/// byte per 7 bits, continuation-bit-in-the-high-bit, matching
/// `gix_features::decode::leb64_from_read`. Only the byte length is needed
/// here, never the value.
fn skip_varint(data: &[u8]) -> Option<&[u8]> {
    let mut i = 0;
    loop {
        let byte = *data.get(i)?;
        i += 1;
        if byte & 0x80 == 0 {
            return Some(&data[i..]);
        }
    }
}

/// One `(signature, payload, rest)` step over the extensions block, or `None`
/// when fewer than 8 bytes remain (no room for another signature + size) or a
/// declared size does not fit in what is left. Both are treated by the caller
/// as "cannot confirm there is nothing dangerous past this point" — see the
/// module doc — never as "no more extensions" (that is the caller's own
/// `rest.is_empty()` check, made before ever calling this function).
fn next_extension(data: &[u8]) -> Option<([u8; 4], &[u8], &[u8])> {
    if data.len() < 8 {
        return None;
    }
    let signature: [u8; 4] = data[0..4].try_into().ok()?;
    let size = u32::from_be_bytes(data[4..8].try_into().ok()?) as usize;
    let rest = &data[8..];
    let (payload, after) = rest.split_at_checked(size)?;
    Some((signature, payload, after))
}

/// The outcome of walking a `TREE` extension's payload.
enum CacheTreeDepth {
    /// Walked to completion; nesting never exceeded the limit.
    Fine,
    /// Nesting exceeded the limit before the walk needed to look any
    /// further — the walk stops the instant this is known, so a
    /// well-formed-until-there prefix is all it ever reads.
    TooDeep,
    /// The walk could not reach a confirmed end of the payload. This is
    /// *not* "fine" — see the module doc: a parse failure here does not mean
    /// `gix_index`'s own decode would also stop, so it cannot be treated as
    /// evidence of safety.
    Unreadable,
}

/// Walk a `TREE` extension's payload with an explicit heap stack instead of
/// recursion (gix-index 0.54.0's own decode does this with the call stack —
/// `extension::tree::decode::one_recursive`, one frame per subtree, with no
/// depth bound; see docs/issues.md R4).
///
/// `pending[i]` is how many more direct children the node open at depth `i`
/// still owes; the stack's length *is* the current nesting depth once the
/// root is pushed, mirroring `one_recursive`'s call depth frame for frame
/// (each node it visits is one more open ancestor, exactly one more `Vec`
/// entry here). Returns [`CacheTreeDepth::TooDeep`] the instant depth would
/// exceed `limit` — before parsing anything past that point, so a
/// well-formed-until-there prefix is all this function ever needs to see.
fn cache_tree_depth(payload: &[u8], limit: usize) -> CacheTreeDepth {
    let mut pending: Vec<usize> = Vec::new();
    let mut data = payload;
    loop {
        while pending.last() == Some(&0) {
            pending.pop();
        }
        if pending.is_empty() && data.is_empty() {
            return CacheTreeDepth::Fine;
        }
        let Some((subtree_count, rest)) = parse_cache_tree_node(data) else {
            return CacheTreeDepth::Unreadable;
        };
        data = rest;
        if let Some(top) = pending.last_mut() {
            *top -= 1;
        }
        pending.push(subtree_count);
        if pending.len() > limit {
            return CacheTreeDepth::TooDeep;
        }
    }
}

/// One cache-tree node: `<path> NUL <entry-count> SP <subtree-count> NL
/// [<hash>]`, the hash present only when `entry-count` is not negative
/// (`index-format.txt`, "Cache tree"). Returns the subtree count and what
/// follows; the path and entry count are skipped, never read as trusted
/// content.
fn parse_cache_tree_node(data: &[u8]) -> Option<(usize, &[u8])> {
    let nul_at = data.iter().position(|&b| b == 0)?;
    let data = &data[nul_at + 1..];
    let sp_at = data.iter().position(|&b| b == b' ')?;
    let entry_count_is_negative = data.first() == Some(&b'-');
    let data = &data[sp_at + 1..];
    let nl_at = data.iter().position(|&b| b == b'\n')?;
    let subtree_count: usize = std::str::from_utf8(&data[..nl_at]).ok()?.parse().ok()?;
    let data = &data[nl_at + 1..];
    let data = if entry_count_is_negative { data } else { data.get(HASH_LEN..)? };
    Some((subtree_count, data))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Build one cache-tree node's on-disk bytes: name, entry count, subtree
    /// count, and (for a non-negative entry count) a hash. A helper for
    /// composing whole `TREE` extension payloads by hand.
    fn node(name: &str, entry_count: i64, subtree_count: usize) -> Vec<u8> {
        let mut out = Vec::new();
        out.extend_from_slice(name.as_bytes());
        out.push(0);
        out.extend_from_slice(entry_count.to_string().as_bytes());
        out.push(b' ');
        out.extend_from_slice(subtree_count.to_string().as_bytes());
        out.push(b'\n');
        if entry_count >= 0 {
            out.extend_from_slice(&[0u8; HASH_LEN]);
        }
        out
    }

    fn is_fine(outcome: CacheTreeDepth) -> bool {
        matches!(outcome, CacheTreeDepth::Fine)
    }

    fn is_too_deep(outcome: CacheTreeDepth) -> bool {
        matches!(outcome, CacheTreeDepth::TooDeep)
    }

    fn is_unreadable(outcome: CacheTreeDepth) -> bool {
        matches!(outcome, CacheTreeDepth::Unreadable)
    }

    #[test]
    fn a_single_root_node_is_not_too_deep() {
        let payload = node("", 0, 0);
        assert!(is_fine(cache_tree_depth(&payload, 2)));
    }

    #[test]
    fn a_root_with_two_leaf_children_is_depth_two() {
        let mut payload = node("", 3, 2);
        payload.extend(node("a", 1, 0));
        payload.extend(node("b", 1, 0));
        assert!(is_fine(cache_tree_depth(&payload, 2)), "depth 2 must fit a limit of 2");
        assert!(
            is_too_deep(cache_tree_depth(&payload, 1)),
            "depth 2 must exceed a limit of 1"
        );
    }

    #[test]
    fn a_chain_of_n_single_child_nodes_is_depth_n() {
        let depth = 10;
        let mut payload = Vec::new();
        for i in 0..depth {
            let subtree_count = usize::from(i + 1 < depth);
            payload.extend(node(&format!("d{i}"), 1, subtree_count));
        }
        assert!(
            is_fine(cache_tree_depth(&payload, depth)),
            "exactly at the limit must not refuse"
        );
        assert!(
            is_too_deep(cache_tree_depth(&payload, depth - 1)),
            "one over the limit must refuse"
        );
    }

    #[test]
    fn an_invalidated_node_carries_no_hash() {
        // entry_count -1 means "invalidated": no hash follows, and the next
        // node's bytes start immediately after the newline.
        let mut payload = node("", -1, 1);
        payload.extend(node("a", 1, 0));
        assert!(is_fine(cache_tree_depth(&payload, 2)));
    }

    /// The fail-closed fix: a payload that promises a child (`subtree_count:
    /// 1`) but does not contain one is refused, not silently declared "not
    /// too deep". Before this fix, a mid-chain parse failure returned `false`
    /// ("not too deep") having only counted the levels it managed to read —
    /// exactly the "found nothing" vs. "the check broke" confusion the module
    /// doc warns about.
    #[test]
    fn truncated_payload_is_unreadable_not_fine() {
        let payload = node("", 1, 1);
        assert!(is_unreadable(cache_tree_depth(&payload, 256)));
    }

    /// The same truncation, but positioned so that a short `limit` is
    /// exceeded before the walk would ever need to read the missing child —
    /// pinning down that the depth check still fires as `TooDeep`, not
    /// `Unreadable`, when it can be decided first (the check runs
    /// immediately after each push, before the next node is parsed).
    #[test]
    fn too_deep_is_reported_even_when_the_chain_is_truncated_further_in() {
        let depth = 5;
        let mut payload = Vec::new();
        for i in 0..depth {
            payload.extend(node(&format!("d{i}"), 1, 1)); // every node still promises one more child
        }
        // No more bytes after `depth` nodes, even though the last one promised
        // a child — but a limit of `depth - 1` is exceeded before the walk
        // would ever need to read that missing child.
        assert!(is_too_deep(cache_tree_depth(&payload, depth - 1)));
    }

    #[test]
    fn an_index_with_no_tree_extension_is_not_refused() {
        // DIRC header, version 2, 0 entries, no extensions, 20-byte checksum.
        let mut bytes = Vec::new();
        bytes.extend_from_slice(b"DIRC");
        bytes.extend_from_slice(&2u32.to_be_bytes());
        bytes.extend_from_slice(&0u32.to_be_bytes());
        bytes.extend_from_slice(&[0u8; HASH_LEN]);
        assert!(refuse_if_cache_tree_too_deep("status", &bytes).is_ok());
    }

    #[test]
    fn an_unrecognized_version_is_left_for_gix_to_reject() {
        let mut bytes = Vec::new();
        bytes.extend_from_slice(b"DIRC");
        bytes.extend_from_slice(&99u32.to_be_bytes());
        bytes.extend_from_slice(&0u32.to_be_bytes());
        bytes.extend_from_slice(&[0u8; HASH_LEN]);
        assert!(refuse_if_cache_tree_too_deep("status", &bytes).is_ok());
    }

    /// A dangling extension header — fewer than 8 bytes left, but not zero —
    /// is refused, not read as "no more extensions". Exercises
    /// `ExtensionsRegion::Extensions` reaching `next_extension` with data
    /// that is short but nonempty, the fail-closed twin of the header-level
    /// `NothingToCheck` cases above.
    #[test]
    fn a_dangling_extension_header_is_refused() {
        let mut bytes = Vec::new();
        bytes.extend_from_slice(b"DIRC");
        bytes.extend_from_slice(&2u32.to_be_bytes());
        bytes.extend_from_slice(&0u32.to_be_bytes());
        // 3 bytes: not enough for a signature + size, and not empty either.
        bytes.extend_from_slice(&[0u8; 3]);
        bytes.extend_from_slice(&[0u8; HASH_LEN]);
        let err = refuse_if_cache_tree_too_deep("status", &bytes).expect_err("must refuse");
        assert!(matches!(err, GitError::IndexTreeUnreadable { .. }));
    }
}
