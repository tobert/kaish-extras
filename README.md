# kaish-extras

Companion repo to [kaish](https://github.com/tobert/kaish) — 会sh, the
sandboxed, embeddable shell for agents. kaish core stays lean; the extras
live here:

- **`crates/kaish-web`** — the kaish kernel compiled to
  `wasm32-unknown-unknown` behind a small wasm-bindgen surface
  (`KaishShell`: `execute`, `seed_file`). The whole shell — parser, VFS,
  builtins, jq — runs in the browser tab: no server, no syscalls, no POSIX.
- **`site/`** — the *try kaish* playground built on it: a REPL seeded with
  the kaish and kaish-extras source trees, so you can `grep` the shell's own
  implementation from inside the shell. Published via GitHub Pages.
- *(planned)* tool bundles that were deliberately kept out of kaish core to
  keep its dependency tree light: `kaish-tools-git` (revived — see kaish
  GH #119), jq and ripgrep as add-ons.

## Building the site

```sh
rustup target add wasm32-unknown-unknown
cargo install wasm-bindgen-cli --version <locked wasm-bindgen version>
scripts/build-site.sh ~/src/kaish   # → site/ is a static bundle
python3 -m http.server -d site 8137 # try it locally
```

`site/smoke.html` is the machine-checked page: load it headless and assert
`SMOKE OK` appears in `#out`:

```sh
chromium --headless=new --virtual-time-budget=20000 \
  --dump-dom http://127.0.0.1:8137/smoke.html | grep "SMOKE OK"
```

## kaish dependency

kaish crates are pinned by git rev while this repo finds its feet; before
any release of these crates the pins move to the published crates.io
versions, keeping kaish-extras an honest external embedder of the same API
everyone else gets.
