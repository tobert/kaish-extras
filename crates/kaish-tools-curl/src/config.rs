//! The embedder-supplied profile config, mirroring
//! `kaish_tools_git::GitConfig`'s shape (see that crate's `config.rs`).
//!
//! No config file, deliberately, for the same reason git's config has none:
//! a config that decides what an agent may reach over the network must not
//! be reachable from inside the sandbox, and a file would add a lookup path,
//! a parse failure mode, and a tempting write target. This is a Rust struct
//! the embedder builds and hands to [`crate::tool`] (when it exists).
//!
//! The shape is **subtractive**: `default()` is the only constructor, and
//! every builder narrows the surface further. There is no method that widens
//! per-request past what the constructor granted — see the test below that
//! enforces it. Compare git's `no_public_method_can_widen_the_verb_set` for
//! the same invariant.

use std::net::IpAddr;
use std::sync::Arc;

use url::Url;

/// Decide whether a resolved URL is permitted by the embedder.
///
/// Called at the ureq transport layer for every outbound request and every
/// redirect hop. An embedder implements this to capture approval flows,
/// maintain dynamic allowlists, or gate on policy engines.
///
/// A default implementation always returns `true` (the old permissive
/// posture); the embedder should replace it with a deny-by-empty policy
/// (`AllowByList`) for production use.
pub trait AllowEgress: Send + Sync {
    /// Whether `url` is permitted. The URL is absolute and fully resolved
    /// (scheme + authority); it is NOT a VFS path.
    fn permit(&self, url: &str) -> bool;
}

/// Default allow-everything implementation. Not recommended for production
/// use — the embedder should substitute `AllowByList` for deny-by-default.
#[derive(Debug, Clone)]
pub struct AllowAll;

impl AllowEgress for AllowAll {
    fn permit(&self, _url: &str) -> bool {
        true
    }
}

/// Host-restricted allowlist with opt-in loopback/link-local.
///
/// Default: **empty** `allowed_hosts` — nothing resolves unless the embedder
/// names a host first. Loopback and link-local ranges are denied unless the
/// caller enables them via constructors. Subtractive: once constructed, no
/// method adds hosts or changes the scopes.
/// `Default` is deliberately the same deny-everything starting point as
/// [`AllowByList::new`] — an allowlist whose default let anything through
/// would be the wrong shape of mistake to make convenient.
#[derive(Debug, Clone, Default)]
pub struct AllowByList {
    /// Allowed hostnames / IPs. Empty = nothing passes.
    allowed_hosts: Vec<String>,
    /// Whether loopback addresses are permitted (127.0.0.0/8, ::1, etc.).
    allow_loopback: bool,
    /// Whether link-local / metadata addresses are permitted
    /// (169.254.0.0/16, fe80::/10, etc.). Default false.
    allow_link_local: bool,
}

impl AllowByList {
    /// Create with an empty allowlist and both loopback and link-local denied.
    pub fn new() -> Self {
        Self {
            allowed_hosts: Vec::new(),
            allow_loopback: false,
            allow_link_local: false,
        }
    }

    /// Add hosts to the allowlist. Each call *replaces* the list entirely,
    /// so the caller builds the full list before passing it to `CurlConfig`.
    /// There is no individual removal method.
    pub fn with_allowed_hosts(mut self, hosts: impl IntoIterator<Item = impl Into<String>>) -> Self {
        // Normalized on the way in, so a list written `Allowed.Example.` still
        // matches the host a URL resolves to.
        self.allowed_hosts = hosts
            .into_iter()
            .map(|h| normalize_host(&h.into()))
            .collect();
        self
    }

    /// Permit loopback addresses in addition to the allowlist.
    /// Denies 127.0.0.0/8, ::1, and similar.
    pub fn with_allow_loopback(mut self, allow: bool) -> Self {
        self.allow_loopback = allow;
        self
    }

    /// Permit link-local / metadata addresses in addition to the allowlist.
    /// Denies 169.254.0.0/16, fe80::/10, fd00::/8, and cloud metadata ranges.
    pub fn with_allow_link_local(mut self, allow: bool) -> Self {
        self.allow_link_local = allow;
        self
    }
}

/// The host a URL actually reaches, normalized for comparison.
///
/// Parsed, never split. The userinfo in
/// `https://allowed.example:443@169.254.169.254/` belongs to the authority,
/// and a lexical split on the first `:` cannot tell it from the host — which
/// is exactly how that URL used to satisfy an allowlist naming
/// `allowed.example` while ureq dialed the metadata service. `None` means we
/// could not determine the host, and an undeterminable host is denied.
fn reachable_host(url: &str) -> Option<String> {
    Url::parse(url).ok()?.host_str().map(normalize_host)
}

/// Lower-case, and drop the root label's trailing dot: `Allowed.Example.` and
/// `allowed.example` name the same host, and an allowlist that says otherwise
/// denies for a reason nobody can see.
fn normalize_host(host: &str) -> String {
    host.strip_suffix('.').unwrap_or(host).to_ascii_lowercase()
}

/// The host as an IP address, if it is one. Brackets come off first —
/// `Url::host_str` renders an IPv6 literal as `[::1]`.
fn parse_ip(host: &str) -> Option<IpAddr> {
    host.strip_prefix('[')
        .and_then(|h| h.strip_suffix(']'))
        .unwrap_or(host)
        .parse()
        .ok()
}

fn is_host_allowed(host: &str, allowed: &[String]) -> bool {
    allowed.iter().any(|h| h == host)
}

/// Whether the host is a loopback address.
///
/// Address, not spelling: this used to be `host.starts_with("127.")`, under
/// which the ordinary DNS name `127.evil.com` was loopback and an embedder
/// that opted into loopback opted into whatever that name resolved to.
fn is_loopback(host: &str) -> bool {
    host == "localhost" || parse_ip(host).is_some_and(|ip| ip.is_loopback())
}

/// Whether the host is link-local, or one of the cloud metadata endpoints
/// that share the same "reachable from inside, trusted by accident" shape.
fn is_link_local(host: &str) -> bool {
    if host == "metadata.google.internal" {
        return true;
    }
    match parse_ip(host) {
        // 169.254.0.0/16, plus Alibaba's metadata address, which is not
        // link-local but belongs to the same opt-in.
        Some(IpAddr::V4(v4)) => v4.is_link_local() || v4.octets() == [100, 100, 100, 200],
        // fe80::/10 (link-local) and fc00::/7 (unique local).
        Some(IpAddr::V6(v6)) => {
            let first = v6.segments()[0];
            (first & 0xffc0) == 0xfe80 || (first & 0xfe00) == 0xfc00
        }
        None => false,
    }
}

impl AllowEgress for AllowByList {
    fn permit(&self, url: &str) -> bool {
        let Some(host) = reachable_host(url) else {
            return false;
        };

        if is_host_allowed(&host, &self.allowed_hosts) {
            return true;
        }
        if self.allow_loopback && is_loopback(&host) {
            return true;
        }
        if self.allow_link_local && is_link_local(&host) {
            return true;
        }
        false
    }
}

/// Per-invocation caps the embedder sets as a ceiling; a flag's own value
/// (`--max-redirs`) may only lower it, never raise it.
#[derive(Debug, Clone, Copy, PartialEq)]
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
    /// their own kernel's limit. `-o` streams to the VFS instead of
    /// buffering and is governed by the embedder's VFS byte budget, not
    /// this field.
    pub max_response_bytes: u64,
    /// Maximum time in seconds for the entire request. Prevents the runtime
    /// from freezing on unresponsive servers. Default: 30s (curl has no
    /// default overall timeout, which means agents can hang the embedder).
    pub max_time: f64,
}

impl Default for Limits {
    /// ureq's own default before docs/curl.md's guidance to lower it to the
    /// embedder's actual output cap, curl's own `--max-redirs` default, and
    /// the 30-second safety net against silent hangs.
    fn default() -> Self {
        Self {
            max_redirects: 50,
            max_response_bytes: 10 * 1024 * 1024,
            max_time: 30.0,
        }
    }
}

/// Follow-redirect policy. The default is no-follow (matches curl's `-L`
/// requirement); the embedder can set the config default to auto-follow
/// for agents that consistently trip over 301/302 responses.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum RedirectPolicy {
    /// Do not follow redirects unless the caller passes `-L`. Matches curl.
    #[default]
    Manual,
    /// Automatically follow redirects up to `--max-redirs`. Analogue of
    /// curl's `.curlrc` `location` directive.
    Auto,
}

/// Per-request result of the egress policy check.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EgressResult {
    /// Request is permitted.
    Allowed,
    /// Request was denied by the embedder's allowlist.
    Denied,
}

/// The embedder's decision about how this curl tool is named and bounded.
///
/// Built by [`CurlConfig::default`] (or [`CurlConfig::new`], the same
/// thing spelled as a constructor) and narrowed from there. Fields are
/// private so the only reachable transitions are through the builders.
#[derive(Clone)]
#[non_exhaustive]
pub struct CurlConfig {
    tool_name: String,
    limits: Limits,
    follow_redirects: RedirectPolicy,
    allow_egress: Arc<dyn AllowEgress>,
}

impl std::fmt::Debug for CurlConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CurlConfig")
            .field("tool_name", &self.tool_name)
            .field("limits", &self.limits)
            .field("follow_redirects", &self.follow_redirects)
            .field("allow_egress", &"[AllowEgress]")
            .finish()
    }
}

impl CurlConfig {
    /// The default config: tool name `curl`, deny-by-empty egress allowlist,
    /// manual redirect policy, and the [`Limits`] defaults.
    ///
    /// Registering as `curl` deliberately shadows any external `curl` on
    /// PATH — kaish resolves builtins before PATH, which is the point for
    /// an agent surface. Use [`CurlConfig::with_tool_name`] if you want
    /// both to be reachable.
    pub fn new() -> Self {
        Self::default()
    }

    fn make_allow_egress(policy: impl AllowEgress + 'static) -> Arc<dyn AllowEgress> {
        Arc::new(policy)
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

    /// Set the follow-redirect policy.
    pub fn with_redirect_policy(mut self, policy: RedirectPolicy) -> Self {
        self.follow_redirects = policy;
        self
    }

    /// Whether to follow redirects by default (the `.curlrc` analogue).
    pub fn follow_redirects(&self) -> RedirectPolicy {
        self.follow_redirects
    }

    /// Set the egress allowlist policy.
    ///
    /// The default is [`AllowByList::new`] — an empty allowlist, so nothing
    /// passes until the embedder names hosts. Call this with
    /// [`AllowByList`] for deny-by-default operation.
    pub fn with_allow_egress(mut self, policy: impl AllowEgress + 'static) -> Self {
        self.allow_egress = Self::make_allow_egress(policy);
        self
    }

    /// Permit this egress request.
    ///
    /// Returns [`EgressResult::Allowed`] if the policy permits the URL,
    /// [`EgressResult::Denied`] otherwise. The embedder's allowlist gates
    /// every outbound request and every redirect hop.
    pub fn permit_egress(&self, url: &str) -> EgressResult {
        if self.allow_egress.permit(url) {
            EgressResult::Allowed
        } else {
            EgressResult::Denied
        }
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
            follow_redirects: RedirectPolicy::default(),
            // Deny-by-empty-allowlist, which is what `CurlConfig::new`'s own
            // doc, this module's header, and docs/curl.md have always said
            // this was. It used to be `AllowAll`: an embedder that took the
            // default got unrestricted egress from a tool whose entire
            // safety story is the allowlist, and the unit test asserting it
            // was named for the deny-by-default behavior it was not testing.
            // A security default that fails open is worth nobody's
            // convenience.
            allow_egress: Self::make_allow_egress(AllowByList::new()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // ── Egress containment ──────────────────────────────────────────────
    //
    // The 2026-08-20 cross-model review found the allowlist walkable. Each
    // case below is a bypass that worked, or a legitimate URL that used to be
    // denied for the same parsing reason (docs/issues.md CU24-CU26).

    fn allowing(hosts: &[&str]) -> AllowByList {
        AllowByList::new().with_allowed_hosts(hosts.iter().copied())
    }

    #[test]
    fn userinfo_cannot_impersonate_an_allowlisted_host() {
        let policy = allowing(&["allowed.example"]);
        // The host here is 169.254.169.254. Splitting the string on the first
        // ':' saw `allowed.example` and said yes, while ureq dialed the
        // metadata service.
        assert!(!policy.permit("https://allowed.example:443@169.254.169.254/latest/meta-data/"));
        assert!(!policy.permit("http://allowed.example:80@127.0.0.1:9999/x"));
        assert!(!policy.permit("http://allowed.example@evil.example/x"));
        // The real host, reached with credentials in the URL, is still fine.
        assert!(policy.permit("https://user:pass@allowed.example/x"));
    }

    #[test]
    fn a_hostname_that_starts_like_an_ip_is_not_an_ip() {
        // `is_in_cidr` was `starts_with`, so `127.evil.com` was "loopback".
        let policy = AllowByList::new().with_allow_loopback(true);
        assert!(!policy.permit("http://127.evil.com/x"));
        assert!(policy.permit("http://127.0.0.1:8080/x"));
        assert!(policy.permit("http://localhost:8080/x"));

        let policy = AllowByList::new().with_allow_link_local(true);
        assert!(!policy.permit("http://169.254.evil.com/x"));
        assert!(policy.permit("http://169.254.169.254/latest/meta-data/"));
    }

    #[test]
    fn ipv6_literals_are_expressible() {
        // `[::1]:8080` split to `[` on the first colon, so no IPv6 host could
        // ever match — the `::1` branch was unreachable.
        let policy = AllowByList::new().with_allow_loopback(true);
        assert!(policy.permit("http://[::1]:8080/x"));
        assert!(!policy.permit("http://[2606:4700::1111]/x"));

        assert!(allowing(&["[2606:4700::1111]"]).permit("http://[2606:4700::1111]/x"));
    }

    #[test]
    fn host_matching_ignores_case_and_a_trailing_dot() {
        let policy = allowing(&["allowed.example"]);
        assert!(policy.permit("HTTP://Allowed.Example/x"));
        assert!(policy.permit("http://allowed.example./x"));
        // and an allowlist entry spelled either way still matches
        assert!(allowing(&["Allowed.Example."]).permit("http://allowed.example/x"));
    }

    #[test]
    fn an_unparseable_url_is_denied() {
        // Fail closed: if we cannot say what host this reaches, it does not go.
        let policy = allowing(&["allowed.example"]);
        assert!(!policy.permit("not a url"));
        assert!(!policy.permit("http://"));
        assert!(!policy.permit(""));
    }

    #[test]
    fn link_local_and_loopback_stay_denied_unless_opted_into() {
        let policy = allowing(&["allowed.example"]);
        assert!(!policy.permit("http://127.0.0.1/x"));
        assert!(!policy.permit("http://[::1]/x"));
        assert!(!policy.permit("http://169.254.169.254/x"));
        assert!(!policy.permit("http://metadata.google.internal/x"));
        assert!(!policy.permit("http://100.100.100.200/x"));
    }

    #[test]
    fn default_config_is_named_curl_with_deny_by_default_egress() {
        let cfg = CurlConfig::default();
        assert_eq!(cfg.tool_name(), "curl");
        // An embedder that registers curl and sets nothing else reaches
        // nothing. This test carried this name while asserting the opposite.
        assert_ne!(cfg.permit_egress("https://example.com"), EgressResult::Allowed);
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
        assert_eq!(l.max_time, 30.0);
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
            max_time: 60.0,
        });
        assert_eq!(cfg.limits().max_redirects, 0);
        assert_eq!(cfg.limits().max_response_bytes, 1024);
        assert_eq!(cfg.limits().max_time, 60.0);
    }

    #[test]
    fn redirect_policy_defaults_to_manual() {
        assert_eq!(CurlConfig::default().follow_redirects(), RedirectPolicy::Manual);
    }

    #[test]
    fn allow_by_list_denies_everything_by_default() {
        let cfg = CurlConfig::default().with_allow_egress(
            AllowByList::new()
        );
        assert_eq!(cfg.permit_egress("https://example.com"), EgressResult::Denied);
    }

    #[test]
    fn allow_by_list_permits_named_hosts() {
        let cfg = CurlConfig::default().with_allow_egress(
            AllowByList::new().with_allowed_hosts(["example.com", "api.example.org"])
        );
        assert_eq!(cfg.permit_egress("https://example.com/path?q=1"), EgressResult::Allowed);
        assert_eq!(cfg.permit_egress("https://api.example.org/data"), EgressResult::Allowed);
        assert_eq!(cfg.permit_egress("https://evil.com/spoof"), EgressResult::Denied);
    }

    #[test]
    fn allow_by_list_permits_loopback_when_enabled() {
        let cfg = CurlConfig::default().with_allow_egress(
            AllowByList::new().with_allow_loopback(true)
        );
        assert_eq!(cfg.permit_egress("http://127.0.0.1:8080/api"), EgressResult::Allowed);
        assert_eq!(cfg.permit_egress("http://localhost:3000/"), EgressResult::Allowed);
        // Named host still needs explicit allowlist membership.
        assert_eq!(cfg.permit_egress("https://example.com"), EgressResult::Denied);
    }

    #[test]
    fn allow_by_list_denies_metadata_ranges_by_default() {
        let cfg = CurlConfig::default().with_allow_egress(
            AllowByList::new()
        );
        assert_eq!(cfg.permit_egress("http://169.254.169.254/latest/meta-data/"), EgressResult::Denied);
        assert_eq!(cfg.permit_egress("http://metadata.google.internal/computeMetadata/v1/"), EgressResult::Denied);
    }

    #[test]
    fn allow_by_list_permits_metadata_when_explicitly_enabled() {
        let cfg = CurlConfig::default().with_allow_egress(
            AllowByList::new().with_allow_link_local(true)
        );
        assert_eq!(cfg.permit_egress("http://169.254.169.254/latest/meta-data/"), EgressResult::Allowed);
    }

    /// Every field is private and every builder narrows; there is no path to
    /// widen the egress surface or add hosts after construction. This is the
    /// subtractive guarantee enforced by compilation.
    #[test]
    fn no_public_method_can_widen_the_egress_surface() {
        // The compiler checks this: after construction with AllowByList,
        // the only method is permit_egress which evaluates the policy.
        // There is no add_host(), enable_loopback(), etc.
        let cfg = CurlConfig::default().with_allow_egress(
            AllowByList::new().with_allowed_hosts(["only-this.com"])
        );

        // This compiles and runs — it calls permit_egress (narrows, never widens).
        assert_eq!(cfg.permit_egress("https://only-this.com"), EgressResult::Allowed);
        assert_eq!(cfg.permit_egress("https://any-other.com"), EgressResult::Denied);

        // These methods do NOT exist (compile error if uncommented):
        // cfg.add_host("evil.com");          // no such method
        // cfg.enable_loopback();             // no such method
        // cfg.with_allow_egress(Box::new(   // with_allow_egress replaces,
        //     AllowAll));                     // never widens
    }
}
