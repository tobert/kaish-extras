//! `-F` / `--form`: parse the part grammar, and build the multipart body.
//!
//! The grammar is curl's, for the parts this build carries:
//!
//! ```text
//! name=text                 a text part
//! name=@path                a file part: filename and Content-Type from the file
//! name=<path                the file's bytes as the value, no filename
//! ...;type=MIME             set the part's Content-Type
//! ...;filename=NAME         set the filename shown (a text part gains one)
//! ```
//!
//! A value may be double-quoted to hold a `;`; inside quotes `\"` and `\\`
//! stand for `"` and `\`. A `;` followed by a space stays in the value, which
//! is how `;type=text/plain; charset=x` keeps its charset.
//!
//! What curl accepts and this build refuses, each with exit 2 and no request:
//! `@a,b` (several files in one part), `;headers=`, `;encoder=`, `;format=`,
//! an unknown `;key=`, a repeated `;type=`, an empty name, an empty `;type=`,
//! and stdin (`@-`, `<-`). curl drops an unknown key or sends a nameless part
//! without a word, and a silent drop is the failure this surface avoids.

use crate::error::CurlError;

/// One `-F` part, before any file is read.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FormPart {
    /// The field name, as typed. Escaped (`"` as `%22`) when sent.
    pub name: String,
    /// Where the bytes come from.
    pub source: FormSource,
    /// `;filename=`, when given.
    pub filename: Option<String>,
    /// `;type=`, when given.
    pub content_type: Option<String>,
}

/// Where a part's bytes come from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FormSource {
    /// `name=text`.
    Text(String),
    /// `name=@path`: sent as a file.
    File(String),
    /// `name=<path`: the file's bytes as a plain value.
    FileContents(String),
}

fn refuse(spec: &str, why: &str) -> CurlError {
    CurlError::Usage(format!(
        "'-F {spec}' {why}, and no request was sent (exit 2). The form is name=value, name=@file or name=<file, with optional ;type=MIME and ;filename=NAME."
    ))
}

/// Parse one `-F` argument.
pub fn parse(spec: &str) -> Result<FormPart, CurlError> {
    let Some((name, rest)) = spec.split_once('=') else {
        return Err(refuse(spec, "has no '=', so it names no field"));
    };
    if name.is_empty() {
        return Err(refuse(spec, "has an empty field name (curl would send a part with no name; this build will not)"));
    }
    let (kind, rest) = match rest.chars().next() {
        Some('@') => ('@', &rest[1..]),
        Some('<') => ('<', &rest[1..]),
        _ => ('t', rest),
    };
    let (value, after) = take_value(rest, spec)?;
    let mut part = FormPart {
        name: name.to_string(),
        source: match kind {
            '@' => FormSource::File(value.clone()),
            '<' => FormSource::FileContents(value.clone()),
            _ => FormSource::Text(value.clone()),
        },
        filename: None,
        content_type: None,
    };
    if kind != 't' {
        if value == "-" {
            return Err(refuse(spec, "reads stdin, which a tool cannot do through ToolCtx (docs/issues.md CU47). Write the input to a file first and name it"));
        }
        // An unquoted comma lists several files in curl and makes a nested
        // multipart/mixed part. One file per part here.
        if kind == '@' && value.contains(',') && !rest.starts_with('"') {
            return Err(refuse(spec, "lists more than one file with a comma; this build sends one file per part. Use one -F per file"));
        }
    }
    let mut params = after;
    while let Some(tail) = params.strip_prefix(';') {
        let (key, tail) = match tail.split_once('=') {
            Some((k, t)) => (k.trim(), t),
            // A trailing ';' with nothing after it is accepted by curl.
            None if tail.trim().is_empty() => return Ok(part),
            None => return Err(refuse(spec, "has a ';' parameter with no '='")),
        };
        let (val, next) = take_value(tail, spec)?;
        match key {
            "type" => {
                if val.is_empty() {
                    return Err(refuse(spec, "has an empty ;type="));
                }
                if val.contains(['\r', '\n']) {
                    return Err(refuse(spec, "has a line break in ;type="));
                }
                if part.content_type.replace(val).is_some() {
                    return Err(refuse(spec, "repeats ;type="));
                }
            }
            "filename" => part.filename = Some(val),
            "headers" | "encoder" | "format" => {
                return Err(refuse(spec, &format!("uses ;{key}=, which this build does not carry (headers, encoder and format parameters are not supported)")));
            }
            other => return Err(refuse(spec, &format!("has an unknown parameter ';{other}='"))),
        }
        params = next;
    }
    Ok(part)
}

/// Read one value: a double-quoted string, or text up to the next `;` that is
/// not followed by a space.
/// Returns the value and the remainder, which is empty or starts with `;`.
fn take_value<'a>(s: &'a str, spec: &str) -> Result<(String, &'a str), CurlError> {
    if let Some(quoted) = s.strip_prefix('"') {
        let mut out = String::new();
        let mut chars = quoted.char_indices();
        while let Some((i, c)) = chars.next() {
            match c {
                '\\' => match chars.next() {
                    Some((_, e @ ('"' | '\\'))) => out.push(e),
                    Some((_, other)) => {
                        out.push('\\');
                        out.push(other);
                    }
                    None => break,
                },
                '"' => {
                    let rest = &quoted[i + 1..];
                    if rest.is_empty() || rest.starts_with(';') {
                        return Ok((out, rest));
                    }
                    return Err(refuse(spec, "has text after a closing quote"));
                }
                c => out.push(c),
            }
        }
        return Err(refuse(spec, "has an opening quote that is never closed"));
    }
    let bytes = s.as_bytes();
    let mut end = bytes.len();
    for (i, b) in bytes.iter().enumerate() {
        if *b == b';' && !bytes.get(i + 1).is_some_and(|n| n.is_ascii_whitespace()) {
            end = i;
            break;
        }
    }
    Ok((s[..end].to_string(), &s[end..]))
}

/// curl's extension table for a file part's Content-Type, matched without
/// regard to case. `None` for an extension the table lacks.
pub fn guess_content_type(filename: &str) -> Option<&'static str> {
    let ext = filename.rsplit_once('.').map(|(_, e)| e.to_ascii_lowercase());
    match ext.as_deref() {
        Some("gif") => Some("image/gif"),
        Some("jpg" | "jpeg") => Some("image/jpeg"),
        Some("png") => Some("image/png"),
        Some("svg") => Some("image/svg+xml"),
        Some("txt") => Some("text/plain"),
        Some("htm" | "html") => Some("text/html"),
        Some("pdf") => Some("application/pdf"),
        Some("xml") => Some("application/xml"),
        _ => None,
    }
}

/// Escape a name or filename for a quoted header parameter, as curl 8 does.
fn escape(s: &str) -> String {
    s.replace('"', "%22").replace('\r', "%0D").replace('\n', "%0A")
}

/// One part with its bytes in hand.
pub struct LoadedPart<'a> {
    pub part: &'a FormPart,
    pub data: Vec<u8>,
}

/// Build the body for `parts`, and the boundary it uses.
pub fn build(parts: &[LoadedPart<'_>]) -> (String, Vec<u8>) {
    let boundary = pick_boundary(parts);
    let mut body = Vec::new();
    for LoadedPart { part, data } in parts {
        body.extend_from_slice(format!("--{boundary}\r\n").as_bytes());
        let (filename, content_type) = match &part.source {
            // A text part given a filename becomes a file part to the server,
            // and gets a type from that name when the extension is in the table,
            // none when it is not (checked against curl 8.23).
            FormSource::Text(_) => {
                let ct = part.content_type.clone().or_else(|| {
                    part.filename
                        .as_deref()
                        .and_then(guess_content_type)
                        .map(str::to_string)
                });
                (part.filename.clone(), ct)
            }
            FormSource::FileContents(_) => (None, part.content_type.clone()),
            FormSource::File(path) => {
                let shown = part
                    .filename
                    .clone()
                    .unwrap_or_else(|| path.rsplit('/').next().unwrap_or(path).to_string());
                // curl tries the name shown first, then the file's own name,
                // then gives up with a generic type.
                let own = path.rsplit('/').next().unwrap_or(path);
                let ct = part.content_type.clone().unwrap_or_else(|| {
                    guess_content_type(&shown)
                        .or_else(|| guess_content_type(own))
                        .unwrap_or("application/octet-stream")
                        .to_string()
                });
                (Some(shown), Some(ct))
            }
        };
        body.extend_from_slice(
            format!("Content-Disposition: form-data; name=\"{}\"", escape(&part.name)).as_bytes(),
        );
        if let Some(f) = filename {
            body.extend_from_slice(format!("; filename=\"{}\"", escape(&f)).as_bytes());
        }
        body.extend_from_slice(b"\r\n");
        if let Some(ct) = content_type {
            body.extend_from_slice(format!("Content-Type: {ct}\r\n").as_bytes());
        }
        body.extend_from_slice(b"\r\n");
        body.extend_from_slice(data);
        body.extend_from_slice(b"\r\n");
    }
    body.extend_from_slice(format!("--{boundary}--\r\n").as_bytes());
    (boundary, body)
}

/// A boundary that appears in no part's data, in curl's shape: 24 dashes and
/// 22 letters and digits. Random per request, so a file cannot be written to
/// contain it ahead of time.
fn pick_boundary(parts: &[LoadedPart<'_>]) -> String {
    use std::hash::{BuildHasher, Hasher};
    const ALPHABET: &[u8] = b"0123456789abcdefghijklmnopqrstuvwxyzABCDEFGHIJKLMNOPQRSTUVWXYZ";
    // `RandomState` is seeded from the operating system per instance.
    let state = std::collections::hash_map::RandomState::new();
    let mut counter = 0u64;
    loop {
        let mut suffix = String::with_capacity(22);
        while suffix.len() < 22 {
            let mut h = state.build_hasher();
            h.write_u64(counter);
            counter += 1;
            let mut x = h.finish();
            for _ in 0..8 {
                if suffix.len() < 22 {
                    suffix.push(ALPHABET[(x % 62) as usize] as char);
                }
                x /= 62;
            }
        }
        let boundary = format!("------------------------{suffix}");
        let needle = boundary.as_bytes();
        let clash = parts
            .iter()
            .any(|p| p.data.windows(needle.len()).any(|w| w == needle));
        if !clash {
            return boundary;
        }
    }
}
