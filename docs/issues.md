# Deferred work

In-tree issue tracker: known divergences, limitations, and parked work that is
deliberately out of scope for the PR that discovered it. Entries move out when
fixed (delete the entry in the fixing PR) or when they graduate to a GitHub
issue because someone outside the repo needs the link.

## git status — divergences from git

Found by the PR #23 cross-model review (deepseek-v4-pro C-series); each needs a
fixture asserted against real `git status --porcelain` before fixing.

- **C1 — staged half, file↔dir typechange reports `A` not `T`.** HEAD tree
  entries are dropped early in `stage_the_index` (status.rs), so the
  comparison never sees the old class.
- **C2 — unstaged half, file→dir reports `Typechange`.** git reports ` D` for
  the file plus `??` for the directory's contents.
- **C3 — ignored directory containing tracked files.** Emitted as `!!` and not
  descended; git descends (tracked wins over ignore), reports the tracked
  files' states, and never emits the directory itself.

## git status — known limitations (accepted for the read profile)

- **C5 — config-driven comparison ignored, by design.** `core.autocrlf`,
  `core.fileMode`, `core.ignorecase` are not consulted; hosts relying on them
  can see spurious `M`/`D`. We are hermetic by design — revisit only if
  real-world noise shows up.
- **C6 — submodule dirty content never reported.** Gitlinks compare by
  recorded OID only; a dirty or ahead submodule worktree is invisible (matches
  the intent comment in `unstage_the_worktree`).
- **R3 — verify gix-worktree `Stack` under sparse/skip-worktree.** Confirm the
  ignore `Stack` doesn't persist an id-mapping that misbehaves with sparse
  checkout or skip-worktree bits — needs a fixture or an upstream source check.

## git status — needs a probe

- **G4 — `guard_alternates` may be a 1-bit existence oracle (unconfirmed).**
  A repository with a relative `objects/info/alternates` entry pointing outside
  the mount is refused by `contain` (exit 4) when the target exists. The open
  question is what `gix_odb::alternate::resolve` does when the target does *not*
  exist: git's native behavior is to silently drop a broken alternate, and if
  gix does the same, `contain` is never reached and the store opens (exit 0) —
  the same outside-vs-nonexistent split G2 had. Needs a probe against
  `gix_odb::alternate::resolve` (raised by gemini-pro in the PR #23 re-review;
  behavioral, do not reason it out — build the fixture).

## git log — known limitations (accepted for the read profile)

- **L1 — `--stat` does not detect renames.** A renamed file is counted as one
  deletion plus one addition, so `files` is 2 and the line counts are the whole
  file twice. `status` has exact-match rename pairing and `log` does not,
  because the tree comparison here is per-commit and pairing would need the same
  oid-reappearance pass run against every commit walked. Git's own default
  (`git log --stat` without `-M`) also does not detect renames, so this matches
  the tool an agent is comparing against — but it is a divergence from `status`
  within our own surface, which is the part worth closing. Revisit when PR 5
  (`diff`) factors the rename pairing out of `status`.

- **L2 — a filtered `log` walks history rather than stopping at `--limit`.**
  No filter (`--author`, `--path`, a date window) is sorted along ancestry, so
  the walk cannot stop when it has enough matches — it stops when it has enough
  matches *or* has examined `MAX_COMMITS_EXAMINED` (100k) commits. On a large
  repository an unmatched filter therefore pays a full-history walk and reports
  `truncated: true`. Correct and bounded, but a commit-graph-backed date cutoff
  (`gix-traverse`'s `ByCommitTimeCutoff`) would make the date case much cheaper.
  Not worth the complexity until a real repository makes it hurt.

- **L3 — `--stat` line counts read whole blobs.** Counting lines needs both
  sides of every changed file in memory, bounded per blob by the embedder's
  `max_blob_bytes` but not in aggregate across a commit. A commit touching very
  many just-under-cap files is a large transient allocation. The row cap bounds
  commits, not bytes per commit.

## kaish boundaries — for the write profiles

- **S1 — an out-of-tree tool cannot name kaish's effect ids type-safely.**
  `ToolSchema::operations` (kaish 0.14.0) carries the dotted effect ids a tool
  declares, so an embedder learns `rm` removes a path without recognizing the
  name. The ids themselves live in `KernelOperation` (`kaish-kernel/src/
  operation.rs`), and `kaish-tools-git` deliberately does not depend on
  `kaish-kernel` — that independence is the honest-embedder posture. So when
  the write profiles land we would either hardcode strings that must match the
  kernel's by convention, or ask kaish to move the id constants down into
  `kaish-types` where the out-of-tree contract lives. The second is the kaish PR
  to open, and it is the AGENTS.md pattern exactly: a boundary that does not exist
  yet, fixed upstream rather than worked around.

  **Nothing to do for the read profile** — `operations` is documented as
  "empty for a tool with no destructive effect", and every kaish builtin that
  declares one is destructive (`write`, `mv`, `cp`, `dd`, `rm`, `patch`, `tee`,
  `sed`). A read-only git tool correctly declares none, so today's empty vector
  is the right answer rather than an omission.

## Upstream

- **R4 — unbounded recursion decoding the index cache-tree (gix-index).**
  `gix_index::extension::tree::decode::one_recursive` (gix-index 0.54.0,
  `src/extension/tree/decode.rs:36`) recurses once per subtree of the `TREE`
  extension with no depth bound, so a `.git/index` carrying a deep enough
  cache-tree aborts the process with a stack overflow. Confirmed by probe
  while capping our own tree walk: a 1200-level chain, which `git write-tree`
  produces on request, overflows a 2 MiB thread inside `ReadRepo::open_index`
  — before any kaish-git code sees a tree. Backtrace is ~1200 frames of
  `one_recursive`.

  We cannot close this from here. `gix_index::decode::Options` carries no way
  to skip or bound the extension (`thread_limit`,
  `min_extension_block_in_bytes_for_threading`, `expected_checksum`,
  `alloc_limit_bytes`), and `alloc_limit_bytes` does not help: it bounds the
  per-node subtree allocation, while the hostile shape is one subtree per
  level. Our own `MAX_TREE_DEPTH` bounds `flatten_subtree` and nothing else,
  so `a_tree_deeper_than_the_cap_is_refused` deletes the index to keep its
  assertion pointed at our recursion.

  Next step is a gitoxide issue plus a depth limit in `one_recursive`. Until
  then a hostile index can crash any embedder of this crate.

## curl — deferred (see docs/curl.md)

`kaish-tools-curl` ships native first (kaijutsu); the rest is parked here.

- **CU1 — wasm backend.** The sync-XHR design in `docs/curl.md` is reasoned,
  not probed. Build the one-call wasm probe before the backend; if sync XHR
  does not complete under `block_on`, the wasm path is an async-`execute`
  architecture change, not this design.
- **CU2 — COEP interaction.** `coi-sw.js` stamps `COEP: require-corp` for the
  `SharedArrayBuffer` Ctrl-C path, which blocks cross-origin responses without
  CORP. Probe a real fetch from the cross-origin-isolated worker against
  CORS-only and CORS+CORP endpoints; this decides whether playground curl is
  useful at all.
- **CU3 — `--max-time` / `--connect-timeout` on wasm.** **Corrected 2026-08-14;
  the original claim here was wrong.** It said sync XHR forbids a timeout. The
  XHR spec throws `InvalidAccessError` from the `timeout` setter only "if the
  current global object is a `Window` object and this's synchronous is true"
  (xhr.spec.whatwg.org). The playground runs in a **Web Worker**, where the
  global is not a `Window` — so `timeout` is settable and `--max-time` **can**
  be honored on wasm. The same spec sentence governs `responseType`, so
  `arraybuffer` (binary bodies) is available there too. `tokio::time` still
  panics on `wasm32-unknown`, but XHR's own timeout does not need it.
  Reading the spec is not running the code: confirm both in a live worker as
  part of the CU1 probe before relying on either.
- **CU4 — `-k` / `--insecure` on wasm.** The browser holds the TLS verifier;
  there is no per-request override. Native keeps it via a rustls dangerous
  config.
- **CU5 — features not in the 80/20 cut.** Multiple URLs, `--next`,
  `--parallel`, `--retry`/`--retry-*`, `--resolve`/`--connect-to`, proxies and
  SOCKS, `--cookie`/`--cookie-jar` (a jar), `-F`/`--form` (multipart),
  `-G`/`--get`, `-w`/`--write-out`, `-v`/`--verbose`, `-K`/`--config`,
  `--netrc`, `--abstract-unix-socket`, `--cert`/`--key`/`--cacert`/`--capath`. Each is a
  parse-time refusal with a literate error today (see `docs/curl.md`); graduate
  one to support only when an agent need is real. `--unix-socket` (filesystem)
  moved into the buildout; `--abstract-unix-socket` (Linux abstract namespace)
  stays here as the same transport with abstract addressing.
- **CU6 — curl's `--json` request-body shorthand.** Refused to keep kaish's
  `--json` (structured output) convention universal. Revisit only if the
  idiom `-H Content-Type:application/json --data <body>` proves too much
  friction in practice.
- **CU22 — `-k`/`--insecure` on wasm.** Implemented natively on 2026-08-20
  (`TlsConfig::disable_verification`, gated behind
  `CurlConfig::with_insecure_permitted`). wasm cannot have it at all: the
  browser owns the verifier (CU4). The wasm backend must refuse `-k` even
  where the embedder permitted it.
- **CU7b — `--unix-socket` rides an unstable ureq API.** The transport
  (`src/backend/unix.rs`) is a `Connector`/`Transport` pair reached through
  `ureq::unversioned::transport`, which carries no semver guarantee. ureq is
  pinned for that reason; `tests/unix_socket.rs` is the tripwire. Revisit on
  every ureq minor bump, and expect a 4.x bump to move it.
### Blockers raised by the 2026-08-14 cross-model review

Both reviewers independently said **do not build `docs/curl.md` as written**.
Full findings in `docs/design/reviews/curl-review-2026-08.md`. These have been
resolved and are tracked in the MVP commit history. Resolved entries below for
reference only; they no longer block work.

### Blockers — resolved

- **CU8 — egress policy.** DECIDED: `AllowEgress` trait, embedder-supplied,
  called before initial dispatch. Default deny-by-empty allowlist with opt-in
  loopback/link-local. (Redirect chains bypass this check — ureq has no
  hook for per-hop interception.) Subtractive (no method widens past constructor). See
  git's `GitConfig` pattern.
- **CU9 — unix socket containment.** Route path through `ToolCtx::resolve_path`
  + `backend().resolve_real_path()` and refuse outside the mount.
- **CU10 — cross-host redirect credentials.** Strip user/password on cross-host
  redirect unless `CurlConfig::follow_redirects` is true (the `.curlrc`
  analogue of `--location-trusted`; refused as a flag).
- **CU11 — exit code 3 is kernel-reserved.** DECIDED: malformed URL maps to
  **exit 7** ("could not connect") with a literate error naming the actual
  cause. Kaish note: embedded tools should use an enum to check kaish-specific
  conditions rather than numeric codes, because the kernel shouldn't set a
  fixed policy on exit numbers — that belongs to repl/UI. Filed separately.

- **CU12 — `operations` unreachable type-safely.** DECIDED: hardcode dotted
  strings per Amy's ruling. Declared as of 2026-08-20 —
  `with_operations(["net.request", "fs.overwrite"])`, asserted by
  `schema_declares_what_the_tool_actually_does`. Only `fs.` can drift.
- **CU13 — kernel byte budget invisible.** DECIDED: embedder supplies limits
  via `CurlConfig`. `-o` streams, stdout reads into `max_response_bytes`.
- **CU14 — accepted no-op flags.** DECIDED: refuse `-s`, `-S`, `--compressed`
  at parse time with literate errors. Keeps the door open for later implementation.
- **CU15 — `--max-time` default.** DECIDED: `CurlConfig` carries a default
  (30s). Opt-in override via flag. Prevents silent hang on current-thread runtime.
- **CU16 — `--json` collision.** DECIDED: kaish convention wins. Curl request-body
  `--json` refused with literate error. Document false-negative honestly.
- **CU17 — `--data-urlencode` grammar.** DECIDED: encode only the value after
  first `=`. `@filename` and `name@filename` forms deferred to CU5.
- **CU18 — `-d` stripping and `-i`+`-o`.** DECIDED: match curl for `-i`+`-o`
  (headers go into the file). Diverge on `-d` (no newline stripping); documented.
- **CU19 — delete xhr stub.** DECIDED: carry `compile_error!` on wasm until
  the backend is real. No dual representation.
- **CU20 — agent ergonomics.** Three rulings implemented:
  - `-O` dropped (literate error naming `-o`)
  - Redirects stay opt-in (`-L` flag) with `follow_redirects` config default
  - `--retry` moved in minimally: transient failures with exponential backoff.
    Not retrying non-idempotent methods without stating so.

## curl — from the 2026-08-20 cross-model review (gemini-pro + deepseek)

Both reviewers ran without a diff, on whole files. Findings they reached
independently are marked **(both)**. Every containment claim below was
re-verified against the code, and the two egress bypasses were probed
standalone before being written down.

### Containment — must fix before any embedder registers curl

### Honesty of the declared surface

### Argument binding — the parser is built on the wrong contract

### Limits that are not limits

### Response fidelity

### Agent ergonomics (design lane, gemini)

## Writing style — deferred

- **W1 — `seam` survives in `docs/git.md` and in code comments.** The term rule
  is `boundary`, not `seam` (AGENTS.md, "Writing style"). AGENTS.md,
  `docs/curl.md` and `docs/issues.md` were corrected on 2026-08-20 as the files
  that pass was touching. `docs/git.md` has two, and the `kaish-tools-git` and
  `kaish-tools-curl` sources have several in `//` comments. Groom at the point
  of touch; there is no bulk pass.

## curl — the embedder boundary, still open

- **CU46b — proxy and custom TLS roots.** `CurlConfig` gained injected headers
  on 2026-08-20; the other two the review asked for did not land. An embedder
  behind an egress proxy, or one terminating TLS at an inspection CA, has no
  way to say so, and `--proxy` stays a parse-time refusal. Both are one-line
  passes to ureq (`Config::proxy`, `TlsConfig::root_certs`) — deliberately not
  built until an embedder asks, because the shape of the ask decides whether
  it belongs on `CurlConfig` or on a transport the embedder supplies whole.
