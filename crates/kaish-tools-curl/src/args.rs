//! Argument parsing and refused-flag detection for kaish-curl.
//!
//! [`parse_args`] is the only parser in this crate, and it makes **one pass**.
//! That is load-bearing, not tidiness. The previous version swept the whole
//! argv for boolean flags before scanning it, so `curl -d "-i" <url>` set
//! `--include` from a token that was a *body*, and `curl -d "-O" <url>`
//! refused a flag nobody typed. A single pass never sees a flag's value as a
//! flag, because the arm that owns the flag consumes it.
//!
//! The tool declares [`ToolSchema::with_raw_argv`], so kaish binds every
//! argument to `positional` in source order rather than splitting it into an
//! unordered flag set. curl is exactly the position-sensitive case that
//! setting exists for: `-d -5` is a body, not a flag, and `-d a -d b` means
//! nothing without its order. The one shape the raw-argv binder still
//! composes is `--key=value`, which [`split_inline_value`] handles.
//!
//! A flag the backend cannot honor is refused here, never accepted and
//! dropped: accepting `-k` and then verifying the certificate anyway does the
//! opposite of what the caller asked, and says nothing about it.

use kaish_types::ToolArgs;
use kaish_types::Value;

use crate::config::CurlConfig;
use crate::error::CurlError;

/// Every flag the scan loop honors, in the long spelling the schema uses.
///
/// `tool.rs` builds the `ToolSchema` from a clap struct, and a test asserts
/// the two agree in both directions. They drifted badly before that guard
/// existed: the schema advertised `-k` and `--unix-socket`, which are
/// refused, and omitted ten flags that work — and `help curl` is most of what
/// an embedded agent ever learns about this tool.
/// Test-only: the parser's own list lives in the `match` arms below, and Rust
/// gives no way to reflect over them. This mirror covers the direction a
/// behavioral test cannot — "the parser honors a flag the schema never
/// mentions". The other direction is checked by actually running the parser.
#[cfg(test)]
pub(crate) const HONORED_FLAGS: &[&str] = &[
    "--url",
    "--request",
    "--data",
    "--data-binary",
    "--data-raw",
    "--data-urlencode",
    "--header",
    "--include",
    "--head",
    "--output",
    "--location",
    "--max-redirs",
    "--user",
    "--user-agent",
    "--referer",
    "--fail",
    "--max-time",
    "--connect-timeout",
];

/// Flags refused at parse time, each with the message an agent reads.
///
/// Two kinds live here: out of the 80/20 cut (`-O`, `--form`, …), and not
/// implemented by the backend (`-k`, `--unix-socket`). Both refuse, because
/// the alternative is a flag that quietly does nothing.
pub(crate) const REFUSED_FLAGS: &[(&str, &str)] = &[
    ("-O", "'-O' is not supported. Use '-o <file>' for an explicit output path."),
    ("-s", "'-s' is not supported. This build has no progress meter to suppress."),
    ("-S", "'-S' is not supported. Error display is always enabled."),
    ("--compressed", "'--compressed' is not supported. Decompression is automatic."),
    ("--form", "'--form' (multipart) is not supported. Use '--data' for application/x-www-form-urlencoded."),
    ("-F", "'-F' (multipart) is not supported. Use '--data' for application/x-www-form-urlencoded."),
    ("--cookie", "'--cookie' is not supported. Send cookies with '-H Cookie:<value>'."),
    ("-b", "'-b' is not supported. Send cookies with '-H Cookie:<value>'."),
    ("--verbose", "'--verbose' is not supported. Use '-i' to see response headers; request tracing is not available in this build."),
    ("-v", "'-v' is not supported. Use '-i' to see response headers; request tracing is not available in this build."),
    ("--write-out", "'--write-out' is not supported. Use kaish '--json' for a structured response object."),
    ("-w", "'-w' is not supported. Use kaish '--json' for a structured response object."),
    ("--retry", "'--retry' is not supported. Retry from the shell: 'for i in 1 2 3; do curl ... && break; done'."),
    ("--get", "'--get' is not supported. Put the query string in the URL."),
    ("-G", "'-G' is not supported. Put the query string in the URL."),
    ("--cert", "'--cert' is not supported. Client certificates are not available."),
    ("--key", "'--key' is not supported. Client key files are not available."),
    ("--proxy", "'--proxy' is not supported. Direct connections only."),
    ("-x", "'-x' is not supported. Direct connections only."),
    ("--config", "'--config' is not supported. Pass flags on the command line."),
    ("-K", "'-K' is not supported. Pass flags on the command line."),
    ("--netrc", "'--netrc' is not supported. Use '--user <user[:pass]>' for credentials."),
    ("--resolve", "'--resolve' is not supported. To reach a specific address with a different Host, request the IP and set the header: curl -H 'Host: example.com' http://<ip>/."),
    ("-k", "'-k' is not supported. This build always verifies TLS — use an https:// URL whose certificate validates, or an http:// URL."),
    ("--insecure", "'--insecure' is not supported. This build always verifies TLS — use an https:// URL whose certificate validates, or an http:// URL."),
    ("--unix-socket", "'--unix-socket' is not supported. This build has no AF_UNIX transport; reach the service over http:// instead."),
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
    /// hang the embedder by leaving it off. The backend clamps it to the
    /// embedder's ceiling.
    pub max_time: f64,
    /// Connect-phase deadline in seconds, when the caller asked for one.
    pub connect_timeout: Option<f64>,
}

/// Split a `--flag=value` token, which is the one composed shape the kaish
/// raw-argv binder still produces (`Arg::Named` renders as `--key=value`).
///
/// Short flags are left alone: `-H` never arrives glued to its value from the
/// binder, and splitting `-d a=1` on `=` would tear a body in half.
fn split_inline_value(token: &str) -> (&str, Option<&str>) {
    match token.strip_prefix("--").and_then(|rest| rest.split_once('=')) {
        Some((name, value)) => (&token[..name.len() + 2], Some(value)),
        None => (token, None),
    }
}

/// Parse curl-style [`ToolArgs`] into a validated [`Request`].
pub fn parse_args(args: &ToolArgs, config: &CurlConfig) -> Result<Request, CurlError> {
    let argv = trim_argv(args)?;

    let mut bodies: Vec<String> = Vec::new();
    let mut headers: Vec<(String, String)> = Vec::new();
    let mut output_file: Option<String> = None;
    let mut user: Option<String> = None;
    let mut password: Option<String> = None;
    let mut method_override: Option<String> = None;
    let mut url: Option<String> = None;
    let mut max_redirects: Option<u32> = None;
    let mut referer: Option<String> = None;
    let mut user_agent: Option<String> = None;
    let mut max_time: Option<f64> = None;
    let mut connect_timeout: Option<f64> = None;
    let mut include_headers = false;
    let mut head_only = false;
    let mut fail_on_error = false;
    let mut follow_redirects = false;

    // Only `-d`/`--data`/`--data-urlencode` imply a form Content-Type; real
    // curl sends none for `--data-binary`/`--data-raw`, so declaring
    // `--data-binary '{"a":1}'` as form-urlencoded was a lie about the bytes
    // on the wire (CU40).
    let mut body_implies_form = false;

    let mut i = 0;
    while i < argv.len() {
        let (flag, inline) = split_inline_value(&argv[i]);
        let argv_flag = flag.to_string();
        let flag = argv_flag.clone();
        let inline = inline.map(str::to_string);

        if let Some((_, message)) = REFUSED_FLAGS.iter().find(|(f, _)| *f == flag) {
            return Err(CurlError::Transport((*message).to_string()));
        }

        // Consume this flag's value: the inline half of `--flag=value`, or the
        // next token — whatever it looks like. A value that begins with `-`
        // belongs to its flag (`-d -5` is a body), which is why this never
        // inspects the next token before taking it.
        let value = |i: &mut usize| -> Result<String, CurlError> {
            if let Some(v) = inline.clone() {
                return Ok(v);
            }
            *i += 1;
            argv.get(*i).cloned().ok_or_else(|| {
                CurlError::Transport(format!("'{flag}' requires a value, and none followed it."))
            })
        };

        match argv[i].as_str().split('=').next().unwrap_or(&argv[i]) {
            "-d" | "--data" | "--data-binary" | "--data-raw" => {
                let raw = argv_flag == "--data-raw";
                let val = value(&mut i)?;
                // `--data-raw` takes `@` literally; the others would read a
                // file, which is deferred (CU5).
                if !raw && val.starts_with('@') {
                    return Err(CurlError::Transport(
                        "'@path' body syntax is not supported. Read a file with 'cat <path>' and pipe it into curl --data-binary -."
                            .into(),
                    ));
                }
                if matches!(argv_flag.as_str(), "-d" | "--data") {
                    body_implies_form = true;
                }
                bodies.push(val);
            }
            "--data-urlencode" => {
                let val = value(&mut i)?;
                if val.starts_with('@') || (val.contains('@') && !val.contains('=')) {
                    return Err(CurlError::Transport(
                        "'--data-urlencode' file forms ('@file', 'name@file') are not supported. Pass 'name=value' instead."
                            .into(),
                    ));
                }
                body_implies_form = true;
                bodies.push(urlencode_pair(&val));
            }
            "-H" | "--header" => {
                let hv = value(&mut i)?;
                let Some((name, v)) = hv.split_once(':') else {
                    return Err(CurlError::Transport(format!(
                        "'-H {hv}' is not a header. Headers are 'Name: value'."
                    )));
                };
                let name = name.trim();
                if name.eq_ignore_ascii_case("user-agent") {
                    user_agent = Some(v.trim().to_string());
                } else {
                    headers.push((name.to_string(), v.trim().to_string()));
                }
            }
            "-o" | "--output" => output_file = Some(value(&mut i)?),
            "-X" | "--request" => method_override = Some(value(&mut i)?),
            "--url" => url = Some(set_once(url, value(&mut i)?)?),
            "-u" | "--user" => {
                let auth = value(&mut i)?;
                match auth.split_once(':') {
                    Some((u, p)) => {
                        user = Some(u.to_string());
                        password = Some(p.to_string());
                    }
                    None => user = Some(auth),
                }
            }
            "-A" | "--user-agent" => user_agent = Some(value(&mut i)?),
            "-e" | "--referer" => referer = Some(value(&mut i)?),
            "-i" | "--include" => include_headers = true,
            "-I" | "--head" => head_only = true,
            "-f" | "--fail" => fail_on_error = true,
            "-L" | "--location" => follow_redirects = true,
            "--max-redirs" => {
                let raw = value(&mut i)?;
                max_redirects = Some(raw.parse::<u32>().map_err(|_| {
                    CurlError::Transport(format!(
                        "'--max-redirs' wants a whole number of redirects, got '{raw}'."
                    ))
                })?);
            }
            "--max-time" => max_time = Some(seconds(&value(&mut i)?, "--max-time")?),
            "--connect-timeout" => {
                connect_timeout = Some(seconds(&value(&mut i)?, "--connect-timeout")?)
            }
            // kaish's global output flag, not curl's request-body `--json`
            // (CU16). The kernel has already applied it via
            // `GlobalFlags::apply_from_args`, which handles exactly this
            // raw-argv case — it reaches us only because a raw_argv binder
            // lifts nothing out of source order. Consume and move on;
            // refusing it as unknown would break `curl --json <url> | jq`.
            "--json" => {}
            // End of options: everything after is an operand.
            "--" => {
                for token in &argv[i + 1..] {
                    url = Some(set_once(url, token.clone())?);
                }
                break;
            }
            other if other.starts_with('-') && other.len() > 1 => {
                // Silently ignoring an unknown flag is the failure mode this
                // whole surface exists to avoid: the agent believes it asked
                // for something.
                return Err(CurlError::Transport(format!(
                    "'{other}' is not a flag this build understands. Run 'help curl' for the supported set."
                )));
            }
            _ => url = Some(set_once(url, argv[i].clone())?),
        }
        i += 1;
    }

    let url = url.ok_or_else(|| CurlError::MalformedUrl {
        url: "<missing>".into(),
        reason: "URL is required — one positional argument or --url must specify the target".into(),
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
    if body_implies_form
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
        follow_redirects,
        max_redirects,
        fail_on_error,
        max_time: max_time.unwrap_or_else(|| config.limits().max_time),
        connect_timeout,
    })
}

/// One URL per invocation, and say so rather than fetching the first and
/// ignoring the rest.
fn set_once(existing: Option<String>, candidate: String) -> Result<String, CurlError> {
    match existing {
        Some(first) => Err(CurlError::Transport(format!(
            "only one URL per invocation is supported, and both '{first}' and '{candidate}' were given. Run curl once per URL."
        ))),
        None => Ok(candidate),
    }
}

/// Parse a seconds value, naming the flag when it isn't a number.
fn seconds(raw: &str, flag: &str) -> Result<f64, CurlError> {
    raw.parse::<f64>()
        .ok()
        .filter(|s| *s > 0.0 && s.is_finite())
        .ok_or_else(|| {
            CurlError::Transport(format!(
                "'{flag}' wants a positive number of seconds, got '{raw}'."
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

/// The argv, in source order.
///
/// The tool sets [`ToolSchema::with_raw_argv`], so kaish binds every argument
/// to `positional` in the order it was typed and leaves `flags`/`named`
/// empty. Reading only `positional` is therefore reading the true argv — and
/// the earlier version, which concatenated the three buckets, put a flag's
/// value before its flag whenever the binder split them.
fn trim_argv(args: &ToolArgs) -> Result<Vec<String>, CurlError> {
    args.positional
        .iter()
        .map(|v| match v {
            Value::String(s) => Ok(s.clone()),
            Value::Int(n) => Ok(n.to_string()),
            Value::Float(f) => Ok(f.to_string()),
            Value::Bool(b) => Ok(b.to_string()),
            // A structured or binary value has no argv spelling. Refusing
            // beats picking one and pretending the caller meant it.
            other => Err(CurlError::Transport(format!(
                "an argument of this kind cannot be part of a command line: {other:?}. Pass a string."
            ))),
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

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
    fn kaish_global_json_is_not_mistaken_for_an_unknown_flag() {
        // The kernel applies `--json` before execute (GlobalFlags::
        // apply_from_args), but a raw_argv binder leaves it in argv — so the
        // parser sees it and must not refuse it. `curl --json <url> | jq` is
        // the shape this whole surface is for.
        let req = parse(&["--json", "http://x.com"]);
        assert_eq!(req.url, "http://x.com");
    }

    #[test]
    fn a_double_dash_ends_the_flags() {
        let req = parse(&["--", "http://x.com"]);
        assert_eq!(req.url, "http://x.com");
    }

    #[test]
    fn an_unknown_flag_is_refused_rather_than_ignored() {
        let err = parse_args(&argv(&["--frobnicate", "http://x.com"]), &CurlConfig::default())
            .expect_err("an unknown flag must not be silently dropped");
        assert!(format!("{err}").contains("--frobnicate"), "{err}");
    }

    #[test]
    fn a_second_url_is_refused_rather_than_dropped() {
        let err = parse_args(&argv(&["http://a.com", "http://b.com"]), &CurlConfig::default())
            .expect_err("one URL per invocation");
        assert!(format!("{err}").contains("http://b.com"), "{err}");
    }

    #[test]
    fn a_missing_flag_value_fails_rather_than_no_op() {
        let err = parse_args(&argv(&["http://x.com", "-o"]), &CurlConfig::default())
            .expect_err("-o with nothing after it must not behave as though absent");
        assert!(format!("{err}").contains("-o"), "{err}");
    }

    #[test]
    fn a_body_that_looks_like_a_flag_stays_a_body() {
        // The old pre-scan swept all of argv for booleans, so `-d "-i"` set
        // --include and `-d "-O"` refused a flag nobody typed.
        let req = parse(&["-d", "-i", "http://x.com"]);
        assert_eq!(req.bodies, vec!["-i".to_string()]);
        assert!(!req.include_headers, "a body must not set a flag");

        let req = parse(&["-d", "-O", "http://x.com"]);
        assert_eq!(req.bodies, vec!["-O".to_string()]);

        // and a value beginning with `-` no longer desyncs the URL search
        let req = parse(&["-d", "-5", "http://x.com"]);
        assert_eq!(req.url, "http://x.com");
    }

    #[test]
    fn inline_flag_values_are_understood() {
        // kaish's raw-argv binder renders a `Arg::Named` as `--key=value`,
        // which the exact-match arms used to drop on the floor.
        let req = parse(&["--header=X-Foo: bar", "--max-redirs=2", "--url=http://x.com"]);
        assert_eq!(header(&req, "x-foo"), Some("bar"));
        assert_eq!(req.max_redirects, Some(2));
        assert_eq!(req.url, "http://x.com");
    }

    #[test]
    fn only_the_form_body_flags_imply_a_form_content_type() {
        // Real curl sends no Content-Type for --data-binary/--data-raw.
        assert_eq!(
            header(&parse(&["--data-binary", "{\"a\":1}", "http://x.com"]), "content-type"),
            None
        );
        assert_eq!(
            header(&parse(&["--data-raw", "x", "http://x.com"]), "content-type"),
            None
        );
        assert_eq!(
            header(&parse(&["-d", "a=1", "http://x.com"]), "content-type"),
            Some("application/x-www-form-urlencoded")
        );
    }

    #[test]
    fn a_refusal_names_the_tool_exactly_once() {
        // `CurlError::Transport`'s Display already prefixes "curl: ", and
        // every message here used to carry its own — so every refusal an
        // agent read began "curl: curl:".
        for (flag, message) in REFUSED_FLAGS {
            let rendered = CurlError::Transport((*message).to_string()).to_string();
            assert!(rendered.starts_with("curl: "), "{flag}: {rendered}");
            assert!(!rendered.contains("curl: curl:"), "{flag}: {rendered}");
            assert!(
                rendered.contains(flag),
                "a refusal must name the flag it refused: {rendered}"
            );
        }
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
