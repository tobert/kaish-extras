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
use kaish_types::backend::BackendError;
use kaish_types::DirEntryKind;

use crate::args::{percent_encode, BodyPart, Request};
use crate::error::CurlError;

/// Resolve every file the request names and fill [`Request::body`].
pub async fn prepare(
    req: &mut Request,
    ctx: &dyn ToolCtx,
    max_bytes: u64,
) -> Result<(), CurlError> {
    if req.resume {
        add_resume_range(req, ctx).await?;
    }
    if let Some(path) = req.upload_file.clone() {
        req.body = read_file(ctx, &path, max_bytes).await?;
        add_file_name_to_url(&mut req.url, &path);
        return Ok(());
    }
    if !req.forms.is_empty() {
        return prepare_form(req, ctx, max_bytes).await;
    }
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
    if req.get {
        append_query(&mut req.url, &joined)?;
    } else {
        req.body = joined;
    }
    Ok(())
}

/// `-G`: add the data to the URL's query. A URL cannot carry a space, a
/// control byte, `#`, or a byte outside ASCII unencoded, and curl refuses such
/// a URL (exit 3). Refuse before the request, and name the fix.
fn append_query(url: &mut String, data: &[u8]) -> Result<(), CurlError> {
    if let Some(bad) = data.iter().find(|b| !(0x21..=0x7e).contains(*b) || **b == b'#') {
        return Err(CurlError::Usage(format!(
            "'-G' data contains a byte a URL cannot carry ({:?}), so no request was sent (exit 2). \
             Percent-encode it with '--data-urlencode', as in: curl -G --data-urlencode 'q=a b' <url>",
            char::from(*bad)
        )));
    }
    let data = std::str::from_utf8(data).expect("ASCII checked above");
    // Keep a fragment out of the query: it stays at the end, as in a browser.
    let (base, fragment) = match url.split_once('#') {
        Some((b, f)) => (b.to_string(), Some(f.to_string())),
        None => (url.clone(), None),
    };
    let sep = match base.contains('?') {
        false => "?",
        true if base.ends_with('?') || base.ends_with('&') => "",
        true => "&",
    };
    *url = format!("{base}{sep}{data}");
    if let Some(f) = fragment {
        url.push('#');
        url.push_str(&f);
    }
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

/// `-F`: read every file part, build the multipart body, and name its
/// boundary in the Content-Type.
async fn prepare_form(
    req: &mut Request,
    ctx: &dyn ToolCtx,
    max_bytes: u64,
) -> Result<(), CurlError> {
    use crate::form::{build, FormSource, LoadedPart};

    let mut loaded = Vec::with_capacity(req.forms.len());
    let mut total: u64 = 0;
    for part in &req.forms {
        let data = match &part.source {
            FormSource::Text(text) => text.as_bytes().to_vec(),
            FormSource::File(path) | FormSource::FileContents(path) => {
                read_file(ctx, path, max_bytes).await?
            }
        };
        total += data.len() as u64;
        check_total(total, max_bytes)?;
        loaded.push(LoadedPart { part, data });
    }
    let (boundary, body) = build(&loaded);
    check_total(body.len() as u64, max_bytes)?;
    drop(loaded);

    // A Content-Type the caller set wins, as in curl; when it names a
    // multipart type without a boundary, ours is added so the body parses.
    match req
        .headers
        .iter_mut()
        .find(|(k, _)| k.eq_ignore_ascii_case("content-type"))
    {
        Some((_, v)) => {
            if v.to_ascii_lowercase().starts_with("multipart/") && !v.to_ascii_lowercase().contains("boundary=") {
                v.push_str(&format!("; boundary={boundary}"));
            }
        }
        None if req.suppressed.iter().any(|n| n == "content-type") => {}
        None => req
            .headers
            .push(("Content-Type".into(), format!("multipart/form-data; boundary={boundary}"))),
    }
    req.body = body;
    Ok(())
}

/// `-T` to a URL whose path ends in `/` names the file in it: curl appends
/// the file's basename. A URL with a query is left alone, as in curl.
fn add_file_name_to_url(url: &mut String, path: &str) {
    let Ok(mut parsed) = url::Url::parse(url) else {
        return; // the backend reports an unusable URL
    };
    if !parsed.path().ends_with('/') || parsed.query().is_some() {
        return;
    }
    let name = path.rsplit('/').next().unwrap_or(path);
    if name.is_empty() {
        return;
    }
    if let Ok(mut segments) = parsed.path_segments_mut() {
        segments.pop_if_empty().push(name);
    }
    *url = parsed.to_string();
}

/// `-C -`: ask for the bytes after what the output file already holds. A
/// missing or empty file sends no Range, and the download starts from zero.
async fn add_resume_range(req: &mut Request, ctx: &dyn ToolCtx) -> Result<(), CurlError> {
    let path = req.output_file.as_deref().expect("the parser requires -o or -O with -C -");
    let vfs = ctx.resolve_path(path);
    let unusable = |reason: String| CurlError::WriteFailed {
        path: vfs.display().to_string(),
        reason,
    };
    match ctx.backend().stat(&vfs).await {
        Ok(entry) if entry.kind == DirEntryKind::Directory => {
            Err(unusable("it is a directory".into()))
        }
        Ok(entry) if entry.size > 0 => {
            req.headers.push(("Range".into(), format!("bytes={}-", entry.size)));
            req.resume_from = Some(entry.size);
            Ok(())
        }
        Ok(_) | Err(BackendError::NotFound(_)) => Ok(()),
        Err(e) => Err(unusable(e.to_string())),
    }
}
