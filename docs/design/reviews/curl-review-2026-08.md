# kaish-tools-curl — cross-model design review, 2026-08-14

Two reviews of `docs/curl.md` at commit `2df2d4b`, run **before any HTTP code
existed** (only the crate skeleton and the test harness were built, deliberately
scoped to the parts no review outcome could invalidate).

- **deepseek** (`consult_submit`, cast `deepseek`) — agentic, read the real
  repo, cited `file:line` throughout.
- **gemini-pro** (`batch_submit`, cast `gemini-batch`) — toolless, whole files
  attached (`curl.md`, `git.md`, `issues.md`, `kaish-tools-git/src/tool.rs`,
  `CLAUDE.md`).

No diff was given to either, per the house pattern: a reviewer without one
evaluates the design holistically instead of rubber-stamping a change.

Both briefs weighted security and containment hardest, because that is the
section `curl.md` barely has.

## The headline

**Both reviewers independently said: do not build this as written.** They
converged, without contact, on the same three structural defects. That
convergence is the finding — a single reviewer saying it would be an opinion.

## Where they agreed (highest confidence)

1. **No egress or containment policy at all.** `CurlConfig` carries "tool name,
   defaults, limits" and nothing else. The tool ships always-on arbitrary
   egress: `--unix-socket` to `/var/run/docker.sock`, SSRF to
   `169.254.169.254` and loopback services, `-d @path` reading any VFS-readable
   file and POSTing it anywhere (an exfil primitive over an existing file read),
   `-o <file>` writing any VFS-writable path, and `-L` laundering a permitted
   URL into a forbidden one with no per-hop re-check.

   deepseek framed it against the sibling: the git crate's *entire design
   budget* went to exactly this — a subtractive config unreachable from inside
   the sandbox (`config.rs:3-6`), no `with_verb` so nothing can widen the
   surface past the constructor (`config.rs:137-157`), and a long fight against
   hostile repos (`hostile_repo.rs`, `discovery_ceiling.rs`). curl's resources
   are strictly more dangerous than a repo's and it inherited none of that.

   gemini added the concrete fix: `allow_loopback`, `allow_link_local`,
   `allowed_hosts` on `CurlConfig`, enforced at the ureq transport layer so it
   covers redirects too, and `--unix-socket` routed through
   `ToolCtx::resolve_path` + `backend().resolve_real_path()` rather than
   handing an agent-supplied string to a host OS API.

   **This is not polish.** Adding a policy surface after the crate ships is a
   breaking change to every embedder.

2. **`-s` / `-S` / `--compressed` as accepted no-ops violate the house rule.**
   The doc's own "Decisions" section says every unsupported feature is a
   parse-time refusal and nothing is quietly degraded — then accepts three
   flags that do nothing.

   deepseek found the sharp version: **`-S` exists in curl specifically to
   re-enable error display after `-s` suppressed it.** `-sS` is the standard
   combo. Modelling `-S` as a no-op means `-sS` suppresses errors and never
   restores them — the exact opposite of curl, with nothing in the output
   saying so.

3. **The `xhr.rs` stub in cut 1 is a dual representation.** The git sibling
   ships a `compile_error!` on wasm plus target-scoped deps so a wasm build
   fails fast on one honest message. curl cut 1 would instead *compile* a tool
   whose only backend is a stub. Delete the stub; carry git's `compile_error!`
   until the wasm backend is real.

## Where a reviewer was wrong — and the doc was wrong the same way

gemini's most alarming finding was that synchronous XHR **cannot download
binary data**, because setting `responseType` throws `InvalidAccessError` on a
sync request — which would mean `curl -o data.tar.gz` in the playground
silently corrupts every binary file, and would kill the whole wasm design.

**Checked against the spec (xhr.spec.whatwg.org) rather than reasoned about.**
The responseType setter throws only:

> If the current global object is a `Window` object and this's synchronous is
> true, then throw an "InvalidAccessError"

The playground runs in a **Web Worker**, where the global is not a `Window`.
`responseType = "arraybuffer"` is legal there. The finding does not apply.

The *same* spec sentence governs the `timeout` setter, verbatim — which means
`curl.md`'s own claim, and **CU3 in `docs/issues.md`**, are wrong in the
opposite direction: sync XHR does *not* forbid a timeout in a worker, so
`--max-time` **can** be honored on the wasm path. Both errors come from the
same misreading of one spec sentence, one in each direction.

Still owed as a real probe (reading the spec is not running the code): confirm
in a live worker that `responseType`/`timeout` do not throw and that
`arraybuffer` survives the wasm boundary intact.

## deepseek only (it could read the repo; gemini could not)

4. **Exit code 3 collides with a kernel-reserved code.** `curl.md` maps 3 to
   "URL malformed". `kaish-tools-git/src/error.rs:5-6` states that 3 (output
   spill), 124 (timeout) and 130 (cancel) belong to the kernel and are never
   manufactured by a tool — "a `GitError` can only ever produce 1, 2, 4 or 5".
   An embedder's spill handling would misclassify a malformed URL.
   *Independently found by the agent building the crate skeleton, which is a
   good sign about both.*

5. **The `operations` declaration is not reachable as claimed.** `curl.md` says
   declaring `net.request` / `fs.overwrite` is "correct without a
   `kaish-kernel` dep, the same call `kaish-tools-git` makes". Three problems:
   neither id exists anywhere in the repo; **issue S1 records this exact
   reachability failure**; and git does not make that call — it declares the
   *empty* vector. curl would be the first consumer of a non-empty
   `operations` with no type-safe id. Either declare empty, or open the kaish
   PR moving the constants into `kaish-types` first.

6. **"Governed by the VFS byte budget" and "the kernel output cap" are
   invisible to an out-of-tree tool.** The reachable `ToolCtx` surface is
   `backend, cwd, resolve_path, var, set_var, set_output_format, as_any,
   as_any_mut`. There is no byte-budget and no output-cap accessor — those are
   kernel-side `KernelConfig` fields. The tool cannot "lower ureq's 10 MB
   default to the kernel's output cap" because it cannot see the cap. Related:
   `KernelBackend::write` takes a whole `&[u8]`, so `-o` is buffered or
   chunked-append, not "streams via `Body::as_reader()`" as the doc says.
   Replace the prose with embedder-supplied `CurlConfig` limits.

7. **The `--json` collision cannot be refused, only guessed at.** `--json` is
   one argv token; the tool cannot distinguish "structured output" from curl's
   request-body shorthand. `curl --json http://host` silently becomes a
   structured-output GET where real curl would fail with "no URL", and
   `curl --json @payload.json http://host` gives a confusing bad-scheme error
   rather than the promised literate one. The doc's "an agent does not get a
   silent surprise" is not met for the most natural spellings. State this
   honestly instead of claiming a refusal that cannot fire.

8. **`--data-urlencode` is a grammar, not a line.** curl accepts
   `name=content`, `content`, `@filename`, `name@filename`, and percent-encodes
   only the *value*. The doc's one-liner ("URL-encode the body") licenses
   encoding the whole string, turning `a=b&c=d` into `a%3Db%26c%3Dd`. Silent
   wrong body for the common case.

9. **Redirect credential laundering is unspecified.** curl strips
   user/password on cross-host redirect unless `--location-trusted`. The doc
   neither replicates nor refuses that. A permitted URL that 302s to an
   attacker host exfiltrates the `-u` credential at exit 0 with a
   correct-looking body.

10. **`block_in_place_compat` transplants a bounded-local seam to unbounded
    remote I/O.** On a current-thread runtime it calls `f()` inline, so a
    blocking ureq request freezes the whole embedder for its duration — and
    `--max-time` is opt-in, so the *default* is curl's default of no overall
    timeout. A server that accepts and sends nothing is an unbounded freeze
    with no watchdog able to run. **`--max-time` should be a `CurlConfig`
    default, not opt-in.** (The helper's existing tests only prove it returns
    42; they say nothing about a multi-second network call.)

## gemini only — the agent-ergonomics argument

gemini alone attacked the 80/20 line as modelled on a human developer rather
than an LLM agent. Recorded as **Amy's call**, not adopted:

- **Follow redirects by default**, do not require `-L`. Agents fail to
  navigate 301/302, read an HTML redirect notice, and hallucinate an API
  failure. *Cost: a stated divergence from curl, in a doc whose flag table
  currently claims curl parity.*
- **Implement `--retry` natively.** The doc's literate error tells the agent to
  write a shell `for` loop; agents write brittle loops, and transient 502s are
  common.
- **Drop `-O`.** Agents cannot reliably predict the filename curl derives from
  a URL path, so they lose files in the VFS. Force explicit `-o <file>`.

Also from gemini, mechanical and probably right:

- **`-d` strips CR/LF in real curl**; ureq will not do it for you. Either strip
  manually or admit the divergence — the table currently claims "None".
- **`-i` combined with `-o` writes headers *into the file*** in real curl. The
  doc has `-i` printing to stdout and `-o` writing the body to the VFS, which
  diverges completely.

## What survived

The out-of-tree posture is real and compiler-checked. `GlobalFlags` flattening
is the right way to get kaish's `--json`. `KernelBackend::write` exists, so the
`-o` seam is nameable. A blocking client behind the `block_in_place_compat`
shape is sound as far as it goes.

deepseek's summary is the fair one: the failures concentrate exactly where the
doc **assumed rather than read** — the reachable surface of `ToolCtx`, the
ownership of exit code 3, and a containment model a network egress tool needs
and the doc never specifies.
