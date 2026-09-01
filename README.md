# kaish-extras

Companion repo to [kaish](https://github.com/tobert/kaish) — 会sh, the
sandboxed, embeddable shell for agents. kaish core stays lean; the extras
live here:

- **`crates/kaish-web`** — the kaish kernel compiled to
  `wasm32-unknown-unknown` behind a small wasm-bindgen surface
  (`KaishShell`: `execute`, `execute_interruptible`, `complete`,
  `seed_file`). The whole shell — parser, VFS,
  builtins, jq — runs in the browser tab: no server, no syscalls, no POSIX.
- **`site/`** — the *try kaish* playground built on it: a REPL seeded with
  the kaish and kaish-extras source trees, so you can `grep` the shell's own
  implementation from inside the shell. Published via GitHub Pages.
- **`crates/kaish-tools-git`** — a reinvented git plugin, deliberately kept out
  of kaish core to keep its dependency tree light: shallow, safety-first,
  read-only by construction, gitoxide plumbing rather than the `gix` facade.
  History, autopsy, and design intent in [docs/git.md](docs/git.md); the
  registration guide, every `GitConfig` knob, and the read-only story stated
  precisely in [docs/embedding-git.md](docs/embedding-git.md).
- *(planned)* further tool bundles kept out of kaish core the same way: jq and
  ripgrep as add-ons.

## How the playground works

Three layers, main thread → worker → wasm:

- **`site/index.html`** — the REPL UI. Owns history, keybindings, scrollback
  (capped at 4000 nodes), and the Ctrl-C flow. Talks to the worker over a
  strict FIFO message protocol (`boot` / `seed` / `exec` / `complete`); the
  page never blocks on the kernel.
- **`site/worker.js`** — owns the wasm instance. Messages process in order;
  execution is synchronous inside the worker so results return FIFO. When the
  interrupt flag is available it calls `execute_interruptible` (the production
  path; plain `execute` is the non-isolated fallback and what smoke.html
  exercises). The worker (not the main thread) consumes the interrupt flag
  after each exec so queued commands survive a Ctrl-C aimed at the current one.
- **`crates/kaish-web`** — `KernelConfig::isolated()`: in-memory VFS rooted at
  `/`, no subprocesses, hermetic env. A current-thread tokio runtime driven by
  `block_on` per call suffices because all I/O is in-memory. Guardrails, since
  a tab is not a machine: 256 MiB VFS budget, kernel-level output limits
  (`OutputLimitConfig::agent().in_memory()`), and 1 MB out/err clipping at the
  wasm boundary. A wasm panic *does* poison the instance — the protection is
  at the worker boundary: `console_error_panic_hook` turns the panic into a
  thrown JS error, worker.js catches it and posts a `crash` message, and the
  main thread respawns the worker (state resets, honestly reported).

**Ctrl-C has two tiers.** Tier 2 (preferred): the page flips an `Int32Array`
over a `SharedArrayBuffer`; the kernel polls it via `ExecuteOptions::interrupt`
and stops with exit 130, session state intact. SharedArrayBuffer requires
cross-origin isolation, and GitHub Pages can't set headers — `site/coi-sw.js`
is a service worker that stamps COOP/COEP onto every same-origin response, with
a one-shot guarded reload so the document response gets them too. Tier 1
(fallback when not isolated): terminate + respawn + reseed the worker
(milliseconds in practice) with an honest state-reset notice. The e2e branches
on which tier is active.

**Completion** (`KaishShell::complete`) shares context detection with the
native REPL via `kaish_client::completion`; candidates come from the live
kernel — tool schemas for commands (and for `--flag` words in argument
position), the scope for variables, the in-memory VFS for paths.

**Seeding**: `scripts/make_seed.py` packs the git-tracked text files of the
kaish and kaish-extras repos into `site/seed.json` (a path → contents map);
the worker writes them
through the kernel's own VFS router under `/src/<name>/`. It skips binaries,
non-UTF-8, and files >512 KiB — the seed is not literally every tracked file.

## Building the site

Prerequisites: Rust with the wasm target, `python3` (seed generation and local
serving), `deno` and a Chromium-based browser (the machine checks below).
`wasm-opt` (binaryen) is optional locally but used when present.

```sh
rustup target add wasm32-unknown-unknown
cargo install wasm-bindgen-cli --locked --version \
  "$(grep -A1 '^name = "wasm-bindgen"$' Cargo.lock | grep '^version' | cut -d'"' -f2)"
scripts/build-site.sh ~/src/kaish   # → site/ is a static bundle
python3 -m http.server -d site 8137 # try it locally
```

Two machine checks, both against a locally served `site/`:

```sh
# engine smoke (main-thread wasm, virtual-time friendly):
chromium --headless=new --virtual-time-budget=20000 \
  --dump-dom http://127.0.0.1:8137/smoke.html | grep "SMOKE OK"

# full e2e (worker boot, seeding, FIFO exec, Ctrl-C interrupt+restart) —
# real-time CDP, because virtual time can't wait on a Web Worker:
deno run --allow-net --allow-run scripts/e2e.ts
```

## kaish dependency

All four kaish crates are pinned to **published crates.io versions** — `"0.17"`
as of 2026-09-01 — which keeps kaish-extras an honest external embedder of the
same API everyone else gets. A git rev is an affordance no real embedder has,
so it is used only while developing against an unreleased kaish boundary.

The rules for changing the pin, and what a kaish minor costs here, are in
[AGENTS.md](AGENTS.md), "kaish dependency pinning".
