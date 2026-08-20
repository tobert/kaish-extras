//! Format a [`Response`] as text (stdout/stderr or into a file) or JSON.


use serde_json::{json, Value};

use crate::model::Response;

/// Render the response in text form for kaish output.
///
/// When `include_headers` is true, response headers are printed above the body.
pub fn render_text(response: &Response, include_headers: bool, head_only: bool) -> String {
    let mut out = String::new();

    if include_headers {
        out.push_str(&format!("HTTP/1.1 {}\r\n", response.status));
        for (name, value) in &response.headers {
            out.push_str(&format!("{name}: {value}\r\n"));
        }
        out.push_str("\r\n");
    }

    // `-I` asked for headers; a HEAD response has no body to print anyway.
    if !head_only {
        out.push_str(&String::from_utf8_lossy(&response.body));
    }

    out
}

/// Build a structured JSON object for kaish --json output.
pub fn render_json(response: &Response) -> Value {
    let body_str = String::from_utf8_lossy(&response.body).to_string();

    json!({
        "status": response.status,
        "url": response.url,
        "headers": response.headers,
        "body": body_str,
    })
}
