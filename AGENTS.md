# AGENTS.md

Orientation for AI agents working in this repo. `CLAUDE.md` is a symlink here.
Architecture and build docs live in [README.md](README.md) — read that first;
this file is only what you need beyond it to work effectively.

The one principle that shapes everything: this repo is an **honest external
embedder** of kaish. `kaish-web` consumes the same public API any embedder gets.
When it needs a seam that doesn't exist, the fix is a kaish PR (same
maintainer), never a fork or a workaround here.

## Commands

Build, serve, smoke, and e2e commands are in README.md ("Building the site") —
use those. Beyond them:

```bash
# One-time: the wasm target (the only target kaish-web builds for)
rustup target add wasm32-unknown-unknown

# Lint — must be clean
cargo clippy --target wasm32-unknown-unknown -p kaish-web --all-targets
```

There are no cargo tests in this workspace — the checks are smoke.html and
`scripts/e2e.ts`. The e2e must be **real-time** (it polls over raw CDP):
`--virtual-time-budget` cannot wait on a Web Worker, which is why smoke.html
(main-thread wasm) and e2e.ts (worker) are separate harnesses. New site
behavior gets an e2e stage. PRs run clippy + site build + e2e
(`.github/workflows/ci.yml`); pushes to main run the same build + e2e as the
gate in front of the Pages deploy (`.github/workflows/pages.yml`) — that gate
has caught real broken deploys.

## kaish dependency pinning

`[workspace.dependencies]` in the root `Cargo.toml` pins both `kaish-kernel`
and `kaish-client` by git rev. Rules:

- **Both crates must pin the SAME rev** — mixed revs put two copies of
  kaish-kernel in the dependency graph.
- `kaish-kernel` is `default-features = false`. Keep it that way: a sibling
  crate enabling kernel default features tramples the no-default choice
  (`localfs` etc. must not leak into the browser build).
- Workflow for changes that need a kaish-side seam: open a kaish PR, pin both
  crates here to the PR branch head to develop against it, then bump both pins
  to the merged main sha once the PR lands. Before any crates.io release of
  these crates, pins move to published kaish versions.

## Cross-model review with kaibo

We review with [kaibo](https://github.com/tobert/kaibo) (解剖) — a read-only
codebase-analysis MCP that answers with `file:line` citations. kaibo embeds
kaish, so reviewing kaish-extras through it dogfoods the whole stack.

The combo that has earned its keep: start a deliberate job on a frontier model
(gemini pro and/or claude fable) with lots of **whole files attached**, and in
parallel run a deepseek agent over a similar surface. We generally do **not**
provide a diff — a reviewer without one evaluates the code holistically instead
of rubber-stamping the change. Cross-family review has caught real deploy
blockers here (wasm panic poisoning the instance, unbounded scrollback,
WebKit focus-during-keydown dropping the first char).

## Conventions

Only the house rules that aren't standard practice:

- Never silently discard errors. If an error is deliberately ignored, the
  narrowest case is matched and a comment says so (see `seed_file`: only
  `AlreadyExists` is swallowed).
- No legacy dual-representations — delete superseded code immediately, no
  compat shims or parallel old/new paths.
- Defer out-of-scope work to GitHub Issues, not inline TODOs or scratch notes.
- Comments carry non-obvious intent only; this codebase leans on them for
  browser-specific constraints (see worker.js, coi-sw.js) — keep that bar.

## Gotchas

- **Build from the workspace root.** `.cargo/config.toml` carries the
  `--cfg getrandom_backend="wasm_js"` rustflag that pairs with kaish-web's
  `getrandom` `wasm_js` feature; without it the wasm build fails with
  getrandom unable to find a backend.
- **wasm-bindgen-cli must match the locked `wasm-bindgen` version** in
  Cargo.lock. After a bump, reinstall with the same one-liner CI uses:
  `cargo install wasm-bindgen-cli --version "$(grep -A1 '^name = "wasm-bindgen"$' Cargo.lock | grep '^version' | cut -d'"' -f2)" --locked`
- **wasm-opt is optional locally, mandatory in CI.** build-site.sh uses it only
  if present, so a local build may be ~3x larger than the deployed one.
- **Don't edit files with `sed` when the replacement contains `&`** — it
  expands to the whole match and mangles files. Prefer exact-string edit tools
  over stream editing.
- **e2e needs a throwaway `--user-data-dir`**: branded Chrome 136+ silently
  ignores remote debugging on the default profile.
- **Bundle size is settled**: measured analysis found the bulk is product
  (builtins/regex/parser/jaq) with no single melon; size trimming was
  deliberately waved off. Don't reopen it without new evidence. Known open
  correctness question instead: chrono-tz's name table is DCE'd out of the
  browser build, so named timezones are likely broken there (kaish GH #225).
- **`tokio::time` panics on wasm-unknown** — the `sleep` builtin and armed
  request-timeout watchdogs can't run in the browser build.
- Mobile browsers restrict programmatic `focus()` — the playground is
  effectively desktop-only for now.
