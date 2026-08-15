//! The kaish [`Tool`] implementation for curl.
//!
//! Wires together `CurlConfig`, argument parsing (`args`), the ureq backend,
//! and renderers. Constructed by [`tool()`] with an embedder's config.

use async_trait::async_trait;

use kaish_tool_api::{schema_tree_from_clap, Tool, ToolCtx};
use kaish_types::{ExecResult, OutputData, ToolArgs, ToolSchema, Value};

use crate::config::CurlConfig;
use crate::error::CurlError;

/// Registered `curl` tool for kaish.
#[derive(Debug)]
pub struct CurlTool {
    config: CurlConfig,
}

/// Build a curl tool from an embedder's config.
pub fn tool(config: CurlConfig) -> CurlTool {
    CurlTool { config }
}

impl CurlTool {
    /// Return the curl command's clap schema (flattened into every builtin).
    fn schema(&self) -> ToolSchema {
        #[derive(clap::Parser)]
        #[command(name = "curl", about = "Make HTTP requests")]
        struct CurlCli {
            /// URL to fetch (required).
            #[arg(required = true)]
            url: String,

            /// HTTP method (GET, POST, PUT, DELETE, HEAD, PATCH, OPTIONS).
            #[arg(short = 'X', long)]
            request: Option<String>,

            /// Request body data (repeatable for multiple parts, joined with &).
            #[arg(short = 'd', long, action = clap::ArgAction::Append)]
            data: Vec<String>,

            /// Include response headers in output (-i) and write them to file (-o).
            #[arg(short = 'i', long)]
            include: bool,

            /// Follow redirects up to --max-redirs.
            #[arg(short = 'L', long)]
            location: bool,

            /// Maximum redirect count (default: 50).
            #[arg(long, default_value = "50")]
            max_redirs: u32,

            /// Write body to file instead of stdout.
            #[arg(short = 'o', long)]
            output: Option<String>,

            /// Basic authentication user[:pass].
            #[arg(short = 'u', long)]
            user: Option<String>,

            /// Fail silently on server errors (>=400).
            #[arg(short = 'f')]
            fail: bool,

            /// Allow insecure SSL connections (skip certificate verification).
            #[arg(short = 'k')]
            insecure: bool,

            /// Use Unix domain socket for connection.
            #[arg(long)]
            unix_socket: Option<String>,
        }

        let cmd = <CurlCli as clap::CommandFactory>::command();
        schema_tree_from_clap(&cmd, "curl", "Make HTTP requests", [])
    }
}

#[async_trait]
impl Tool for CurlTool {
    fn name(&self) -> &str {
        self.config.tool_name()
    }

    fn schema(&self) -> ToolSchema {
        self.schema()
    }

    /// Execute the curl tool with parsed arguments and context.
    async fn execute(
        &self,
        args: ToolArgs,
        _ctx: &mut dyn ToolCtx,
    ) -> ExecResult {
        let trimmed = trim_argv(&args);

        // Refused flag check.
        if let Some(refused) = find_refused_flag(&trimmed) {
            return failure(CurlError::Transport(refused));
        }

        // Parse flags.
        let include_headers = trimmed.contains(&"-i".to_string()) || trimmed.contains(&"--include".to_string());
        let fail_on_error = trimmed.contains(&"-f".to_string()) || trimmed.contains(&"--fail".to_string());
        let insecure = trimmed.contains(&"-k".to_string()) || trimmed.contains(&"--insecure".to_string());

        let output_file = extract_arg(&trimmed, "-o").or_else(|| extract_arg(&trimmed, "--output"));
        let follow_redirects = if trimmed.contains(&"-L".to_string()) || trimmed.contains(&"--location".to_string()) {
            Some(self.config.limits().max_redirects)
        } else {
            None
        };

        // URL.
        let url = find_positional(&trimmed).unwrap_or_default();
        let url = if url.is_empty() {
            extract_arg(&trimmed, "--url").unwrap_or_default()
        } else {
            url
        };

        if url.is_empty() {
            return failure(CurlError::MalformedUrl {
                url: "<missing>".into(),
                reason: "URL is required — one positional or --url must specify the target".into(),
            });
        }

        // Validate scheme.
        if !url.starts_with("http://") && !url.starts_with("https://") {
            return failure(CurlError::MalformedUrl {
                url: url.clone(),
                reason: "only http and https schemes are supported".into(),
            });
        }

        // Auth.
        let mut auth_user: Option<String> = None;
        let mut auth_pass: Option<String> = None;
        if let Some(auth) = extract_arg(&trimmed, "-u").or_else(|| extract_arg(&trimmed, "--user")) {
            if let Some((u, p)) = auth.split_once(':') {
                auth_user = Some(u.to_string());
                auth_pass = Some(p.to_string());
            } else {
                auth_user = Some(auth);
            }
        }

        // Bodies — refuse @file body forms (reading files via VFS is deferred).
        let mut bodies: Vec<String> = Vec::new();
        scan_bodies(&trimmed, &mut bodies);
        if bodies.is_empty() && has_body_flag(&trimmed) {
            return failure(CurlError::Transport(
                "curl: '@path' body syntax is not supported. Read a file with 'cat <path>' and pipe it into curl --data-binary -."
                    .into(),
            ));
        }

        // Headers.
        let mut headers: Vec<(String, String)> = Vec::new();
        let mut ua_explicit = false;
        scan_headers_with_ua_check(&trimmed, &mut headers, &mut ua_explicit);
        // Default User-Agent only when user didn't set one.
        if !ua_explicit {
            headers.push(("User-Agent".to_string(), "kaish-curl".to_string()));
        }

        // Method.
        let method = if let Some(m) = extract_arg(&trimmed, "-X").or_else(|| extract_arg(&trimmed, "--request")) {
            m
        } else if !bodies.is_empty() {
            "POST".to_string()
        } else {
            "GET".to_string()
        };

        let req = crate::args::Request {
            url,
            method,
            bodies,
            headers,
            include_headers,
            head_only: false,
            output_file,
            user: auth_user,
            password: auth_pass,
            insecure,
            follow_redirects,
            fail_on_error,
            unix_socket: extract_arg(&trimmed, "--unix-socket"),
            ua_explicit,
        };

        // Execute via blocking backend.
        let output_path = req.output_file.clone();
        let include_hdrs = req.include_headers;
        let result = crate::util::block_in_place_compat(move || {
            crate::backend::fetch(&req, &self.config)
        });

        match result {
            Ok(response) => handle_success(output_path.as_deref(), response, include_hdrs),
            Err(err) => error_result(err),
        }
    }
}

fn error_result(err: CurlError) -> ExecResult {
    let code = err.exit_code();
    let msg = format!("{err}");
    let mut r = ExecResult::success("");
    r.code = code;
    r.err = msg;
    r
}

fn failure(err: CurlError) -> ExecResult {
    let code = err.exit_code();
    let msg = format!("{err}");
    let mut r = ExecResult::success("");
    r.code = code;
    r.err = msg;
    r
}

fn handle_success(
    output_file: Option<&str>,
    response: crate::model::Response,
    include_headers: bool,
) -> ExecResult {
    if let Some(path) = output_file {
        let _bytes_written = path.len(); // placeholder
        let status = response.status;
        let mut result = ExecResult::success(format!(
            "Wrote {} bytes to '{}'",
            response.body.len(),
            path
        ));
        result.baggage.insert("curl.file".to_string(), path.to_string());
        result.baggage.insert("curl.url".to_string(), response.url.clone());
        result.baggage.insert("curl.status".to_string(), status.to_string());

        // Create directory if needed.
        if let Some(parent) = std::path::Path::new(path).parent() {
            if !parent.as_os_str().is_empty() {
                let _ = std::fs::create_dir_all(parent);
            }
        }

        // If -i, prepend headers to file content.
        let payload = if include_headers {
            let mut h = format!("HTTP/1.1 {}\r\n", response.status);
            for (name, value) in &response.headers {
                h.push_str(&format!("{name}: {value}\r\n"));
            }
            h.push_str("\r\n");
            let body_str = String::from_utf8_lossy(&response.body);
            format!("{h}{body_str}").into_bytes()
        } else {
            response.body
        };

        match std::fs::write(path, payload) {
            Ok(_) => result,
            Err(e) => {
                let mut r = ExecResult::success(format!("Write failed: {e}"));
                r.code = 1;
                r
            }
        }
    } else {
        // Stdout output.
        let text = crate::render::render_text(&response, include_headers, false);
        let json_obj = crate::render::render_json(&response);
        let status = response.status;

        let mut result = ExecResult::with_output(OutputData::text(text));
        result.baggage.insert("curl.url".to_string(), response.url);
        result.data = Some(Value::Json(json_obj));
        result.baggage.insert("curl.status".to_string(), status.to_string());
        result
    }
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
    "--unix-socket", "--json", "--compressed",
];

fn find_positional(args: &[String]) -> Option<String> {
    let mut i = 0;
    while i < args.len() {
        if args[i].starts_with("--") || (args[i].starts_with("-") && args[i].len() > 1) {
            let needs_value = {
                let t = args[i].as_str();
                // These flags consume the next token as their value
                let has_value_arg = matches!(t, "-d"|"-X"|"--data"|"--data-binary"|"--data-raw"
                    |"--data-urlencode"|"-A"|"-e"|"-H"|"-o"|"--header"|"--output"
                    |"--request"|"-u"|"--user"|"--max-time"|"--unix-socket");
                !KNOWN_FLAGS.contains(&args[i].as_str()) || args[i].contains('=') || has_value_arg
            };
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

fn extract_arg(args: &[String], flag: &str) -> Option<String> {
    for i in 0..args.len() {
        if args[i] == flag {
            if i + 1 < args.len() && !args[i + 1].starts_with('-') {
                return Some(args[i + 1].clone());
            }
        }
    }
    None
}

fn scan_bodies(trimmed: &[String], bodies: &mut Vec<String>) {
    let mut i = 0;
    while i < trimmed.len() {
        if trimmed[i] == "-d" || trimmed[i] == "--data" || trimmed[i] == "--data-binary" || trimmed[i] == "--data-raw" {
            i += 1;
            if let Some(val) = trimmed.get(i) {
                // @path body form is refused — read files through VFS (cat).
                if val.starts_with('@') {
                    return; // caller checks for this after scan returns
                }
                bodies.push(val.clone());
            }
        }
        i += 1;
    }
}

/// Scans for `-H`/`--header` flags and detects User-Agent presence.
fn scan_headers_with_ua_check(trimmed: &[String], headers: &mut Vec<(String, String)>, ua_explicit: &mut bool) {
    let mut i = 0;
    while i < trimmed.len() {
        if trimmed[i] == "-H" || trimmed[i] == "--header" {
            i += 1;
            if let Some(hv) = trimmed.get(i) {
                if let Some((name, value)) = hv.split_once(':') {
                    let hname = name.trim().to_lowercase();
                    if hname == "user-agent" {
                        *ua_explicit = true;
                    }
                    headers.push((name.trim().to_string(), value.trim().to_string()));
                }
            }
        }
        i += 1;
    }
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

/// Check if any body-generating flag was present in trimmed args.
fn has_body_flag(trimmed: &[String]) -> bool {
    trimmed.iter().any(|f| matches!(f.as_str(), "-d" | "--data" | "--data-binary" | "--data-raw"))
}
