//! The kaish [`Tool`] implementation for curl.
//!
//! Wires together `CurlConfig`, argument parsing (`args`), the ureq backend,
//! and renderers. Constructed by [`tool()`] with an embedder's config.

use async_trait::async_trait;

use kaish_tool_api::{schema_tree_from_clap, Tool, ToolCtx};
use kaish_types::{ExecResult, OutputData, ToolArgs, ToolSchema, Value, WriteMode};

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
        // This struct is what `help curl`, completion, and `tools --json`
        // describe — for an embedded agent it is very nearly everything the
        // tool ever says about itself. It drifted badly once: it advertised
        // `-k` and `--unix-socket`, which are refused, and omitted ten flags
        // that work. `schema_matches_the_parser` in this file now fails if it
        // drifts again.
        #[derive(clap::Parser)]
        #[command(name = "curl", about = DESCRIPTION)]
        struct CurlCli {
            /// The URL to fetch, positionally or as --url. Exactly one per
            /// invocation; http and https only.
            #[arg(required = true)]
            url: String,

            /// HTTP method. Defaults to GET, or POST when a body is given.
            #[arg(short = 'X', long)]
            request: Option<String>,

            /// Request body. Repeatable; parts join with '&'. Implies POST and
            /// Content-Type: application/x-www-form-urlencoded unless -H sets one.
            #[arg(short = 'd', long, action = clap::ArgAction::Append)]
            data: Vec<String>,

            /// Like --data, and never strips anything from the value.
            #[arg(long, action = clap::ArgAction::Append)]
            data_binary: Vec<String>,

            /// Like --data, and '@' is literal rather than a file read.
            #[arg(long, action = clap::ArgAction::Append)]
            data_raw: Vec<String>,

            /// Body part 'name=value' with only the value percent-encoded.
            #[arg(long, action = clap::ArgAction::Append)]
            data_urlencode: Vec<String>,

            /// Request header, 'Name: value'. Repeatable.
            #[arg(short = 'H', long, action = clap::ArgAction::Append)]
            header: Vec<String>,

            /// Print response headers above the body; with -o they go into the file.
            #[arg(short = 'i', long)]
            include: bool,

            /// HEAD request: print the response headers and no body.
            #[arg(short = 'I', long)]
            head: bool,

            /// Write the body to this path in the shell's filesystem instead of stdout.
            #[arg(short = 'o', long)]
            output: Option<String>,

            /// Follow redirects. Every hop is re-checked against the embedder's
            /// egress policy, and credentials are dropped on a change of host.
            #[arg(short = 'L', long)]
            location: bool,

            /// Cap on redirects followed. The embedder's ceiling still applies.
            #[arg(long)]
            max_redirs: Option<u32>,

            /// Basic auth, 'user' or 'user:password'.
            #[arg(short = 'u', long)]
            user: Option<String>,

            /// Set the User-Agent. Defaults to kaish-curl.
            #[arg(short = 'A', long)]
            user_agent: Option<String>,

            /// Set the Referer header.
            #[arg(short = 'e', long)]
            referer: Option<String>,

            /// Exit 22 on an HTTP status of 400 or more instead of printing the body.
            #[arg(short = 'f', long)]
            fail: bool,

            /// Whole-request timeout in seconds. May lower the embedder's
            /// ceiling, never raise it.
            #[arg(long)]
            max_time: Option<f64>,

            /// Connect-phase timeout in seconds.
            #[arg(long)]
            connect_timeout: Option<f64>,
        }

        let cmd = <CurlCli as clap::CommandFactory>::command();
        schema_tree_from_clap(&cmd, self.config.tool_name(), DESCRIPTION, EXAMPLES)
            // curl is position-sensitive: `-d -5` is a body and `-d a -d b`
            // means nothing without its order, so it needs the true argv
            // rather than kaish's unordered flag/named split. See args.rs.
            .with_raw_argv()
            // What this tool does, for an embedder gating on declared effects.
            // It used to declare nothing, so curl looked side-effect-free
            // while making network requests and writing files (CU12, CU29).
            .with_operations(["net.request", "fs.overwrite"])
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
        ctx: &mut dyn ToolCtx,
    ) -> ExecResult {
        // One parser: `args::parse_args`. This function used to carry a
        // second, inline copy that drifted out of step with it — see that
        // module's header.
        let req = match crate::args::parse_args(&args, &self.config) {
            Ok(req) => req,
            Err(err) => return failure(err),
        };

        let output_file = req.output_file.clone();
        let include_headers = req.include_headers;
        let head_only = req.head_only;

        let result = crate::util::block_in_place_compat(move || {
            crate::backend::fetch(&req, &self.config)
        });

        match result {
            Ok(response) => {
                handle_success(output_file.as_deref(), response, include_headers, head_only, ctx)
                    .await
            }
            Err(err) => error_result(err),
        }
    }
}

/// What `help curl` leads with. It says what the tool refuses as well as what
/// it does, because an agent that learns the boundary early stops guessing at
/// it — and for an embedded agent this line and the examples below are most
/// of what it will ever read about curl.
const DESCRIPTION: &str = "Make one HTTP request — GET or POST a URL, set headers, send a body, save the response. \
Reaches only hosts the embedder permits; flags this build cannot honor are refused rather than ignored";

/// Examples the schema carries into `help curl` and completion.
///
/// Chosen for the shapes an agent actually types, not for flag coverage: read
/// an API, post JSON, authenticate, save a file, branch on the result.
const EXAMPLES: [(&str, &str); 6] = [
    ("Fetch a URL", "curl https://api.example.com/status"),
    (
        "Post JSON and read the reply",
        "curl -H 'Content-Type: application/json' -d '{\"q\":\"hello\"}' https://api.example.com/search",
    ),
    ("Authenticate", "curl -u alice:secret https://api.example.com/private"),
    ("See the response headers too", "curl -i https://api.example.com/status"),
    ("Save the body to a file", "curl -o /tmp/page.html https://example.com/"),
    (
        "Branch on the response without re-parsing text",
        "curl --json https://api.example.com/status | jq -r .status",
    ),
];

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

/// Render the response to stdout, or write it to a VFS path for `-o`.
///
/// `-o` goes through `ToolCtx::resolve_path` + `backend().write()`, never
/// `std::fs`. Writing to the host filesystem would let `curl -o /etc/passwd`
/// out of the embedder's mount entirely — the containment CU9 requires for
/// paths, applied to the one flag that produces a path.
async fn handle_success(
    output_file: Option<&str>,
    response: crate::model::Response,
    include_headers: bool,
    head_only: bool,
    ctx: &mut dyn ToolCtx,
) -> ExecResult {
    let status = response.status;
    let url = response.url.clone();

    let Some(path) = output_file else {
        // curl prints headers for `-I` whether or not `-i` was given.
        let text = crate::render::render_text(&response, include_headers || head_only, head_only);
        let json_obj = crate::render::render_json(&response);

        let mut result = ExecResult::with_output(OutputData::text(text));
        result.data = Some(Value::Json(json_obj));
        result.baggage.insert("curl.url".to_string(), url);
        result.baggage.insert("curl.status".to_string(), status.to_string());
        return result;
    };

    // With `-i`, headers go into the file alongside the body — curl's
    // behavior, and what docs/curl.md's `-o` row promises.
    let payload = if include_headers {
        crate::render::render_text(&response, true, head_only).into_bytes()
    } else {
        response.body
    };
    let written = payload.len();

    let vfs_path = ctx.resolve_path(path);
    if let Err(err) = ctx
        .backend()
        .write(&vfs_path, &payload, WriteMode::Overwrite)
        .await
    {
        return error_result(CurlError::Transport(format!(
            "could not write '{}': {err}",
            vfs_path.display()
        )));
    }

    let mut result = ExecResult::success(format!("Wrote {written} bytes to '{}'", vfs_path.display()));
    result.baggage.insert("curl.file".to_string(), vfs_path.display().to_string());
    result.baggage.insert("curl.url".to_string(), url);
    result.baggage.insert("curl.status".to_string(), status.to_string());
    result
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::args::{HONORED_FLAGS, REFUSED_FLAGS};
    use kaish_types::Value;

    /// A minimal invocation exercising one flag, with a value after it in
    /// case it takes one — a spare token is harmless to a boolean flag.
    fn argv_for(flag: &str) -> ToolArgs {
        let mut args = ToolArgs::new();
        args.positional = [flag, "1", "http://example.com"]
            .iter()
            .map(|t| Value::String((*t).to_string()))
            .collect();
        args
    }

    /// `help curl` is nearly everything an embedded agent learns about this
    /// tool, and the schema and the parser are two hand-maintained lists that
    /// have already drifted once — the schema advertised `-k` and
    /// `--unix-socket`, which are refused, while omitting ten flags that
    /// work. This fails the moment they disagree again, in either direction.
    #[test]
    fn schema_matches_the_parser() {
        let schema = tool(CurlConfig::default()).schema();
        // clap renders an underscore field as a hyphenated flag; `url_flag`
        // is the schema's spelling of the parser's `--url`.
        let mut described: Vec<String> = schema
            .params
            .iter()
            .filter(|p| p.name != "url")
            .map(|p| format!("--{}", p.name))
            .collect();
        // `--url` is the positional's other spelling; clap cannot carry both
        // under one id, so the positional's own help text names it and the
        // guard credits it here rather than growing a `url-flag` parameter
        // that no agent should ever type.
        assert!(
            schema.params.iter().any(|p| p.name == "url" && p.description.contains("--url")),
            "the positional must tell an agent that --url is the same thing"
        );
        described.push("--url".to_string());

        // Forward direction, checked by running the parser rather than by
        // consulting a list: every flag `help curl` advertises must actually
        // parse. A flag the schema invents fails here with the parser's own
        // "not a flag this build understands".
        for flag in &described {
            let probe = argv_for(flag);
            let err = crate::args::parse_args(&probe, &CurlConfig::default())
                .err()
                .map(|e| e.to_string())
                .unwrap_or_default();
            assert!(
                !err.contains("is not a flag this build understands") && !err.contains("is not supported"),
                "the schema describes {flag}, which the parser does not honor — \
                 `help curl` would be advertising a flag that fails: {err}"
            );
            assert!(
                !REFUSED_FLAGS.iter().any(|(f, _)| f == flag),
                "the schema describes {flag}, which the parser refuses"
            );
        }

        for flag in HONORED_FLAGS {
            assert!(
                described.iter().any(|d| d == flag),
                "the parser honors {flag} and the schema never mentions it — \
                 an agent reading `help curl` would never know to type it"
            );
        }
    }

    /// An embedder gating on declared effects has to see what curl really
    /// does. It declared nothing at all until 2026-08-20, which made a tool
    /// that makes network requests and writes files look inert.
    #[test]
    fn schema_declares_what_the_tool_actually_does() {
        let schema = tool(CurlConfig::default()).schema();
        assert!(schema.operations.iter().any(|o| o == "net.request"));
        assert!(schema.operations.iter().any(|o| o == "fs.overwrite"));
    }

    /// curl is position-sensitive; without this kaish hoists a body that
    /// looks like a flag into an unordered flag set (`-d "-i"` setting
    /// `--include`) and renders named values as `--key=value`.
    #[test]
    fn schema_asks_for_the_true_argv() {
        assert!(tool(CurlConfig::default()).schema().raw_argv);
    }

    /// Examples are the part of `help` an agent copies. Empty is the state
    /// this schema shipped in.
    #[test]
    fn schema_carries_examples() {
        let schema = tool(CurlConfig::default()).schema();
        assert!(!schema.examples.is_empty());
        for ex in &schema.examples {
            assert!(ex.code.starts_with("curl "), "example should be runnable: {}", ex.code);
        }
    }
}
