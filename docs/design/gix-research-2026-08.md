# gitoxide (gix) for kaish-git — verified research (2026-08-01)

Design input for kaish-git (see [../git.md](../git.md)). Produced by a research
agent that **built throwaway probe crates against real crates.io versions**
rather than trusting docs — the compile-matrix claims below were verified
empirically on this machine (rustc 1.96.0, gix 0.86.0). Version-specific facts
(feature names, CI matrix, crate-status checkboxes) will drift; re-verify before
implementation if much time has passed.

---

**Bottom line up front:** `gix` 0.86.0 **compiles clean for `wasm32-wasip1` today** with the full read-only surface (status, blob-diff, blame, revwalk) — verified empirically, not from docs. It does **not** compile for `wasm32-unknown-unknown` (`gix-sec` blocker). The real wasm caveat is runtime, not compile time: `gix-pack` uses `memmap2`, which is a stub on non-unix/non-windows, so **packed objects will fail to load under WASI**. Network features don't build for wasm at all and must be feature-gated off.

---

## 0. Empirical verification (2026-08-01, rustc 1.96.0)

Throwaway crates built against real crates.io versions rather than trusting docs.

| Config | Target | Result |
|---|---|---|
| `gix 0.86` `default-features=false` + `sha1,index,revision,blob-diff,status,blame,max-control` | `x86_64-unknown-linux-gnu` | ✅ compiles |
| same | **`wasm32-wasip1`** | ✅ **compiles clean** (178 crates) |
| same | `wasm32-unknown-unknown` | ❌ `gix-sec` fails: `std::os::unix::fs::MetadataExt`, `libc::geteuid` not found |
| `sha1,index,revision` only | `wasm32-wasip1` | ✅ compiles (144 crates) |
| above + `blocking-http-transport-reqwest` + `reqwest/rustls-no-provider` + `rustls/ring` | host | ✅ compiles, **no aws-lc-sys, no openssl-sys, no curl** |
| same network config | `wasm32-wasip1` | ❌ `socket2` "doesn't support the compile target", `tokio` wasm feature restrictions |

The `wasm32-unknown-unknown` failure is precise and fixable-looking: `gix-sec-0.14.2/src/identity.rs:30` gates on `#[cfg(all(not(windows), not(target_os = "wasi")))]` — WASI is special-cased to `Ok(true)` (ownership check always passes), but bare wasm32 is not. That's exactly why gitoxide's CI has a "WASI only: crates without feature toggle → `gix-sec`" step.

**C-toolchain audit of the read-only tree:** only build-script dep is `zlib-rs` (pure Rust). gix now always uses `zlib-rs` — `max-performance-safe` is documented as *"Deprecated: gix always uses zlib-rs, so this is equivalent to `max-performance`"*. No `cc`, no `cmake`, no `openssl-sys`. A genuinely clean fit for the no-C-deps constraint.

## 1. gitoxide wasm32 state (2026)

### The tracking issue is gone

[Issue #463 "WASM support"](https://github.com/GitoxideLabs/gitoxide/issues/463) was **closed as not planned on 2026-07-22** and moved to [Discussion #2772](https://github.com/GitoxideLabs/gitoxide/discussions/2772) — part of a repo-wide migration of task tracking to Discussions. The closure is a process change, not abandonment, but Byron's stance is explicit and recent:

> "I leave it to @EliahKagan to dig through this proposal if WASM ever becomes a priority again. I myself skip investing time into this for that reason as well." — Byron, 2026-06-21

> "without a known downstream consumer this is really just best-effort, or a blind stab that ideally is cheap to maintain." — Byron, 2025-05-05

**Practical read: wasm is unowned upstream.** kaish would be the "known downstream consumer" that doesn't currently exist. That's leverage if you want to push fixes upstream, and risk if you assume it stays working.

### What CI actually guarantees

gitoxide's [`ci.yml`](https://github.com/GitoxideLabs/gitoxide/blob/main/.github/workflows/ci.yml) `wasm` job (matrix: wasm32-unknown-unknown, wasm32-wasip1, wasm32-wasip2) builds only a hand-maintained subset of leaf crates: `gix-sec` (WASI only), 16 no-feature-toggle crates, `gix-features` sub-features, the `sha1` group (`gix-commitgraph`, `gix-hash`, `gix-hashtable`, `gix-index`, `gix-object`, `gix-refspec`, `gix-revision`, `gix-traverse`), and `gix-pack` with `sha1,wasm`.

**`gix` itself is not in the list**, nor are `gix-odb`, `gix-ref`, `gix-diff`, `gix-discover`, `gix-status`, `gix-dir`, `gix-blame`, `gix-worktree`, `gix-config`, `gix-filter`. The list is openly acknowledged as stale (issue #1988). **The probe build proves the list understates reality for wasip1** — the top-level `gix` and every crate above builds. But nothing in CI defends that, so it can break on any release without notice.

### The `wasm` feature toggle is narrow

Only `gix-pack` has one: `wasm = ["gix-diff?/wasm"]`, for `wasm32-unknown-unknown`. Not needed for wasip1 — the probe never enabled it.

### The runtime blocker: mmap

`gix-pack-0.73.0/src/lib.rs:63`:

```rust
pub fn read_only(path: &Path) -> std::io::Result<memmap2::Mmap> {
    let file = std::fs::File::open(path)?;
    unsafe { memmap2::MmapOptions::new().map_copy_read_only(&file) }
}
```

`memmap2-0.9.11` selects `stub.rs` for `#[cfg(not(any(unix, windows)))]`, and the stub returns `Err(io::ErrorKind::Unsupported)` unconditionally. `wasm32-wasip1` has no `unix` cfg. So on WASI:

- **Loose objects** (`.git/objects/ab/cdef…`) → plain `std::fs`, works.
- **Packfiles + `.idx` + multi-pack-index** → hard failure, no fallback path in gix-pack.

Every cloned repo stores history in packs, so *"gix on wasip1"* is close to nothing without a patch. Contributor `uberroot4` hit and patched exactly this ([comment, 2025-04-10](https://github.com/GitoxideLabs/gitoxide/issues/463#issuecomment-2793144836), commit `ac34bdd`), replacing mmap with `Vec<u8>` on wasi targets — and got commit-graph traversal, `gix-diff` **and** `gix-blame` running in a browser under wasip2. Byron's response was receptive but gated on "someone meaningfully using it."

**This is the one real upstream ask:** a read-into-memory fallback in `gix-pack::mmap::read_only` for targets where mmap is unsupported. Small, testable, precedented, and Byron has already blessed the shape of it.

### Virtual filesystem: not possible

[Discussion #1150](https://github.com/GitoxideLabs/gitoxide/discussions/1150) — "Is it possible to use Gitoxide without a file system?" Byron: **"No, that's not possible yet."** gix goes straight to `std::fs` everywhere. **Consequence for kaish: you cannot route gix through `kaish-vfs`.** A kaish-git plugin operates on real host paths only — it belongs behind the `localfs` capability axis and is unavailable in `MemoryFs`/`OverlayFs` mounts. (kaish can still *expose* git data through its VFS from above; it just can't make gix read from a virtual backend below.)

### wasip2 note

`wasm32-wasip2` is in gitoxide's CI matrix and is what the contributor actually exercised. `kaish-wasi` targets wasip1, which the probe verified; wasip2 is likely equal-or-better but untested here.

## 2. TLS and transports

### You don't need transport at all for the read profile

gix's network features are opt-in and mutually-exclusive-by-design. The read-only probe compiles and links with **zero transport crates**. Local-repo operations — discover, open, refs, revwalk, tree/blob read, diff, status, blame — are fully offline. **Confirmed.**

### The reqwest+rustls+ring trap (important)

**Do not use `blocking-http-transport-reqwest-rust-tls`** — it chains to `reqwest/rustls`, and in **reqwest 0.13.4** `rustls = ['__rustls-aws-lc-rs', ...]` — it **pulls aws-lc-rs** (cmake). Also note reqwest 0.13's `default-tls` is now rustls+aws-lc-rs, so any accidental `default-features = true` on reqwest anywhere in the workspace reintroduces cmake. Worth a `cargo deny`/`cargo tree` tripwire in CI.

**The correct wiring is anticipated by gix:** enable the bare `blocking-http-transport-reqwest` feature (gix-transport declares reqwest with `default-features = false` precisely to allow this), add reqwest as a **direct** dependency with `rustls-no-provider`, and install ring as process default provider. This is exactly kaibo's pattern. Verified tree: `ring 0.17.14`, `rustls 0.23.43`, `rustls-webpki`, `hyper-rustls` — **no `aws-lc-sys`, no `openssl-sys`, no `native-tls`, no `curl`**.

Caveats: `cc` appears via ring's own C/asm shims (needs a C compiler but not cmake — the deliberate ring-vs-aws-lc tradeoff already accepted in kaibo); `rustls-no-provider` still pulls `rustls-platform-verifier` (pure-Rust path discovery); reqwest 0.13 renamed the features (`rustls-no-provider`, not 0.12's `rustls-tls-no-provider`). curl-based stacks all pull C — avoid.

## 3. gix API maturity for the intended surface

From [`crate-status.md`](https://github.com/GitoxideLabs/gitoxide/blob/main/crate-status.md) plus direct source reading of gix 0.86.0.

| Surface | Feature | API | Verdict |
|---|---|---|---|
| **status** | `status` | `Repository::status()` → `Platform`/`Iter`, `index_worktree_status()`, `tree_index_status()`, `is_dirty()` | ✅ **Ready.** Rename tracking, untracked files, recursive submodule status. No fsmonitor/sparse-index acceleration. |
| **diff tree↔tree** | `blob-diff` | `Repository::diff_tree_to_tree(old, new, opts)` | ✅ **Ready.** Rename/copy tracking, binary detection. |
| **diff tree↔worktree** | `status` | via status | ✅ Ready. |
| **diff tree↔index** | — | — | ❌ Missing. Workaround: `index_from_tree()` then tree↔tree. |
| **patch/hunk output** | `blob-diff` | line diffs via `imara-diff` | ⚠️ **You assemble it yourself.** No `git diff`-compatible unified patch for free (crate-status: text/binary formatting, interhunk-lines, whitespace settings, hunk handling all unchecked). Biggest concrete gap for a `diff`/`show` builtin. |
| **log / revwalk** | (core) | `rev_walk()`, `rev_parse()`, `revision_graph()` | ✅ **Ready.** Tips + exclusions; minor perf gaps. |
| **blame** | `blame` | `Repository::blame_file(path, suspect, Options)` | ⚠️ **"blame-ish."** Single file, line ranges, `--since`. Missing: worktree changes (must pass a committed ObjectId), rename tracking across paths, shallow history, streaming. Not perf-competitive with git yet. |
| **show** | — | `find_object` + `diff_tree_to_tree` | ✅ Ready (subject to patch-formatting gap). |
| **worktree read** | (core) | `worktree()`, `worktrees()`, locked-state, per-worktree config/refs | ✅ Read is ready. |
| **worktree create/move/remove** | — | — | ❌ **Missing.** Kills the easy "later worktree write support" plan unless implemented against `gix-ref`/`gix-discover` plumbing ourselves, upstreamed, or delegated to the shell-out tier. |
| **commit creation** | (core) | `commit()`, `commit_as()`, tree editing via `tree-editor` | ✅ **Ready** for plain commits. No signed commits/tags ([#12](https://github.com/GitoxideLabs/gitoxide/issues/12)). |
| **add / index mutation** | `index` | index read/write primitives | ⚠️ Partial — no `.gitignore`-aware `add` porcelain. |
| **checkout/switch/restore/reset** | — | — | ❌ Explicitly needs cross-crate plumbing work. |
| **config writes** | — | — | ❌ Missing. |
| **reftable** | — | `gix-reftable` | ❌ **All boxes unchecked.** As git rolls out reftable, a files-backend-only reader will fail on newer repos — emit an explicit "unsupported ref backend" error, never a wrong answer. |
| **hooks** | — | — | ❌ Not executed. For read-only this is a *feature*. |

### Two design notes that matter for kaish specifically

**(a) `blob-diff` transitively enables subprocess spawning.** `blob-diff` → `attributes` → `gix-command`. You cannot get gix's tree diff without linking machinery that shells out for `diff.<driver>.textconv` / `diff.<driver>.command` / clean-smudge filters — all configured by **repo-local config and `.gitattributes`**, i.e. attacker-controlled when opening an untrusted repo. This collides with kaish's `subprocess` capability axis: a no-subprocess kaish build would still link a git layer that can spawn. Mitigations: (i) open with `gix::open::Permissions::isolated()` — but note repo-local `.git/config` is *always* loaded for correctness, so textconv config remains in scope; (ii) drop to `gix-diff`'s `tree_with_rewrites` without the `blob` feature for name/status-only diff, avoiding `attributes` entirely.

**`Permissions::default()` is `secure()`, which is currently *identical to* `all()`** (verified in source — both construct `Environment::all() + Config::all() + Attributes::all()`). **Do not assume "secure" means restricted.** Use `isolated()` explicitly; it maps cleanly onto kaish's hermetic-env doctrine.

**(b) MSRV.** gix 0.86.0 is `edition = "2024"`, `rust-version = "1.85"` — exactly kaish's MSRV. Watch on bumps.

## 4. Alternatives

| Option | Verdict |
|---|---|
| **`gix` / gitoxide** — 0.86.0 (2026-07-23) | ✅ **The only real candidate.** Pure Rust, zlib-rs, no C except optional ring for TLS. Production-proven (Cargo, Helix, GitButler, jj). Still 0.x with breaking minor bumps roughly monthly — pin exactly (`=0.86.0`); note 0.82.0 was yanked. |
| **`git2` / libgit2** — 0.21.0 | ❌ Correctly rejected already. C toolchain; wasm issue closed in 2020, dead. |
| **`rs-git-lib`** — 0.2.1, last published 2020 | ❌ Dead. |
| **wasm-git** (emscripten libgit2) | ❌ Wrong shape (browser JS, not a Rust lib). |
| **Shell out to `git(1)`** | ⚠️ **Keep as the fallback tier** behind `subprocess`: 100% fidelity (unified patches, blame, reftable, worktree create/remove) at the cost of no wasm, no hermeticity, no read-only guarantee. Escape hatch for surfaces gix can't do, not the primary. |

## 5. Feature-flag recipes

### 5a. Read-only local repo, wasm-capable (verified: host + wasm32-wasip1)

```toml
[dependencies]
gix = { version = "=0.86.0", default-features = false, features = [
    "sha1",        # required: at least one hash algorithm, or compilation fails
    "index",       # .git/index access — prerequisite for revision/excludes/status
    "revision",    # rev-parse, rev-walk, describe, merge-base  → log
    "blob-diff",   # tree-to-tree diff + line diffs             → diff, show
    "status",      # index↔worktree and tree↔index              → status
    "blame",       # single-file commit annotation              → blame-ish
    # "max-control",  # parallel + LRU pack caches. HOST ONLY — see notes.
] }
```

Deliberately absent:
- **No `default-features`** — gix defaults pull `extras`, and `interrupt` alone drags in `signal-hook`, wrong for an embedded kernel and wrong for wasm.
- **`max-control`** compiled for wasip1 in the probe, but `parallel` makes gix use threads; gate it per-target. Without `parallel`, most gix types are **not `Send`** — plan on `block_in_place` around gix calls regardless (gix is entirely blocking I/O; that's already the house pattern).
- **`revparse-regex`** costs a `regex` dep for full revspec fidelity (`@^{/^.*x}`) — skip initially; kaish doesn't want regex hell anyway.
- **Minimal variant**: `["sha1", "index", "revision"]` (144 crates vs 178) defers diff/status/blame and crucially avoids the `attributes`→`gix-command` subprocess linkage of §3(a).

### 5b. Wasm gating shape

Network can't build for wasm and mmap doesn't work there — make it a capability axis, mirroring kaish's `localfs`/`subprocess`/`host` pattern:

```toml
[features]
default = ["local"]
local   = ["gix"]                      # read-only, offline, wasm-capable
remote  = ["gix/blocking-http-transport-reqwest", "dep:reqwest", "dep:rustls"]
```

with `remote` excluded from any wasi build entirely.

### 5c. Later: network with rustls + ring, no aws-lc-rs (verified on host)

```toml
gix = { version = "=0.86.0", default-features = false, features = [
    # ...read-only set above...
    "blocking-http-transport-reqwest",   # the bare feature — NOT "-rust-tls"
] }
reqwest = { version = "0.13", default-features = false, features = [
    "blocking", "charset", "http2",
    "rustls-no-provider",   # rustls WITHOUT aws-lc-rs (`rustls` alone pulls it)
] }
rustls = { version = "0.23", default-features = false, features = ["ring", "std", "tls12"] }
```

```rust
// Once, at process init — before any gix fetch/clone. rustls-no-provider bakes
// no default provider; without this, TLS fails at connect time.
rustls::crypto::ring::default_provider()
    .install_default()
    .expect("rustls crypto provider already installed");
```

Verified absent from the tree: `aws-lc-sys`, `aws-lc-rs`, `openssl-sys`, `native-tls`, `curl`. Networking roughly doubles the build (295 crates vs 178). Gotchas: `blocking-http-transport-reqwest` transitively enables `credentials` (`gix-prompt` may try to prompt — audit before shipping); gix's network client is std-blocking, not tokio.

## Recommended shape for kaish-git

1. **Ship read-only on `localfs` only, real host paths only** — gix cannot go through `kaish-vfs` (Discussion #1150), so this is a hard architectural constraint, not a phase-1 shortcut.
2. **Open with `Permissions::isolated()`**, not the default — `secure()` is currently a synonym for `all()`, and isolation matches kaish's hermetic-env doctrine.
3. **Prefer `gix-diff`'s name/status-level `tree_with_rewrites` over `blob-diff`** where the surface allows, keeping `gix-command` out of no-subprocess builds.
4. **Treat wasm as compile-gated-and-known-degraded, not working.** It compiles; packed objects don't load. Either (a) upstream the non-mmap fallback in `gix-pack` — small, precedented, and kaish would be the "real consumer" Byron asked for — or (b) compile the git plugin out of wasi builds until (a) lands. Never ship a build that silently returns "object not found" for every packed object; that's the silent-fallback failure mode the project rejects.
5. **Budget for the two real gaps:** unified-patch formatting (gix gives changes + line diffs, not `git diff` output) and worktree create/move/remove (absent). File both as issues at design time, not discoveries at implementation time.

## Sources

- [gitoxide WASM support — issue #463 (closed not-planned 2026-07-22)](https://github.com/GitoxideLabs/gitoxide/issues/463) → [Discussion #2772](https://github.com/GitoxideLabs/gitoxide/discussions/2772)
- [gitoxide CI wasm-crate list staleness — issue #1988](https://github.com/GitoxideLabs/gitoxide/issues/1988)
- [gitoxide without a filesystem — Discussion #1150](https://github.com/GitoxideLabs/gitoxide/discussions/1150)
- [gitoxide `ci.yml` wasm job](https://github.com/GitoxideLabs/gitoxide/blob/main/.github/workflows/ci.yml) · [`crate-status.md`](https://github.com/GitoxideLabs/gitoxide/blob/main/crate-status.md) · [`gix/Cargo.toml`](https://github.com/GitoxideLabs/gitoxide/blob/main/gix/Cargo.toml)
- [docs.rs/gix/0.86.0](https://docs.rs/gix/0.86.0/gix/struct.Repository.html) · [reqwest 0.13.4 features](https://crates.io/crates/reqwest)
- [git2-rs WebAssembly — issue #511 (closed 2020)](https://github.com/rust-lang/git2-rs/issues/511) · [wasm-git #77](https://github.com/petersalomonsen/wasm-git/issues/77) · [jj wasip1 PR #4519 (closed unmerged)](https://github.com/martinvonz/jj/pull/4519)
- Local verification: rustc 1.96.0 probe builds; source inspection of `memmap2-0.9.11`, `gix-pack-0.73.0`, `gix-sec-0.14.2`, `gix-0.86.0/src/open/permissions.rs`.
