//! A shallow, safety-first git read surface for kaish, built on gitoxide
//! plumbing crates rather than the `gix` facade — see
//! [`docs/git.md`](https://github.com/tobert/kaish-extras/blob/main/docs/git.md)
//! for the history and design intent, and
//! [`docs/design/architecture.md`](https://github.com/tobert/kaish-extras/blob/main/docs/design/architecture.md)
//! for the crate layout, the verb surface, and the read-only enforcement
//! argument this crate exists to prove. This is PR 0 of that plan: the
//! workspace member, its pinned dependencies, and the dependency tripwires
//! that keep spawn-capable machinery out of the graph. No verbs yet.

#[cfg(target_family = "wasm")]
compile_error!(
    "kaish-tools-git does not build for wasm targets yet: gix-sec fails to \
     compile on wasm32-unknown-unknown (ownership check uses geteuid). \
     Tracked in kaish-extras GH #3. Build kaish-web without the git tool."
);
