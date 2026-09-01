# AGENTS.md

Orientation for AI agents working in this repo. `CLAUDE.md` is a symlink here.
Architecture and build docs live in [README.md](README.md) — read that first;
this file is only what you need beyond it to work effectively.

The one principle that shapes everything: this repo is an **honest external
embedder** of kaish. `kaish-web` consumes the same public API any embedder gets.
When it needs a boundary that doesn't exist, the fix is a kaish PR (same
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

`[workspace.dependencies]` in the root `Cargo.toml` pins all four kaish crates
to **published crates.io versions** (`"0.17"` as of 2026-09-01). Rules:

- **All four must pin the SAME version** — mixing them puts two copies of a
  kaish crate in the dependency graph.
- **The pin is a caret range, and downstream depends on that.** `"0.17"` is
  `^0.17`: any 0.17.x patch resolves, so an embedder that consumes both a tool
  crate from here and kaish directly (kaijutsu) can move to 0.17.1 and still
  unify on one copy of each kaish crate. 0.18 cannot ride along — pre-1.0 a
  minor carries breaking changes — so a kaish minor is a bump here that has to
  build and pass the checks. Do not widen the range to buy the illusion.
- **A two-minor range makes the two-copies failure MORE reachable, not less.**
  Measured with cargo and the real crates on 2026-09-01, three resolutions of
  one graph where an embedder depends on a tool crate from here:

  | tool crate asks | embedder asks | cargo resolves |
  |---|---|---|
  | `">=0.16, <0.18"` | `^0.16` | **0.16.0 and 0.17.0 — two copies** |
  | `">=0.16, <0.18"` | `^0.17` | 0.17.0, one copy |
  | `"0.16"` | `^0.16` | 0.16.0, one copy (the control) |

  Cargo does not unify across compatibility groups: pre-1.0 it treats 0.16.x
  and 0.17.x as incompatible, so a requirement spanning both is satisfied
  inside *either* group and the resolver takes the newest. The range therefore
  splits the graph exactly when the embedder is on the **lower** minor — which
  is the embedder widening was supposed to help. The one it does help is
  already on the newer minor and needed nothing. `kaish-types` splitting is
  fatal rather than wasteful: our `Tool` impl stops satisfying the embedder's
  kernel `Tool`, which is a trait mismatch, not a duplicate-symbol warning.

  So the reason to keep a tight pin is not caution about untested versions —
  it is that a tight pin **forces both sides onto one version together**, and
  that coordinated move is the thing that actually fixes the embedder.
  kaijutsu-lead named this from downstream before it was measured here, having
  just been broken by the 0.16→0.17 skew.

  Amy's call on 2026-09-01 was to revisit at publish; the measurement above is
  what that revisit should start from. If a range is ever wanted anyway, the CI
  job that resolves and tests the **low** end is not garnish — the bad state
  compiles clean in this repo and only fails in the embedder's.
- **A kaish minor can change behavior without failing to compile.** 0.16 built
  clean here on the first try with every test binary green, and its changelog
  still carried a dozen **Changed** entries no compiler could have caught. 0.17
  compiled clean too and broke one test — `help <tool>` grew a `Subcommands:`
  roster with every flag's description under it, so `router_kernel_drift`'s
  whole-document substring needle started matching `git info` inside `git diff
  --patch`'s prose. Read the entries and decide each one against this
  workspace; check the answer at the source rather than reasoning from the
  entry. The 0.16 entry that looked like it moved `$(git …)` turned out not to
  — see "Command substitution binds text".
- **`help <tool>` renders one level of subcommands, not two.** Since 0.17 an
  agent reading `help git` sees `worktree — Work with the repository's working
  trees` and never learns that `list` is the verb under it. Every one-word verb
  is named with its flags; `worktree list` is reachable only from the
  `Examples:` block. Tracked as K1 in [docs/issues.md](docs/issues.md).
- `kaish-kernel` is `default-features = false`. Keep it that way: a sibling
  crate enabling kernel default features tramples the no-default choice
  (`localfs` etc. must not leak into the browser build).
- **A published version is the default state, not a git rev.** Depending on a
  rev while claiming to be an ordinary consumer is the one place the
  honest-embedder posture is weaker than it looks: a rev is an affordance no
  real embedder has. Go back to one only while developing against an unreleased
  kaish boundary, and come back off it.
- Workflow for changes that need a kaish-side boundary: open a kaish PR, pin all
  four here to the PR branch head to develop against it, then move back to the
  published version once the release carrying it lands — not to the merged main
  sha, unless the wait is genuinely blocking.

### Command substitution binds text

`x=$(git status)` binds the **text** git printed, not its typed model, and the
same holds for curl. Two independent things make that true, and a change to
either one flips it:

- **Neither crate sets `ExecResult.data`.** Both build results with
  `with_output`/`with_output_and_text`, which take an `OutputData` and leave
  `data: None`. A substitution binds `.data`, so there is nothing for it to
  bind. Switching a verb to `success_with_data` — a reasonable thing to want
  for the pipeline sideband — would change that.
- **Both leave `ToolSchema::typed_substitution` at its default `false`.** kaish
  0.16 stopped inferring the answer from "does the tool set `.data`" (an
  inference that bound a list for `cut -f2 f` and text for `awk '{print $2}'
  f`, the same job). The rule now: declare `with_typed_substitution()` when the
  structured thing IS the answer (`fromjson`, `jq`, `keys`); leave it off when
  `.data` is a structured view of text the tool already printed. Both crates
  are the second kind — `git` and `curl` name real programs, and an agent that
  wants the model asks with `--json`.

**The 0.16 bump did not move this**, though it reads like it should have: with
`.data` never set, these crates bound text under 0.15 too. That was checked at
the source rather than reasoned from the changelog, after the first version of
this section asserted a behavior change that had not happened.

`tests/kaish_behavior_canary.rs` pins the observable half. It goes red on the
two-part mutation above — the schema flag alone is inert, which is itself the
thing worth knowing.

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

## Writing style

kaish's guidance lives in [its `AGENTS.md`, "Writing style"](https://github.com/tobert/kaish/blob/main/AGENTS.md#writing-style),
and that file is the source when the two disagree. It used to live in
`docs/style.md`, which this section pointed at until 2026-08-20 — the file was
gone and the pointer was still here, so the rules below are carried in this
repo rather than referenced out of it. An agent working in kaish-extras may
have no kaish checkout at all.

The weight map for this repo's files:

- **Full weight**: every tool `description`, argument doc, and error or
  diagnostic string a crate in this workspace returns. An agent reads an error
  more often than it reads any help topic, and for an embedded tool the schema
  is most of what it will ever read.
- **Terms only**: `README.md` and the docs under `docs/`.
- **Exempt**: `signoff.md` — it tells a story from a point of view.

Groom at the point of touch; there is no bulk pass.

### The rules

**Keep the vocabulary small.** This limits the number of distinct words, not
the length of the text. Plain words instead of figures of speech, so the
meaning is available from the words themselves. American spelling.

**One term, one meaning.** Pick one word per concept and keep it; do not vary
for style. Write `boundary`, not `seam`. `surface` can hide the thing it names
— in published text, name the tool schema, the error message, the help topic,
or the API.

**Provide specific values.** Give the exit code, size, flag, default, and
condition wherever it is practical. This saves an agent a round trip and gives
it something to update its model of the world with.

> Before: Oversize output fails.
>
> After: Oversize output spills to a file and exits 3.

**Fast and informative failures.** Lead with the consequence, name the
condition, and suggest the next step when it is known. User- and agent-facing
errors must not leak internals — an internal name is unresolvable to the
reader, and belongs only in an assertion about a real bug.

**Published text is published.** A `///` on a clap argument is copied into
`ParamSchema.description` and reaches agents through the tool schema.
Behavior goes there; mechanism goes in a `//` comment. A `///` on the clap
*struct* is not published — `schema_from_clap` reads `cmd.get_about()`
instead. Do not infer the published text by grepping: read the schema.

**Example labels are imperative.** They sit next to a command, so they should
read like one: "Save the body to a file", not "File output".

**The example is the rule.** Show the correct form first and let it carry the
rule by itself. Avoid incorrect examples; when one is necessary, put the
correct form first and mark the error clearly.

**Cross-references take one form per target**: ``see `help <topic>` `` for a
help topic, and `docs/curl.md`, "Section name" for a document.

**Write for model context.** Use the same prose for humans and models, assume
the context may be truncated, teach syntax with examples, and repeat a rule in
the error that enforces it.

## Conventions

Only the house rules that aren't standard practice:

- Never silently discard errors. If an error is deliberately ignored, the
  narrowest case is matched and a comment says so (see `seed_file`: only
  `AlreadyExists` is swallowed).
- No legacy dual-representations — delete superseded code immediately, no
  compat shims or parallel old/new paths.
- Defer out-of-scope work to [docs/issues.md](docs/issues.md), not inline
  TODOs or scratch notes. An entry graduates to a GitHub issue only when
  someone outside the repo needs the link; delete entries in the PR that
  fixes them.
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
