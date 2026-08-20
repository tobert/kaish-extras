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

- **L4 — `@` is not accepted as a synonym for `HEAD`.** Git takes bare `@`
  wherever it takes `HEAD`. `resolve_commit` does not: `@` is not a valid ref
  name, is not hex, and so exits 1 "does not name a commit". That is a refusal,
  not a wrong answer — a probe over the whole grammar (`@`, `HEAD@`, `^{}`,
  `-`, `HEAD~0`, `HEAD^^^`, `HEAD~2~3`, `HEAD~-1`, `HEAD~1x`, oversized `~N`,
  trailing whitespace, empty) found no input that resolves to a *surprising*
  commit, which is the property that matters. But an agent that types `@` gets
  a confusing error rather than the log it wanted. Either accept it as an alias
  or name it in `unsupported_revspec` so the error says "use HEAD".

- **L3 — `--stat` line counts read whole blobs.** Counting lines needs both
  sides of every changed file in memory, bounded per blob by the embedder's
  `max_blob_bytes` but not in aggregate across a commit. A commit touching very
  many just-under-cap files is a large transient allocation. The row cap bounds
  commits, not bytes per commit.

## kaish seams — for the write profiles

- **S1 — an out-of-tree tool cannot name kaish's effect ids type-safely.**
  `ToolSchema::operations` (kaish 0.14.0) carries the dotted effect ids a tool
  declares, so an embedder learns `rm` removes a path without recognizing the
  name. The ids themselves live in `KernelOperation` (`kaish-kernel/src/
  operation.rs`), and `kaish-tools-git` deliberately does not depend on
  `kaish-kernel` — that independence is the honest-embedder posture. So when
  the write profiles land we would either hardcode strings that must match the
  kernel's by convention, or ask kaish to move the id constants down into
  `kaish-types` where the out-of-tree contract lives. The second is the kaish PR
  to open, and it is the AGENTS.md pattern exactly: a seam that does not exist
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
- **CU7 — `--unix-socket` has no transport.** **Corrected 2026-08-20.** This
  entry used to say the build "implements a `Transport` over
  `std::os::unix::net::UnixStream`". It does not — `backend/ureq.rs` never
  read `Request::unix_socket`, so the flag was parsed and silently ignored,
  and the request went to the URL's host over TCP. The flag is now refused at
  parse time. The design still holds: ureq 3.x has no first-class unix-socket
  connect, so the transport goes through `unversioned::transport`, which
  carries no semver guarantee (pin ureq, revisit on minor bumps), and the path
  routes through `ToolCtx::resolve_path` + `backend().resolve_real_path()` per
  CU9. `tests/unix_socket.rs` and the harness's `UnixGuard` wait on it.
- **CU22 — `-k`/`--insecure` has no implementation.** Same shape as CU7: the
  flag was parsed into `Request::insecure` and never read, so a caller asking
  to skip verification got full verification and no notice. Refused at parse
  time now. Native needs a rustls dangerous-verifier config on the ureq agent;
  wasm cannot have it at all (CU4 — the browser owns the verifier).
- **CU23 — `--max-response-bytes` truncation reads the whole body first.**
  `backend/ureq.rs` reads the full body into memory and *then* compares against
  `Limits::max_response_bytes`, so the cap bounds what is returned, not what is
  allocated. A hostile or mistaken endpoint can still make the embedder buffer
  far more than the limit. Wants a limited reader on the ureq body instead.

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
  strings (`net.request`, `fs.overwrite`) per Amy's ruling. Risk noted at the
  declaration site: only `fs.` can drift.
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

- **CU24 — the egress allowlist is bypassable with URL userinfo. (both)**
  `AllowByList::permit` (config.rs) takes the host by splitting on the first
  `/`, `:` or `?` after `://`; `@` is not a delimiter. So
  `https://allowed.example:443@169.254.169.254/latest/meta-data/` presents
  `allowed.example` to the check and connects to the metadata service. Any
  allowlisted host is a key to anywhere. Probed, confirmed. The fix is to stop
  matching lexically on the URL string and compare against the authority a
  real parser (`http::Uri`, or the `url` crate) resolves — the same one ureq
  will dial.
- **CU25 — `is_in_cidr` is a string prefix test.** `is_in_cidr(host, "127.")`
  means the hostname `127.evil.com` is "loopback" and `169.254.evil.com` is
  "link-local", so either opt-in permits attacker-chosen public hosts. Parse
  as an IP; a name is not an address.
- **CU26 — IPv6 literals cannot be expressed.** `[::1]:8080` splits to `[` on
  the first colon, so the `::1`, `fe80:` and `fd00:` branches are unreachable
  and no IPv6 host can be allowlisted. Fails closed, but silently.
- **CU27 — Basic-auth credentials are not stripped on cross-host redirect.
  (both)** docs/curl.md and CU10 both promise the stripping. There is no
  stripping code in the crate. `-u` plus a redirect is an exfiltration path.
- **CU8 (restated, still live) — the allowlist is checked once. (both)** ureq
  follows redirect hops itself with no per-hop hook, so an allowed host that
  302s anywhere is followed. The `AllowEgress` trait doc claims per-hop
  enforcement it does not get. Options: drive redirects manually (fetch with
  `max_redirects(0)` and loop, checking egress each hop), or a ureq middleware.
  This is the one that makes CU24 catastrophic rather than merely bad.
- **CU28 — the allowlist matches names, the connection resolves DNS.** An
  allowlisted name that resolves to loopback or a metadata address gets
  through, and nothing re-checks after resolution. Classic SSRF; needs a
  resolver-side check, not a parser-side one.

### Honesty of the declared surface

- **CU29 — `Tool::schema` does not describe the real parser. (both)** It
  advertises `-k` and `--unix-socket`, which the parser refuses, and omits
  `-I`, `-H`, `-A`, `-e`, `--max-time`, `--connect-timeout`, `--data-binary`,
  `--data-raw`, `--data-urlencode`, `--url`, which it honors. `help curl`,
  completion, and `tools --json` all lie about the surface. It also declares
  no `operations`, so an embedder gating on declared effects sees a tool that
  makes network requests and writes files as side-effect-free (CU12 chose the
  dotted strings; nothing ever passed them).

### Argument binding — the parser is built on the wrong contract

- **CU30 — curl needs `ToolSchema::with_raw_argv()` and does not set it.
  (both, from different directions)** kaish's binder splits argv into
  `positional` / `flags` (an unordered `HashSet`) / `named`, and renders named
  values as `--key=value`. curl is exactly the position-sensitive case
  `raw_argv` exists for (its own kernel doc names POSIX `test`: "an operand
  that looks like a flag must not be hoisted into the unordered flag set").
  Without it: `--flag=value` forms fall through `args.rs`'s exact-match arms
  and are silently dropped; `curl -d "-i" <url>` hoists `-i` out of the body
  and sets `--include`; `curl -d "-O" <url>` refuses a flag the caller never
  typed; and `trim_argv` re-concatenating buckets puts a flag's value before
  its flag. The unit and functional tests all put every token in `positional`
  in order — which is precisely what `raw_argv` would deliver, so the suite
  passes while the real binding path is broken.
- **CU31 — a missing flag value silently no-ops.** `-o` with nothing after it
  leaves `output_file = None` and the request proceeds. Same for `-X`,
  `--url`, `-u`, `-A`, `-e`, `-H`, `-d`, `--max-redirs`. Only `--max-time` and
  `--connect-timeout` fail. curl says "option requires parameter".
- **CU32 — a second URL and unknown flags are silently ignored.**
  `curl http://a http://b` fetches only the first; docs/curl.md says a second
  URL is refused. `curl --frobnicate <url>` runs a normal GET.
- **CU33 — `find_positional` desyncs on a value beginning with `-`.**
  `curl -d -5 <url>` parses the body, then the second scan treats `-5` as a
  flag, skips the URL with it, and reports "URL is required". Two scan loops
  disagree; `raw_argv` plus one pass removes the second.

### Limits that are not limits

- **CU34 — a flag can raise a ceiling the embedder set. (both)**
  `max_time.unwrap_or(config.limits().max_time)` lets `--max-time 3600`
  override the embedder outright, and `--max-redirs` the same. The `Limits`
  doc says a flag "may only lower it, never raise it". Clamp with `min`.
- **CU35 — truncation is reported as success.** A body over
  `max_response_bytes` is cut and returned `Ok`, and with `-o` that
  already-truncated body is written and reported as "Wrote N bytes" with exit
  0 — a silently corrupt file. Both `config.rs` and docs/curl.md claim `-o`
  streams and is governed by the VFS budget rather than this cap; it does
  neither. Supersedes and widens CU23: bound the read with `.take()`, fail
  loudly at the boundary, and make `-o` actually stream.
- **CU36 — `--max-redirs` is discarded under `RedirectPolicy::Auto`.** Without
  `-L`, `follow` is false, so the caller's cap never reaches the backend and
  the config cap is used instead.

### Response fidelity

- **CU37 — duplicate response headers collapse. (both)** `BTreeMap<String,
  String>` keeps the last of three `Set-Cookie`s, and a non-UTF-8 header value
  becomes an empty string. `-i` is documented to print what the server sent.
- **CU38 — the reported URL is the request URL, not the final one.**
  `model.rs` documents `url` as post-redirect; the backend fills it from
  `req.url`, so `--json` and the `curl.url` baggage name the pre-redirect URL.
- **CU39 — exit 6 is unreachable and exit 60 nearly so. (both)** Nothing ever
  constructs `HostNotFound`; DNS failures land in `Io` → `CouldNotConnect` (7)
  or the catch-all (1). Only `ureq::Error::Pem` maps to 60, so an ordinary
  self-signed-cert failure exits 1, not 60. An agent branching on these codes
  branches wrong.
- **CU40 — `--data-binary` and `--data-raw` get a form-urlencoded
  Content-Type.** Real curl sets it for `-d`/`--data`/`--data-urlencode` only,
  so `--data-binary '{"a":1}'` posts JSON declared as a form.
- **CU41 — stdout mangles binary bodies.** `render_text` runs the body through
  `from_utf8_lossy`, so binary to stdout comes back with U+FFFD; only `-o`
  preserves bytes.
- **CU42 — `Timeout` reports the configured budget as elapsed time**, not the
  time actually spent.

### Agent ergonomics (design lane, gemini)

- **CU43 — reconsider refusing `-s`/`-S`.** Models emit `curl -sSL` reflexively
  from training. The semantic content of `-s` is "no progress meter", and this
  build has none — so accepting it and doing nothing *fulfills* the request
  rather than silently substituting for it, which is not the silent-fallback
  the house rule is about. Refusing costs a failed turn on nearly every first
  attempt. Amy's call; CU14 decided the other way.
- **CU44 — three refusal messages dead-end the agent.** `--resolve` says "DNS
  resolution uses the system resolver" — an agent in a VFS cannot edit
  `/etc/hosts`; point it at `-H 'Host: …'` against the IP instead. `-v` points
  at `--json`, which carries no request-side detail; point it at `-i`. `-k`
  explains the tool's philosophy instead of the way forward.
- **CU45 — `--json` shape.** The body is a double-encoded string, so an agent
  must parse JSON inside JSON; emit a real object for `application/json` and
  base64 for binary. Missing: the redirect chain, timing, and a
  `curl.content_type` baggage entry to branch on before parsing.
- **CU46 — `CurlConfig` cannot inject headers, set a proxy, or add TLS
  roots.** An embedder that wants to supply credentials the agent never sees,
  route through an egress proxy, or trust a MITM inspection CA has no seam.
