//! Turn the parsed command line into bytes: read the files it names, through
//! the shell's filesystem, inside the embedder's byte budget.
//!
//! The parser has no filesystem, so `-d @file` and its relatives arrive as
//! [`BodyPart`]s. [`prepare`] is the one place they are read. Every read goes
//! through `ToolCtx::resolve_path` and `ToolCtx::backend`, never `std::fs`, so
//! a path can reach only what the shell's own `cat` can.
//!
//! The byte budget is `Limits::max_response_bytes`. It is the one budget the
//! embedder sets for bytes held in memory, and it bounds a request body the
//! same way it bounds a response body. A file is checked by `stat` before it is
//! read, so an oversize file is refused without being loaded, and the joined
//! body is checked again, so two files that fit alone cannot exceed it
//! together.

use kaish_tool_api::ToolCtx;
use kaish_types::DirEntryKind;

use crate::args::{percent_encode, BodyPart, Request};
use crate::error::CurlError;

/// Resolve every file the request names and fill [`Request::body`].
pub async fn prepare(
    req: &mut Request,
    ctx: &dyn ToolCtx,
    max_bytes: u64,
) -> Result<(), CurlError> {
    if req.bodies.is_empty() {
        return Ok(());
    }
    let mut joined: Vec<u8> = Vec::new();
    for (n, part) in req.bodies.iter().enumerate() {
        if n > 0 {
            joined.push(b'&');
        }
        match part {
            BodyPart::Literal(text) => joined.extend_from_slice(text.as_bytes()),
            BodyPart::File { path, strip_line_breaks } => {
                let mut bytes = read_file(ctx, path, max_bytes).await?;
                if *strip_line_breaks {
                    bytes.retain(|b| *b != b'\r' && *b != b'\n');
                }
                joined.extend_from_slice(&bytes);
            }
            BodyPart::EncodedFile { name, path } => {
                let bytes = read_file(ctx, path, max_bytes).await?;
                if let Some(name) = name {
                    joined.extend_from_slice(name.as_bytes());
                    joined.push(b'=');
                }
                joined.extend_from_slice(percent_encode(&bytes).as_bytes());
            }
        }
        check_total(joined.len() as u64, max_bytes)?;
    }
    req.body = joined;
    Ok(())
}

/// The whole body, past the budget, is refused with the number it reached.
fn check_total(len: u64, max_bytes: u64) -> Result<(), CurlError> {
    if len > max_bytes {
        return Err(CurlError::Transport(format!(
            "the request body is over the {max_bytes}-byte limit this embedder allows, and no request was sent. \
             Send less data, or ask the embedder to raise `Limits::max_response_bytes`."
        )));
    }
    Ok(())
}

/// Read one file named on the command line. `Err` is exit 26 for a file that
/// cannot be read, and exit 1 for one over the budget.
pub async fn read_file(
    ctx: &dyn ToolCtx,
    path: &str,
    max_bytes: u64,
) -> Result<Vec<u8>, CurlError> {
    let unreadable = |reason: String| CurlError::ReadFailed { path: path.to_string(), reason };
    let vfs = ctx.resolve_path(path);
    let backend = ctx.backend();
    let entry = backend.stat(&vfs).await.map_err(|e| unreadable(e.to_string()))?;
    if entry.kind == DirEntryKind::Directory {
        return Err(unreadable("it is a directory".into()));
    }
    if entry.size > max_bytes {
        return Err(CurlError::Transport(format!(
            "'{path}' is {} bytes, over the {max_bytes}-byte limit this embedder allows, and no request was sent. \
             Send a smaller file, or ask the embedder to raise `Limits::max_response_bytes`.",
            entry.size
        )));
    }
    let bytes = backend.read(&vfs, None).await.map_err(|e| unreadable(e.to_string()))?;
    // `stat` can be stale or approximate on a virtual file; the bytes read are
    // the truth.
    if bytes.len() as u64 > max_bytes {
        return Err(CurlError::Transport(format!(
            "'{path}' is over the {max_bytes}-byte limit this embedder allows, and no request was sent. \
             Send a smaller file, or ask the embedder to raise `Limits::max_response_bytes`."
        )));
    }
    Ok(bytes)
}
