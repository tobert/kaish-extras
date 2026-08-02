# kaish-tools-git

A shallow, safety-first git read surface for [kaish](https://github.com/tobert/kaish),
built on gitoxide's plumbing crates rather than the `gix` facade. History,
autopsy, and design intent live in [`../../docs/git.md`](../../docs/git.md);
the crate layout, verb surface, and read-only enforcement argument are in
[`../../docs/design/architecture.md`](../../docs/design/architecture.md).

This is PR 1 of that plan: `git info`, the repository open path (ceilinged
discovery, isolated config parsing, the refusals), the fixture harness, and
the `.git` fingerprint test. One verb so far; the rest arrive one PR at a
time.

```sh
kaish> git info
kaish> git info --repo /mnt/repos/kaish --json
```

Register it in an embedder's `configure_tools` closure:

```rust
tools.register(kaish_tools_git::tool(GitConfig::read_only())?);
```

## Read-only, and why you should believe it

Not "a flag is checked" — in a default build the code that could write does
not exist. Four things enforce that, and each can fail on its own:

- `ReadRepo` assembles the gitoxide plumbing handles itself and never lends
  them out, so no caller can reach a ref transaction or an index writer.
- `tests/write_shaped_identifiers.rs` scans this crate's source and asserts
  the write-shaped plumbing (`gix_object::Write`, gix-ref transactions,
  gix-index writers) appears nowhere outside `src/verbs/write/`.
- `tests/readonly_fingerprint.rs` fingerprints every path, size, mtime and
  content hash under `.git`, runs every read verb across a flag matrix, and
  asserts nothing moved. The same file asserts that real `git status` *does*
  write, so the fingerprint keeps its teeth.
- The dependency tripwires below: nothing in the graph can spawn a
  subprocess, so an attacker-controlled `diff.*.textconv` in a repository's
  own config is inert text — nothing exists that could act on it.

Repository discovery is ceilinged at the VFS mount root
(`tests/discovery_ceiling.rs`): a repository *above* the mount is invisible
from inside it. Note the converse, because the opposite belief is the
dangerous one — mounting a repository `LocalFs::read_only` does **not** make
this crate read-only. It makes kaish's own file verbs read-only on that path;
git goes around the VFS entirely on the native path, and the four layers
above are what make it read-only.

## Tests

Fixtures are built by shelling out to real `git` on PATH, deliberately: a
fixture is then whatever git actually produces — a real pack, a real
`worktrees/` entry, real `packed-refs` — rather than whatever we believed it
produces. That is fine in tests and forbidden in the crate, which links no
spawn machinery at all. The suite asserts git's presence rather than skipping
without it.

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
