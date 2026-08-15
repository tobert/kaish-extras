//! Argument parsing and refused-flag detection for kaish-curl.
//!
//! The public interface is [`parse_args`] — takes raw argv tokens and produces
//! a [`Request`] that the ureq backend executes. Early-return refusals for
//! every denied flag produce literate errors per docs/curl.md's "Literate errors" section.

use kaish_types::ToolArgs;
use kaish_types::Value;

use crate::config::{CurlConfig};
use crate::error::CurlError;

/// Flags this tool understands. Used by refused-flag detection.
const KNOWN_FLAGS: &[&str] = &[
    "--url", "-X", "--request",
    "--data", "-d", "--data-binary", "--data-raw", "--data-urlencode",
    "-H", "--header",
    "-i", "--include", "-I", "--head",
    "-o", "--output",
    "-L", "--location", "--max-redirs",
    "-u", "--user", "-A", "--user-agent", "-e", "--referer",
    "-k", "--insecure",
    "-f", "--fail",
    "--max-time", "--connect-timeout",
    "--unix-socket",
    "--json",
    "--compressed",
];

/// Parsed, validated curl invocation ready for the backend.
#[derive(Debug)]
pub struct Request {
    pub url: String,
    pub method: String,
    /// Body parts joined with `&` (from repeat `-d`). Each may be inline data or
    /// a VFS path prefixed with `@`.
    pub bodies: Vec<String>,
    /// Header name:value pairs (may include a User-Agent if user set one via -H).
    pub headers: Vec<(String, String)>,
    /// Whether to include response headers in stdout. With `-o`, headers go into
    /// the output file alongside the body.
    pub include_headers: bool,
    /// HEAD-only: print headers, discard body.
    pub head_only: bool,
    /// Write body to this VFS path instead of stdout.
    pub output_file: Option<String>,
    /// Basic auth user (password is None when only username provided).
    pub user: Option<String>,
    pub password: Option<String>,
    pub insecure: bool,
    /// Follow redirects? Some(u32) means follow up to N; None means don't follow.
    pub follow_redirects: Option<u32>,
    pub fail_on_error: bool,
    /// Path to AF_UNIX socket.
    pub unix_socket: Option<String>,
    /// Whether user explicitly set User-Agent via `-H User-Agent:...` or `-A`.
    /// Used by tool.rs to avoid duplicating the UA header.
    pub ua_explicit: bool,
}

/// Parse curl-style ToolArgs into a validated request.
///
/// Returns an error for:
/// - Refused flags (literate error)
/// - Unsupported schemes
/// - Missing URL
/// - Conflicting options (--head + --data)
pub fn parse_args(args: &ToolArgs, config: &CurlConfig) -> Result<Request, CurlError> {
    let trimmed = trim_argv(args);

    // Check for refused flags (early return with literate error).
    if let Some(refused) = find_refused_flag(&trimmed) {
        return Err(CurlError::Transport(refused));
    }

    // Extract individual flags.
    let include_headers = trimmed.contains(&"-i".to_string()) || trimmed.contains(&"--include".to_string());
    let head_only = trimmed.contains(&"-I".to_string()) || trimmed.contains(&"--head".to_string());
    let fail_on_error = trimmed.contains(&"-f".to_string()) || trimmed.contains(&"--fail".to_string());
    let insecure = trimmed.contains(&"-k".to_string()) || trimmed.contains(&"--insecure".to_string());

    // Method override.
    let method = if let Some(m) = find_single_arg(&trimmed, "-X") { m }
        else if head_only { "HEAD".to_string() }
        else if has_any_of(&trimmed, &["-d".into(), "--data".into(), "--data-binary".into(),
                                       "--data-raw".into(), "--data-urlencode".into()]) {
            "POST".to_string()
        } else { "GET".to_string() };

    // URL extraction.
    let mut url = find_positional(&trimmed);

    // Override with --url if present.
    if let Some(u) = find_single_arg(&trimmed, "--url") {
        url = Some(u);
    }

    let url = match url {
        Some(u) => u,
        None => {
            return Err(CurlError::MalformedUrl {
                url: "<missing>".into(),
                reason: "URL is required — one positional argument or --url must specify the target".into(),
            });
        }
    };

    // Validate scheme.
    if !url.starts_with("http://") && !url.starts_with("https://") {
        return Err(CurlError::MalformedUrl {
            url: url.clone(),
            reason: "only http and https schemes are supported".to_string(),
        });
    }

    // Options scanning.
    let mut bodies = Vec::new();
    let mut headers = Vec::new();
    let mut output_file: Option<String> = None;
    let mut user: Option<String> = None;
    let mut password: Option<String> = None;
    let mut max_redirs: Option<u32> = None;
    let mut unix_socket: Option<String> = None;
    let mut referer: Option<String> = None;
    let mut ua_explicit: bool = false;

    let mut i = 0;
    while i < trimmed.len() {
        match trimmed[i].as_str() {
            "-d" | "--data" | "--data-binary" | "--data-raw" => {
                i += 1;
                if let Some(val) = trimmed.get(i) {
                    bodies.push(val.clone());
                }
            }
            "--data-urlencode" => {
                // Deferred: accept name=value forms only; @filename deferred to CU5.
                i += 1;
                if let Some(val) = trimmed.get(i) {
                    bodies.push(format!("@{}", val));
                }
            }
            "-H" | "--header" => {
                i += 1;
                if let Some(hv) = trimmed.get(i) {
                    if let Some((name, value)) = hv.split_once(':') {
                        if name.trim().eq_ignore_ascii_case("user-agent") {
                            ua_explicit = true;
                        }
                        headers.push((name.trim().to_string(), value.trim().to_string()));
                    }
                }
            }
            "-o" | "--output" => {
                i += 1;
                if let Some(path) = trimmed.get(i) {
                    output_file = Some(path.clone());
                }
            }
            "-u" | "--user" => {
                i += 1;
                if let Some(auth) = trimmed.get(i) {
                    if let Some((u, p)) = auth.split_once(':') {
                        user = Some(u.to_string());
                        password = Some(p.to_string());
                    } else {
                        user = Some(auth.clone());
                    }
                }
            }
            "-A" | "--user-agent" => {
                i += 1;
                if trimmed.get(i).is_some() {
                    ua_explicit = true;
                }
            }
            "-e" | "--referer" => {
                i += 1;
                if let Some(r) = trimmed.get(i) {
                    referer = Some(r.clone());
                }
            }
            "-L" | "--location" => {
                max_redirs = Some(config.limits().max_redirects);
            }
            "--max-redirs" => {
                i += 1;
                if let Some(n) = trimmed.get(i) {
                    if let Ok(count) = n.parse::<u32>() {
                        max_redirs = Some(count);
                    }
                }
            }
            "--unix-socket" => {
                i += 1;
                if let Some(path) = trimmed.get(i) {
                    unix_socket = Some(path.clone());
                }
            }
            _ => {}
        }
        i += 1;
    }

    // Add referer header if specified.
    if let Some(r) = referer {
        headers.push(("Referer".to_string(), r));
    }

    // Head + data is nonsensical.
    if head_only && !bodies.is_empty() {
        return Err(CurlError::MalformedUrl {
            url: url.clone(),
            reason: "--head discards the response body; providing --data is meaningless".into(),
        });
    }

    Ok(Request {
        url,
        method,
        bodies,
        headers,
        include_headers,
        head_only,
        output_file,
        user,
        password,
        insecure,
        follow_redirects: max_redirs,
        fail_on_error,
        unix_socket,
        ua_explicit,
    })
}

fn find_refused_flag(trimmed: &[String]) -> Option<String> {
    const REFUSED: &[(&str, &str)] = &[
        ("-O", "curl: '-O' is not supported. Use '-o <file>' for explicit output path."),
        ("-s", "curl: '-s' is not supported. This build has no progress meter to suppress."),
        ("-S", "curl: '-S' is not supported. Error display is always enabled."),
        ("--compressed", "curl: '--compressed' is not supported. Decompression is automatic."),
        ("--form", "curl: '--form' (multipart) is not supported. Use '--data' for application/x-www-form-urlencoded."),
        ("--cookie", "curl: '--cookie' is not supported. Send cookies with '-H Cookie:<value>'."),
        ("--verbose", "curl: '--verbose'/'-v' is not supported. Use kaish '--json' for structured output."),
        ("-v", "curl: '--verbose'/'-v' is not supported. Use kaish '--json' for structured output."),
        ("--write-out", "curl: '--write-out' is not supported. Use kaish '--json' for structured response objects."),
        ("--retry", "curl: '--retry' is not supported. Retry transient failures from the shell loop."),
        ("--get", "curl: '--get'/'-G' is not supported. Put query strings in the URL."),
        ("-G", "curl: '--get'/'-G' is not supported. Put query strings in the URL."),
        ("--cert", "curl: '--cert' is not supported. Client certificates are not available."),
        ("--key", "curl: '--key' is not supported. Client key files are not available."),
        ("--proxy", "curl: '--proxy' is not supported. Direct connections only."),
        ("--config", "curl: '--config'/'-K' is not supported. Pass flags on the command line."),
        ("-K", "curl: '--config'/'-K' is not supported. Pass flags on the command line."),
        ("--netrc", "curl: '--netrc' is not supported. Use '--user <user[:pass]>' for credentials."),
        ("--resolve", "curl: '--resolve' is not supported. DNS resolution uses the system resolver."),
    ];

    for (flag, msg) in REFUSED {
        if trimmed.iter().any(|t| t == *flag) {
            return Some(msg.to_string());
        }
    }
    None
}

// ---- Helpers ----

fn trim_argv(args: &ToolArgs) -> Vec<String> {
    let mut result = Vec::new();
    for v in &args.positional {
        if let Value::String(s) = v {
            result.push(s.clone());
        }
    }
    for flag in &args.flags {
        result.push(flag.clone());
    }
    for (name, val) in &args.named {
        if let Value::String(v) = val {
            result.push(format!("--{}={}", name, v));
        } else {
            result.push(format!("--{}", name));
        }
    }
    result
}

fn find_positional(args: &[String]) -> Option<String> {
    let mut i = 0;
    while i < args.len() {
        if args[i].starts_with("--") || (args[i].starts_with("-") && args[i].len() > 1) {
            let takes_values = matches!(args[i].as_str(), "-d"|"-X"|"--data"|"--data-binary"|"--data-raw"|"--data-urlencode"|"-A"|"-e"|"-H"|"-o"|"--header"|"--output"|"--request"|"-u"|"-L"|"--location"|"--max-redirs"|"--max-time"|"--unix-socket");
            let needs_value = !KNOWN_FLAGS.contains(&args[i].as_str()) || args[i].contains('=') || takes_values
                || args[i].contains('=');
            if needs_value && i + 1 < args.len() && !args[i + 1].starts_with('-') {
                i += 2;
            } else {
                i += 1;
            }
        } else {
            return Some(args[i].clone());
        }
    }
    None
}

fn find_single_arg(args: &[String], flag: &str) -> Option<String> {
    for i in 0..args.len() {
        if args[i] == flag {
            if i + 1 < args.len() && !args[i + 1].starts_with('-') {
                return Some(args[i + 1].clone());
            }
        }
    }
    None
}

fn has_any_of(args: &[String], flags: &[String]) -> bool {
    args.iter().any(|a| flags.contains(a))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn refused_flags_catches_properly() {
        let args = vec!["-O".to_string(), "http://example.com".into()];
        assert!(find_refused_flag(&args).is_some());

        let args = vec!["--form".into(), "field=value".into()];
        assert!(find_refused_flag(&args).is_some());

        let args = vec!["http://example.com".into()];
        assert!(find_refused_flag(&args).is_none());
    }

    #[test]

    #[test]
    fn find_positional_skips_flags_with_values() {
        let args = vec![
            "-i".into(), "-o".into(), "/tmp/out".into(),
            "-H".into(), "X-Foo:bar".into(),
            "http://x.com".into(),
        ];
        assert_eq!(find_positional(&args), Some("http://x.com".into()));
    }

    #[test]
    fn find_positional_returns_first_non_flag() {
        let args = vec!["http://x.com".into(), "-d".into()];
        assert_eq!(find_positional(&args), Some("http://x.com".into()));
    }
}
