//! The embedder-supplied profile config, mirroring
//! `kaish_tools_git::GitConfig`'s shape (see that crate's `config.rs`).
//!
//! No config file, deliberately, for the same reason git's config has none:
//! a config that decides what an agent may reach over the network must not
//! be reachable from inside the sandbox, and a file would add a lookup path,
//! a parse failure mode, and a tempting write target. This is a Rust struct
//! the embedder builds and hands to the tool constructor.
//!
//! Unlike `GitConfig`, there is no subtractive verb set here — curl is one
//! flat command, not a set of subcommands an embedder narrows — so the
//! surface is just a tool name and a pair of hard caps.

/// Per-invocation caps the embedder sets as a ceiling; a flag's own value
/// (`--max-redirs`) may only lower it, never raise it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Limits {
    /// Hard ceiling on redirects followed under `-L`. docs/curl.md's
    /// `--max-redirs` default (50) is exactly this value until an
    /// invocation asks for fewer.
    pub max_redirects: u32,
    /// Hard ceiling, in bytes, on a response body read into memory for a
    /// stdout render. docs/curl.md ties this to "the kernel's output cap"
    /// (`ureq`'s own 10 MB `read_to_string` default, lowered) — this crate
    /// does not depend on `kaish-kernel`, so it cannot read that cap
    /// directly, and the embedder is expected to set this field to match
    /// their own kernel's limit. `-o`/`-O` streams to the VFS instead of
    /// buffering and is governed by the embedder's VFS byte budget, not
    /// this field.
    pub max_response_bytes: u64,
}

impl Default for Limits {
    /// ureq's own default before docs/curl.md's guidance to lower it to the
    /// embedder's actual output cap, and curl's own `--max-redirs` default.
    fn default() -> Self {
        Self {
            max_redirects: 50,
            max_response_bytes: 10 * 1024 * 1024,
        }
    }
}

/// The embedder's decision about how this curl tool is named and bounded.
///
/// Built by [`CurlConfig::default`] (or [`CurlConfig::new`], the same
/// thing spelled as a constructor) and narrowed from there. Fields are
/// private so the only reachable transitions are through the builders.
#[derive(Debug, Clone)]
#[non_exhaustive]
pub struct CurlConfig {
    tool_name: String,
    limits: Limits,
}

impl CurlConfig {
    /// The default config: tool name `curl`, and the [`Limits`] defaults.
    ///
    /// Registering as `curl` deliberately shadows any external `curl` on
    /// PATH — kaish resolves builtins before PATH, which is the point for
    /// an agent surface. Use [`CurlConfig::with_tool_name`] if you want
    /// both to be reachable.
    pub fn new() -> Self {
        Self::default()
    }

    /// Register under a name other than `curl`.
    pub fn with_tool_name(mut self, name: impl Into<String>) -> Self {
        self.tool_name = name.into();
        self
    }

    /// Replace the output caps.
    pub fn with_limits(mut self, limits: Limits) -> Self {
        self.limits = limits;
        self
    }

    /// The name this tool registers under.
    pub fn tool_name(&self) -> &str {
        &self.tool_name
    }

    /// The output caps.
    pub fn limits(&self) -> Limits {
        self.limits
    }
}

impl Default for CurlConfig {
    fn default() -> Self {
        Self {
            tool_name: "curl".to_string(),
            limits: Limits::default(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_config_is_named_curl_with_the_documented_limits() {
        let cfg = CurlConfig::default();
        assert_eq!(cfg.tool_name(), "curl");
        assert_eq!(cfg.limits(), Limits::default());
    }

    #[test]
    fn new_is_the_same_as_default() {
        assert_eq!(CurlConfig::new().tool_name(), CurlConfig::default().tool_name());
    }

    /// docs/curl.md's numbers, spelled out. A silent drift in a cap is a
    /// silent change in how much an agent gets back.
    #[test]
    fn limits_defaults_match_the_design() {
        let l = Limits::default();
        assert_eq!(l.max_redirects, 50);
        assert_eq!(l.max_response_bytes, 10 * 1024 * 1024);
    }

    #[test]
    fn with_tool_name_overrides_the_registered_name() {
        let cfg = CurlConfig::default().with_tool_name("kcurl");
        assert_eq!(cfg.tool_name(), "kcurl");
    }

    #[test]
    fn with_limits_overrides_the_defaults() {
        let cfg = CurlConfig::default().with_limits(Limits {
            max_redirects: 0,
            max_response_bytes: 1024,
        });
        assert_eq!(cfg.limits().max_redirects, 0);
        assert_eq!(cfg.limits().max_response_bytes, 1024);
    }
}
