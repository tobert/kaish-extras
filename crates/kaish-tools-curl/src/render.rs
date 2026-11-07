//! Format a [`Response`] as text (stdout/stderr or into a file) or JSON.


use base64::{engine::general_purpose::STANDARD as BASE64, Engine as _};
use serde_json::{json, Value};

use crate::model::Response;

/// The status line, headers, and blank line, as `-i` and `-I` print them —
/// one block per redirect `-L` followed, then the final response's.
pub fn render_head(response: &Response) -> String {
    let mut out = String::new();
    for hop in &response.history {
        push_head(&mut out, hop.status, &hop.reason, &hop.headers);
    }
    push_head(&mut out, response.status, &response.reason, &response.headers);
    out
}

fn push_head(
    out: &mut String,
    status: u16,
    reason: &str,
    headers: &std::collections::BTreeMap<String, Vec<String>>,
) {
    // curl prints `HTTP/1.1 404 Not Found`. The phrase matters to a
    // script that splits the line on spaces: without it the last field
    // is `404\r` (survey SW-2).
    if reason.is_empty() {
        out.push_str(&format!("HTTP/1.1 {status}\r\n"));
    } else {
        out.push_str(&format!("HTTP/1.1 {status} {reason}\r\n"));
    }
    for (name, values) in headers {
        // One line per value, as the server sent them — three
        // `Set-Cookie`s print as three lines, which is what curl does.
        for value in values {
            out.push_str(&format!("{name}: {value}\r\n"));
        }
    }
    out.push_str("\r\n");
}

/// The bytes `-i`, `-I`, and `-o` put out: the head when asked for, then the
/// body unless the request was a HEAD. Bytes, not text — a binary body must
/// reach a file as it arrived.
pub fn render_bytes(response: &Response, include_headers: bool, head_only: bool) -> Vec<u8> {
    let mut out = Vec::new();
    if include_headers {
        out.extend_from_slice(render_head(response).as_bytes());
    }
    // `-I` asked for headers; a HEAD response has no body to print anyway.
    if !head_only {
        out.extend_from_slice(&response.body);
    }
    out
}

/// Render the response in text form for stdout, where the caller has already
/// refused a body that is not UTF-8.
pub fn render_text(response: &Response, include_headers: bool, head_only: bool) -> String {
    String::from_utf8_lossy(&render_bytes(response, include_headers, head_only)).into_owned()
}

/// Build a structured JSON object for kaish `--json` output.
///
/// A JSON response body is emitted as a **real object**, not as a string
/// holding JSON. It used to be the latter, so an agent piping `curl --json`
/// into `jq` had to parse JSON out of the middle of JSON before it could
/// branch on anything (CU45). `body_format` says which shape `body` took, so
/// nothing has to guess.
pub fn render_json(response: &Response) -> Value {
    let (body, body_format) = match std::str::from_utf8(&response.body) {
        Ok(text) => match serde_json::from_str::<Value>(text) {
            // Only when the server said so: a text/plain body that happens to
            // be the word `null` or a bare number is text, not JSON.
            Ok(parsed) if response.is_json() => (parsed, "json"),
            _ => (Value::String(text.to_string()), "text"),
        },
        // Binary reaches an agent intact or not at all — `from_utf8_lossy`
        // would hand back a body peppered with U+FFFD and call it the
        // response (CU41).
        Err(_) => (
            Value::String(BASE64.encode(&response.body)),
            "base64",
        ),
    };

    json!({
        "status": response.status,
        "url": response.url,
        "headers": response.headers,
        "body": body,
        "body_format": body_format,
    })
}
