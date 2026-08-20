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
            "curl: could not write '{}': {err}",
            vfs_path.display()
        )));
    }

    let mut result = ExecResult::success(format!("Wrote {written} bytes to '{}'", vfs_path.display()));
    result.baggage.insert("curl.file".to_string(), vfs_path.display().to_string());
    result.baggage.insert("curl.url".to_string(), url);
    result.baggage.insert("curl.status".to_string(), status.to_string());
    result
}
