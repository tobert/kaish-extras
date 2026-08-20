//! Format a [`Response`] as text (stdout/stderr or into a file) or JSON.


use base64::{engine::general_purpose::STANDARD as BASE64, Engine as _};
use serde_json::{json, Value};

use crate::model::Response;

/// Render the response in text form for kaish output.
///
/// When `include_headers` is true, response headers are printed above the body.
pub fn render_text(response: &Response, include_headers: bool, head_only: bool) -> String {
    let mut out = String::new();

    if include_headers {
        out.push_str(&format!("HTTP/1.1 {}\r\n", response.status));
        for (name, values) in &response.headers {
            // One line per value, as the server sent them — three
            // `Set-Cookie`s print as three lines, which is what curl does.
            for value in values {
                out.push_str(&format!("{name}: {value}\r\n"));
            }
        }
        out.push_str("\r\n");
    }

    // `-I` asked for headers; a HEAD response has no body to print anyway.
    if !head_only {
        out.push_str(&String::from_utf8_lossy(&response.body));
    }

    out
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
