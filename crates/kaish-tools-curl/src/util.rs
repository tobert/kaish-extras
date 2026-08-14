//! A blocking-work scheduling helper, copied from `kaish-tools-git`.
//!
//! This is a deliberate copy, not shared code: `kaish-tools-git`'s
//! `block_in_place_compat` (`crates/kaish-tools-git/src/tool.rs`) is
//! `pub(crate)`, and `kaish-tool-api` — the only kaish crate this workspace
//! lets a tool crate depend on — does not export it. Two out-of-tree tool
//! crates independently needing the same runtime-flavor dance is a signal
//! for kaish to grow the seam (an AGENTS.md-style upstream fix), not a
//! reason to invent a third, unpublished internal crate just to share nine
//! lines.

/// Run blocking work — the future ureq backend's blocking HTTP calls —
/// without stalling the runtime, on either flavor.
///
/// `tokio::task::block_in_place` is the right call on a multi-thread
/// runtime and *panics* on a current-thread one, which an embedder may well
/// be using. Same work either way — this picks a scheduling strategy, it is
/// not a semantic fallback, and the breadcrumb says which path ran so a
/// surprise is visible in a trace rather than inferred from a stall. This
/// is the exact seam docs/curl.md's "Native backend: ureq" section names as
/// already proven by `kaish-tools-git`.
///
/// Not yet called from production code: the ureq backend that will use it
/// (`backend/ureq.rs`) is out of scope for this skeleton (the HTTP surface
/// waits on the review docs/curl.md's "Status" section calls for).
/// `#[allow(dead_code)]` says so rather than leaving a warning an agent
/// might "fix" by deleting the function the next PR needs.
#[allow(dead_code)]
pub(crate) fn block_in_place_compat<T>(f: impl FnOnce() -> T) -> T {
    match tokio::runtime::Handle::try_current() {
        Ok(handle) if handle.runtime_flavor() == tokio::runtime::RuntimeFlavor::MultiThread => {
            tracing::debug!(strategy = "block_in_place", "curl: entering blocking HTTP work");
            tokio::task::block_in_place(f)
        }
        Ok(_) => {
            tracing::debug!(
                strategy = "direct",
                "curl: entering blocking HTTP work on a current-thread runtime"
            );
            f()
        }
        Err(_) => {
            tracing::debug!(strategy = "no-runtime", "curl: entering blocking HTTP work");
            f()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Both scheduling paths must run the closure exactly once and return
    /// its value. The current-thread case is the one that would panic if we
    /// reached for `block_in_place` unconditionally.
    #[test]
    fn block_in_place_compat_runs_without_a_runtime() {
        assert_eq!(block_in_place_compat(|| 42), 42);
    }

    #[tokio::test]
    async fn block_in_place_compat_runs_on_a_current_thread_runtime() {
        assert_eq!(block_in_place_compat(|| 42), 42);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn block_in_place_compat_runs_on_a_multi_thread_runtime() {
        assert_eq!(block_in_place_compat(|| 42), 42);
    }
}
