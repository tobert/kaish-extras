# kaish-tools-git

A shallow, safety-first git read surface for [kaish](https://github.com/tobert/kaish),
built on gitoxide's plumbing crates rather than the `gix` facade. History,
autopsy, and design intent live in [`../../docs/git.md`](../../docs/git.md);
the crate layout, verb surface, and read-only enforcement argument are in
[`../../docs/design/architecture.md`](../../docs/design/architecture.md).

This is PR 0 of that plan: the workspace member, its pinned dependencies, and
the dependency tripwires that keep spawn-capable machinery out of the graph.
No verbs yet.

## Why plumbing, not the facade

The `gix` facade crate links `gix-command` (subprocess-spawn machinery)
unconditionally, even with `--no-default-features` — a default build of this
crate must not be able to spawn a subprocess at all, so `gix` is not a
dependency here. Instead this crate builds directly on gitoxide's low-level
crates (`gix-object`, `gix-odb`, `gix-ref`, `gix-pack`, …), each exact-pinned.
See [`Cargo.toml`](Cargo.toml) for the full pin set and the reasoning behind
each one.

## wasm

**This crate does not build for wasm targets.** `src/lib.rs` opens with a
`compile_error!` naming the real current blocker: `gix-sec`, pulled in by
`gix-discover`/`gix-config`, fails to compile on `wasm32-unknown-unknown`
(its ownership check calls `geteuid`). Tracked in kaish-extras GH #3.
`kaish-web` does not depend on this crate.

## The tripwire contract

CI (`.github/workflows/ci.yml`) enforces, on every PR, that the workspace
lockfile contains none of `gix-command`, `gix-transport`, `gix-filter`,
`aws-lc-sys`, `openssl-sys`, `native-tls`, or `curl-sys` — via `cargo tree -i`
reporting "did not match any packages" for each. A separate CI step asserts
the wasm guard above actually fires, and fires with its own message rather
than a transitive dependency's compile error.
