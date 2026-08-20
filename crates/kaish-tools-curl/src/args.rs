//! Argument parsing and refused-flag detection for kaish-curl.
//!
//! [`parse_args`] is the **only** parser in this crate: it takes raw argv
//! tokens and produces a [`Request`] the backend executes. `tool.rs` used to
//! carry an inline second copy, and the two drifted — the copy silently
//! ignored `-I`, `-A`, `-e`, `--max-redirs` and `--data-urlencode`, all of
//! which docs/curl.md's flag table promises. One parser, one place a flag can
//! be honored or refused.
//!
//! A flag the backend cannot honor is **refused here**, never accepted and
//! dropped: accepting `-k` and then verifying the certificate anyway does the
//! opposite of what the caller asked, and says nothing about it.

use kaish_types::ToolArgs;
use kaish_types::Value;

use crate::config::CurlConfig;
use crate::error::CurlError;

/// Flags this tool understands, for positional detection and refusal.
const KNOWN_FLAGS: &[&str] = &[
    "--url", "-X", "--request",
    "--data", "-d", "--data-binary", "--data-raw", "--data-urlencode",
    "-H", "--header",
    "-i", "--include", "-I", "--head",
    "-o", "--output",
    "-L", "--location", "--max-redirs",
    "-u", "--user", "-A", "--user-agent", "-e", "--referer",
    "-f", "--fail",
    "--max-time", "--connect-timeout",
];

/// Flags that take the next token as their value. Positional detection has to
/// know these or it mistakes a flag's value for the URL — and, just as badly,
/// must *not* list valueless flags like `-L`, or `curl -L <url>` loses its URL.
const VALUE_FLAGS: &[&str] = &[
    "-X", "--request",
    "-d", "--data", "--data-binary", "--data-raw", "--data-urlencode",
    "-H", "--header",
    "-o", "--output",
    "--max-redirs",
    "-u", "--user", "-A", "--user-agent", "-e", "--referer",
    "--max-time", "--connect-timeout",
    "--url",
];

/// The default `User-Agent`, sent when the caller names none.
const DEFAULT_USER_AGENT: &str = "kaish-curl";

/// Parsed, validated curl invocation ready for the backend.
///
/// Every field here is read by the backend. A field the backend ignores is a
/// flag that lies, so there is no place to park one.
#[derive(Debug)]
pub struct Request {
    pub url: String,
    pub method: String,
    /// Body parts, joined with `&` by the backend.
    pub bodies: Vec<String>,
    /// Header name/value pairs, including the `User-Agent` (the caller's or
    /// [`DEFAULT_USER_AGENT`]) and any `Referer` from `-e`.
    pub headers: Vec<(String, String)>,
    /// Print response headers above the body (`-i`).
    pub include_headers: bool,
    /// `-I`: HEAD request, headers only, body discarded.
    pub head_only: bool,
    /// Write the body to this VFS path instead of stdout (`-o`).
    pub output_file: Option<String>,
    /// Basic auth user; `password` is `None` when the caller gave no `:pass`.
    pub user: Option<String>,
    pub password: Option<String>,
    /// `-L`: follow redirects. Separate from the cap, because "not
    /// following" and "following, cap 0" are different answers (CU36) —
    /// conflating them into one `Option<u32>` silently discarded a
    /// `--max-redirs` given without `-L` under `RedirectPolicy::Auto`.
    pub follow_redirects: bool,
    /// `--max-redirs`, when the caller named one. The embedder's ceiling
    /// applies regardless.
    pub max_redirects: Option<u32>,
    /// `-f`: fail with exit 22 on status >= 400 instead of printing the body.
    pub fail_on_error: bool,
    /// Whole-request deadline in seconds. Always set — the `CurlConfig`
    /// default applies when the caller omits `--max-time`, so an agent cannot
    /// hang the embedder by leaving it off.
    pub max_time: f64,
    /// Connect-phase deadline in seconds, when the caller asked for one.
    pub connect_timeout: Option<f64>,
}

/// Parse curl-style [`ToolArgs`] into a validated [`Request`].
///
/// Errors on a refused flag, a missing or non-http(s) URL, an `@path` body
/// (deferred — CU5), and `-I` together with `--data`.
pub fn parse_args(args: &ToolArgs, config: &CurlConfig) -> Result<Request, CurlError> {
    let trimmed = trim_argv(args);

    if let Some(refused) = find_refused_flag(&trimmed) {
        return Err(CurlError::Transport(refused));
    }

    let include_headers = has_flag(&trimmed, &["-i", "--include"]);
    let head_only = has_flag(&trimmed, &["-I", "--head"]);
    let fail_on_error = has_flag(&trimmed, &["-f", "--fail"]);

    let mut bodies: Vec<String> = Vec::new();
    let mut headers: Vec<(String, String)> = Vec::new();
    let mut output_file: Option<String> = None;
    let mut user: Option<String> = None;
    let mut password: Option<String> = None;
    let mut method_override: Option<String> = None;
    let mut url_flag: Option<String> = None;
    let mut max_redirs: Option<u32> = None;
    let mut referer: Option<String> = None;
    let mut user_agent: Option<String> = None;
    let mut max_time: Option<f64> = None;
    let mut connect_timeout: Option<f64> = None;
    let mut follow = false;

    let mut i = 0;
    while i < trimmed.len() {
        // Every value-taking arm below advances `i` past its value, so a
        // value that looks like a flag (`-H` `-weird: v`) is never re-read as
        // one.
        let value = |i: &mut usize| -> Option<String> {
            *i += 1;
            trimmed.get(*i).cloned()
        };
        match trimmed[i].as_str() {
            "-d" | "--data" | "--data-binary" | "--data-raw" => {
                let raw = trimmed[i].as_str() == "--data-raw";
                if let Some(val) = value(&mut i) {
                    // `--data-raw` takes `@` literally; the others would read
                    // a file, which is deferred (CU5).
                    if !raw && val.starts_with('@') {
                        return Err(CurlError::Transport(
                            "curl: '@path' body syntax is not supported. Read a file with 'cat <path>' and pipe it into curl --data-binary -."
                                .into(),
                        ));
                    }
                    bodies.push(val);
                }
            }
            "--data-urlencode" => {
                if let Some(val) = value(&mut i) {
                    if val.starts_with('@') || val.contains('@') && !val.contains('=') {
                        return Err(CurlError::Transport(
                            "curl: '--data-urlencode' file forms ('@file', 'name@file') are not supported. Pass 'name=value' instead."
                                .into(),
                        ));
                    }
                    bodies.push(urlencode_pair(&val));
                }
            }
            "-H" | "--header" => {
                if let Some(hv) = value(&mut i) {
                    if let Some((name, v)) = hv.split_once(':') {
                        let name = name.trim();
                        if name.eq_ignore_ascii_case("user-agent") {
                            user_agent = Some(v.trim().to_string());
                        } else {
                            headers.push((name.to_string(), v.trim().to_string()));
                        }
                    }
                }
            }
            "-o" | "--output" => output_file = value(&mut i),
            "-X" | "--request" => method_override = value(&mut i),
            "--url" => url_flag = value(&mut i),
            "-u" | "--user" => {
                if let Some(auth) = value(&mut i) {
                    match auth.split_once(':') {
                        Some((u, p)) => {
                            user = Some(u.to_string());
                            password = Some(p.to_string());
                        }
                        None => user = Some(auth),
                    }
                }
            }
            "-A" | "--user-agent" => user_agent = value(&mut i),
            "-e" | "--referer" => referer = value(&mut i),
            "-L" | "--location" => follow = true,
            "--max-redirs" => {
                if let Some(n) = value(&mut i) {
                    max_redirs = Some(n.parse::<u32>().map_err(|_| CurlError::Transport(
                        format!("curl: '--max-redirs' wants a whole number of redirects, got '{n}'."),
                    ))?);
                }
            }
            "--max-time" => max_time = Some(seconds(&value(&mut i), "--max-time")?),
            "--connect-timeout" => {
                connect_timeout = Some(seconds(&value(&mut i), "--connect-timeout")?)
            }
            _ => {}
        }
        i += 1;
    }

    let url = url_flag
        .or_else(|| find_positional(&trimmed))
        .ok_or_else(|| CurlError::MalformedUrl {
            url: "<missing>".into(),
            reason: "URL is required — one positional argument or --url must specify the target"
                .into(),
        })?;

    if !url.starts_with("http://") && !url.starts_with("https://") {
        return Err(CurlError::MalformedUrl {
            url: url.clone(),
            reason: "only http and https schemes are supported".into(),
        });
    }

    if head_only && !bodies.is_empty() {
        return Err(CurlError::MalformedUrl {
            url: url.clone(),
            reason: "--head discards the response body; providing --data is meaningless".into(),
        });
    }

    if let Some(r) = referer {
        headers.push(("Referer".to_string(), r));
    }
    // docs/curl.md's `-d` row: a body implies form encoding unless the caller
    // named a Content-Type themselves.
    if !bodies.is_empty()
        && !headers.iter().any(|(k, _)| k.eq_ignore_ascii_case("content-type"))
    {
        headers.push((
            "Content-Type".to_string(),
            "application/x-www-form-urlencoded".to_string(),
        ));
    }
    headers.push((
        "User-Agent".to_string(),
        user_agent.unwrap_or_else(|| DEFAULT_USER_AGENT.to_string()),
    ));

    let method = match method_override {
        Some(m) => m,
        None if head_only => "HEAD".to_string(),
        None if !bodies.is_empty() => "POST".to_string(),
        None => "GET".to_string(),
    };

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
        follow_redirects: follow,
        max_redirects: max_redirs,
        fail_on_error,
        max_time: max_time.unwrap_or_else(|| config.limits().max_time),
        connect_timeout,
    })
}

/// Parse a seconds value, naming the flag when it isn't a number.
fn seconds(raw: &Option<String>, flag: &str) -> Result<f64, CurlError> {
    let Some(raw) = raw else {
        return Err(CurlError::Transport(format!(
            "curl: '{flag}' wants a number of seconds, but none followed it."
        )));
    };
    raw.parse::<f64>()
        .ok()
        .filter(|s| *s > 0.0 && s.is_finite())
        .ok_or_else(|| {
            CurlError::Transport(format!(
                "curl: '{flag}' wants a positive number of seconds, got '{raw}'."
            ))
        })
}

/// Percent-encode the value half of a `name=value` pair, leaving the name and
/// the `=` alone (docs/curl.md's `--data-urlencode` row). A bare value with no
/// `=` is encoded whole, as curl does.
fn urlencode_pair(raw: &str) -> String {
    match raw.split_once('=') {
        Some((name, value)) => format!("{name}={}", percent_encode(value)),
        None => percent_encode(raw),
    }
}

/// Percent-encode everything outside RFC 3986's unreserved set.
fn percent_encode(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(b as char)
            }
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

fn has_flag(trimmed: &[String], names: &[&str]) -> bool {
    trimmed.iter().any(|t| names.contains(&t.as_str()))
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
        // Refused because the backend does not implement them, not because
        // they are out of scope. Accepting either would do the opposite of
        // what the caller asked, quietly. Both are tracked in docs/issues.md.
        ("-k", "curl: '-k' is not supported yet — this build always verifies TLS. It refuses rather than accept the flag and verify anyway."),
        ("--insecure", "curl: '--insecure' is not supported yet — this build always verifies TLS. It refuses rather than accept the flag and verify anyway."),
        ("--unix-socket", "curl: '--unix-socket' is not supported yet — this build has no AF_UNIX transport. It refuses rather than silently connect over TCP instead."),
    ];
    for (flag, msg) in REFUSED {
        if trimmed.iter().any(|t| t == *flag) {
            return Some(msg.to_string());
        }
    }
    None
}

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

/// The first token that is not a flag and not a flag's value — the URL.
fn find_positional(args: &[String]) -> Option<String> {
    let mut i = 0;
    while i < args.len() {
        if args[i].starts_with("--") || (args[i].starts_with('-') && args[i].len() > 1) {
            // An unknown flag might take a value; a known valueless one never
            // does. Getting this wrong in the other direction is what made
            // `curl -L <url>` report a missing URL.
            let takes_value = VALUE_FLAGS.contains(&args[i].as_str())
                || (!KNOWN_FLAGS.contains(&args[i].as_str()) && !args[i].contains('='));
            if takes_value && i + 1 < args.len() && !args[i + 1].starts_with('-') {
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

    // ── One parser, and it has to actually honor the documented surface ──
    //
    // Every case below covers a flag docs/curl.md's table promises and the
    // live path silently ignored while `tool.rs` carried its own inline
    // copy of this module. They are the reason the duplicate is gone.

    fn argv(tokens: &[&str]) -> ToolArgs {
        let mut a = ToolArgs::new();
        a.positional = tokens.iter().map(|t| Value::String((*t).to_string())).collect();
        a
    }

    fn parse(tokens: &[&str]) -> Request {
        parse_args(&argv(tokens), &CurlConfig::default()).expect("parse should succeed")
    }

    fn header<'a>(req: &'a Request, name: &str) -> Option<&'a str> {
        req.headers
            .iter()
            .find(|(k, _)| k.eq_ignore_ascii_case(name))
            .map(|(_, v)| v.as_str())
    }

    #[test]
    fn head_flag_selects_the_head_method() {
        let req = parse(&["-I", "http://x.com"]);
        assert_eq!(req.method, "HEAD");
        assert!(req.head_only);
    }

    #[test]
    fn user_agent_flag_sets_the_header() {
        let req = parse(&["-A", "agent/1.0", "http://x.com"]);
        assert_eq!(header(&req, "user-agent"), Some("agent/1.0"));
        // and it must not also carry the default
        assert_eq!(req.headers.iter().filter(|(k, _)| k.eq_ignore_ascii_case("user-agent")).count(), 1);
    }

    #[test]
    fn referer_flag_sets_the_header() {
        let req = parse(&["-e", "http://ref.example", "http://x.com"]);
        assert_eq!(header(&req, "referer"), Some("http://ref.example"));
    }

    #[test]
    fn max_redirs_is_carried_whether_or_not_dash_l_is_given() {
        let req = parse(&["-L", "--max-redirs", "3", "http://x.com"]);
        assert!(req.follow_redirects);
        assert_eq!(req.max_redirects, Some(3));

        // Without `-L` the cap is inert under the default policy, but it must
        // still reach the backend: under `RedirectPolicy::Auto` the embedder
        // follows, and the caller's cap is the one that should apply.
        let req = parse(&["--max-redirs", "3", "http://x.com"]);
        assert!(!req.follow_redirects);
        assert_eq!(req.max_redirects, Some(3));
    }

    #[test]
    fn location_flag_does_not_swallow_the_url() {
        // `-L` takes no value. A parser that treats it as value-taking eats
        // the URL and reports "URL is required".
        let req = parse(&["-L", "http://x.com"]);
        assert_eq!(req.url, "http://x.com");
        assert!(req.follow_redirects);
    }

    #[test]
    fn default_user_agent_when_caller_sets_none() {
        let req = parse(&["http://x.com"]);
        assert_eq!(header(&req, "user-agent"), Some("kaish-curl"));
    }

    #[test]
    fn explicit_header_user_agent_is_not_duplicated() {
        let req = parse(&["-H", "User-Agent: mine", "http://x.com"]);
        assert_eq!(header(&req, "user-agent"), Some("mine"));
        assert_eq!(req.headers.iter().filter(|(k, _)| k.eq_ignore_ascii_case("user-agent")).count(), 1);
    }

    #[test]
    fn data_urlencode_encodes_only_the_value() {
        let req = parse(&["--data-urlencode", "q=a b&c", "http://x.com"]);
        assert_eq!(req.bodies, vec!["q=a%20b%26c".to_string()]);
        assert_eq!(req.method, "POST");
    }

    #[test]
    fn a_body_implies_form_encoding_unless_the_caller_said_otherwise() {
        let req = parse(&["-d", "a=1", "http://x.com"]);
        assert_eq!(header(&req, "content-type"), Some("application/x-www-form-urlencoded"));

        let req = parse(&["-d", "{}", "-H", "Content-Type: application/json", "http://x.com"]);
        assert_eq!(header(&req, "content-type"), Some("application/json"));
        assert_eq!(req.headers.iter().filter(|(k, _)| k.eq_ignore_ascii_case("content-type")).count(), 1);
    }

    #[test]
    fn at_file_body_is_refused_not_silently_dropped() {
        let err = parse_args(&argv(&["-d", "@/etc/passwd", "http://x.com"]), &CurlConfig::default())
            .expect_err("@file bodies are deferred and must refuse");
        assert!(format!("{err}").contains("@path"), "message should name the syntax: {err}");
    }

    #[test]
    fn head_with_data_is_refused() {
        let err = parse_args(&argv(&["-I", "-d", "x=1", "http://x.com"]), &CurlConfig::default())
            .expect_err("--head discards the body; --data is meaningless");
        assert!(format!("{err}").contains("--head"), "message should name --head: {err}");
    }

    #[test]
    fn max_time_defaults_from_config_and_the_flag_overrides_it() {
        assert_eq!(parse(&["http://x.com"]).max_time, CurlConfig::default().limits().max_time);
        assert_eq!(parse(&["--max-time", "2.5", "http://x.com"]).max_time, 2.5);
    }

    #[test]
    fn unimplemented_flags_refuse_rather_than_lie() {
        // The backend honors neither, so accepting them would silently do
        // the opposite of what the caller asked (verify TLS, use TCP).
        for flag in ["-k", "--insecure", "--unix-socket"] {
            let err = parse_args(&argv(&[flag, "http://x.com"]), &CurlConfig::default())
                .expect_err("must refuse a flag the backend does not honor");
            assert!(format!("{err}").contains(flag), "message should name {flag}: {err}");
        }
    }
}
