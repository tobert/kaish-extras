# kaish-curl — an 80/20 `curl` for kaish

A living design doc for `kaish-tools-curl`, a small `curl`-shaped HTTP tool that
an out-of-tree embedder registers on a kaish kernel. The immediate user is
**kaijutsu** (native); the browser playground (`kaish-web`) is designed in but
**not built yet**. This file is the durable home for the surface, the error
contract, and the wasm story; it defers detail to `docs/issues.md`.

This mirrors the pattern `docs/git.md` set for the git plugin: a deliberately
shallow, safety-first surface, owned outside the kernel, evaluated by
cross-model review before it is built.

## Status

Draft, pre-review. The native backend choice and the wasm feasibility argument
are the parts a kaibo review should pressure-test (see "Wasm" and "Probes
still owed").

## Why

An agent reaches for `curl` constantly: fetch a doc, POST to an API, ping a
health endpoint. kaish core has no HTTP builtin, by the same lean-core instinct
that kept git out. The honest-embedder move is the same one git used: ship it as
an out-of-tree `Tool` against the public `kaish-tool-api` + `kaish-types`
contract, never touching `kaish-kernel`.

The 80/20 is the slice an agent actually types: one URL, GET or POST, a few
headers, a body, maybe basic auth, write to a file or pipe to `jq`. That is
most of `curl` by volume. Everything else is either deferred (multi-URL,
`--retry`, proxies) or refused with a literate error (see "Literate errors").

## Posture

- **Out-of-tree, like git.** `kaish-tools-curl` depends on `kaish-tool-api` and
  `kaish-types` only. It does not depend on `kaish-kernel`. That independence is
  the honest-embedder claim made into a compiler check, the same way
  `kaish-tools-git` proves it.
- **kaijutsu registers it.** kaijutsu already registers custom `Tool` impls on
  its embedded kernel; `kaish-tools-curl` is one `tools.register(...)` plus a
  dep, the same shape as `kaish-tools-git` will be. The version-skew and
  packaging notes that apply to git apply here unchanged (kaijutsu and this
  crate must pin the same kaish version so the `Tool` trait matches).
- **Native now, wasm later.** The native backend is built first because
  kaijutsu is the immediate user. The crate is structured so the wasm backend
  drops in behind the same `Tool`/schema without touching the surface, but the
  wasm path is not built in the first cut.

## The 80/20 surface

Flags mirror `curl(1)` spelling and defaults. One URL is required; a URL is
the last positional unless `--url` is used.

| Flag | kaish-curl behavior | Divergence from curl |
|---|---|---|
| `<url>` (positional) | The URL to fetch. Required. | One URL only (curl takes many). |
| `--url <url>` | Same as the positional. | None. |
| `-X, --request <method>` | HTTP method. | None. |
| `-d, --data <data>` | Body, `Content-Type: application/x-www-form-urlencoded` unless overridden, implies POST. Repeatable, joined with `&`. `@path` reads the file through the VFS. | None. |
| `--data-binary <data>` | Like `-d` but no newline stripping. `@path` reads raw. | None. |
| `--data-raw <data>` | Like `-d` but `@` is literal, never a file read. | None. |
| `--data-urlencode <data>` | URL-encode the body. | None. |
| `-H, --header <h:v>` | Request header. Repeatable. | None. |
| `-i, --include` | Print response headers above the body. | None. |
| `-I, --head` | HEAD; print headers only. | None. |
| `-o, --output <file>` | Write the body to a VFS path instead of stdout. | None. |
| `-O, --remote-name` | Write the body to a file named from the URL path. | None. |
| `-L, --location` | Follow redirects, up to `--max-redirs` (default 50). | **Inverted default**: curl does not follow unless `-L`; this build does not follow unless `-L`. |
| `--max-redirs <n>` | Redirect cap. | None. |
| `-u, --user <user[:pass]>` | Basic auth, `Authorization: Basic`. | None. |
| `-A, --user-agent <ua>` | `User-Agent`. | None. |
| `-e, --referer <url>` | `Referer`. | None. |
| `-k, --insecure` | Skip TLS certificate verification. | Native only; wasm has no per-request override (the browser controls validation). |
| `-f, --fail` | Exit 22 on HTTP status >= 400 instead of printing the body. | None. |
| `-s, --silent` | Accepted; there is no progress meter, so this only suppresses the error stream. | None meaningful. |
| `-S, --show-error` | Accepted; errors are shown by default. | None meaningful. |
| `--compressed` | Accepted; decompression is handled by the backend on both targets (ureq gzip, the browser for wasm). | Effectively a no-op. |
| `--max-time <s>` | Whole-request timeout. | Native only; wasm sync XHR cannot time out. |
| `--connect-timeout <s>` | Connect-phase timeout. | Native only. |
| `--unix-socket <path>` | Connect to an AF_UNIX socket instead of the URL host; the host is a placeholder (e.g. `http://localhost/`). | Native, unix-family targets only; wasm refuses with a literate error. `--abstract-unix-socket` is deferred (CU5). |
| `--json` | **Not curl's request-body shorthand.** It is kaish's global output flag for every tool (see next section). | curl 7.82 `--json` (request body) is refused with a literate error. |

The flags curl has that this build does not carry are listed in "Literate
errors." Each is caught at parse time and refused with a message that names the
unsupported flag and the supported way to do the same thing.

## Output, and the `--json` collision

kaish has a global `--json` flag that means **structured output** on every
tool. `curl` 7.82 added a `--json` that means **a JSON request body**. Same
spelling, opposite meaning. This build keeps kaish's convention intact:

- `curl` flattens `GlobalFlags` like every builtin, so `--json` sets the output
  format. `curl --json <url>` returns a structured object:

  ```json
  {"status": 200, "headers": {"content-type": "application/json"}, "body": "..."}
  ```

- curl's request-body `--json` is **not supported**. An agent who reaches for
  it does not get a silent surprise; the next section's error points at the
  idiom that predates the shorthand and works everywhere.

The rationale is the convention, not the shorthand. kaish's `--json` means
"give me structured output" across the whole toolset; one tool where it means
input is the one crack in that rule. Send a JSON body with `-H
Content-Type:application/json --data <body>`. Parse a JSON response with
`curl <url> | jq`.

## Exit codes

kaish-curl mirrors curl's numeric exit codes for the cases it covers, and maps
the rest to 1. State the number so an agent can branch on it.

| Code | Cause |
|---|---|
| 0 | Success (any HTTP status unless `--fail`). |
| 3 | URL malformed (bad scheme, bad syntax). |
| 6 | Host not found. |
| 7 | Could not connect. |
| 22 | `--fail` and HTTP status >= 400. |
| 28 | `--max-time` or `--connect-timeout` exceeded. |
| 35 | TLS handshake failed. |
| 47 | Too many redirects. |
| 60 | Certificate could not be authenticated. |
| 1 | Any other transport error. |

## Literate errors

The flags and features this build does not carry are caught at parse or URL
time and refused with a message that names the unsupported thing and the
supported alternative. These strings are full weight (see
`~/src/kaish/docs/style.md`): the constraint comes first, the fix follows, and
they never name a mechanism the reader cannot act on.

Non-http scheme:

```
curl: 'ftp://example.com/' uses scheme 'ftp', which this build does not support.
kaish-tools-curl speaks http and https only. Read a local file with 'cat <path>'.
```

curl's `--json` request body (triggered by the URL-parse path when the first
positional looks like a JSON object or array):

```
curl: '{"a":1}' is not a valid http or https URL; it looks like a request body.
Put the body in '--data <body>' and the URL last, or send JSON with
'-H Content-Type:application/json --data <body>'.
```

Proxy and SOCKS:

```
curl: '--proxy' is not supported. kaish-tools-curl connects directly and has
no proxy or SOCKS support.
```

Multipart:

```
curl: '--form' (multipart) is not supported in this build. Use '--data' for
application/x-www-form-urlencoded.
```

Force GET with query:

```
curl: '--get' is not supported. Put the query string in the URL, or build one
with '--data-urlencode'.
```

Cookies:

```
curl: '--cookie' is not supported. Send cookies with '-H Cookie:<value>'.
```

Multiple URLs, `--next`, `--parallel`:

```
curl: only one URL per invocation is supported. Call curl once for each URL.
```

`--write-out`, `--verbose`:

```
curl: '--write-out' is not supported. Use kaish '--json' for a structured
response object, or '-i' to print headers above the body.
```

Retry:

```
curl: '--retry' is not supported. Retry from the shell, for example:
'for i in 1 2 3; do curl <url> && break; sleep 1; end'.
```

Client certificate and CA control (verification skip is `-k`, not these):

```
curl: '--cert' is not supported. kaish-tools-curl uses the system trust store
and has no client-certificate support in this build.
```

DNS override:

```
curl: '--resolve' is not supported. kaish-tools-curl does not override DNS.
```

Config file:

```
curl: '--config' is not supported. Pass flags on the command line.
```

Netrc:

```
curl: '--netrc' is not supported. Put credentials in '--user <user[:pass]>'.
```

Unix sockets on wasm (native supports `--unix-socket`; the browser has no
filesystem sockets):

```
curl: '--unix-socket' is not supported in the browser build. It is a
native-only flag; use it in a kaish native embedder, not the playground.
```

A bare rule with no equivalent stays bare and honest; no invented substitute.

## Native backend: ureq

Probed against the published crate and the source at `~/src/research/ureq`
(ureq 3.4.0, the crates.io default as of 2026-08-08).

- **Sync, no async runtime, no subprocess.** ureq is a blocking client. It
  runs inside `block_in_place_compat` on the kernel's tokio runtime, exactly
  the seam `kaish-tools-git` uses for blocking gix. No embedder IO-driver or
  multi-thread requirement: `block_in_place_compat` picks the right path for a
  current-thread or multi-thread runtime. This is the reason ureq beats an
  async client here: it adds no runtime constraint on the embedder.
- **TLS without a C toolchain, the kaibo tradeoff.** The crate pins ureq with
  `default-features = true`, which is `rustls` + `gzip`. ureq's `rustls`
  feature resolves to `rustls-no-provider` + `_ring` + `rustls-webpki-roots`,
  so the crypto provider is **ring**. `aws-lc-rs` (cmake) and `openssl-sys` are
  absent unless the `native-tls` feature is turned on, which this build never
  does. ring ships its own C/asm shims, which need a C compiler but not cmake;
  that is the deliberate ring-vs-aws-lc tradeoff kaibo already accepted. A CI
  `cargo tree -i` tripwire asserts `aws-lc-sys`, `openssl-sys`, and `curl` stay
  out, the same guard the git crate keeps.
- **Redirects are inverted.** ureq follows redirects up to `max_redirects` by
  default; curl does not follow unless `-L`. The build sets `max_redirects = 0`
  unless `-L` is given, then lifts it to `--max-redirs` (default 50). ureq's
  `max_redirects_will_error` controls whether an over-cap is an error (curl's
  default) or a last-response return; this build errors, to match curl.
- **Timeouts map directly.** `--max-time` is `timeout_global(Some(d))`;
  `--connect-timeout` is `timeout_connect(Some(d))`. Native only; the wasm
  backend cannot honor these and says so.
- **`-k` is a rustls dangerous config.** ureq 3.x has no public `insecure()`
  switch; skipping verification means building a rustls `ClientConfig` with a
  no-op verifier through `rustls::client::danger`. It is a small, contained
  helper, not a one-liner; it is the one place the native backend spends a
  little extra. It never builds for wasm, where the browser holds the verifier.
- **`--unix-socket` is a custom transport.** ureq has no first-class
  unix-socket connect (its `UnixStream` use is for reading a request body, not
  connecting to one). The build adds a small `Transport` impl over
  `std::os::unix::net::UnixStream` through ureq's **unstable**
  `unversioned::transport` API (`Transport`/`Connector`), the one place this
  build takes an unstable dependency, tracked in `docs/issues.md` (CU7). The
  URL host is a placeholder when `--unix-socket` is set, exactly as curl does
  it. `--abstract-unix-socket` (Linux abstract namespace) reuses the same
  transport with abstract addressing; it is deferred to CU5.
- **Body read respects the budget.** `curl -o <file>` streams via
  `Body::as_reader()` and writes through `ToolCtx::backend().write`, so the
  embedder's VFS byte budget governs the download. `curl <url>` to stdout reads
  into the `ExecResult` and is subject to the kernel output limits (and the
  1 MB wasm clip). ureq's own 10 MB `read_to_string` default is lowered to the
  kernel's output cap so a stdout fetch cannot blow the budget.
- **Errors map to exit codes.** ureq's `ErrorKind` lines up with the table
  above: `HostNotFound` is 6, `ConnectionFailed` and connect `Io` are 7,
  `Timeout` is 28, `Tls` is 35, `Rustls`/`Pem` cert failures are 60,
  `TooManyRedirects` is 47, `BadUri` is 3, and `StatusCode` under `--fail` is
  22. Everything else is 1.

## Wasm: designed in, not built

The playground (`kaish-web`) targets `wasm32-unknown-unknown`. The crate is
shaped so a wasm backend slots in behind the same `Tool`/schema, but the first
cut is native only.

The hard part is that a browser `fetch()` returns a `Promise`, and the kernel
runs each tool's `execute` future inside `rt.block_on` on a current-thread
tokio runtime (`kaish-kernel` awaits the tool inline). The whole playground
model rests on in-memory I/O that resolves without the JS event loop. A
`fetch()` Promise is driven by that same event loop, which `block_on` is
holding; the two deadlock. `tokio::time` also panics on `wasm32-unknown`, so
no watchdog can rescue it.

The wasm path is therefore **synchronous `XMLHttpRequest`** in the worker.
Sync XHR blocks the thread until the response arrives, so it completes inside
the async tool future and is polled to `Ready` immediately by `block_on`, no
event-loop reentry, no worker-protocol change. Sync XHR is allowed in Web
Workers (the main thread forbids it), and the playground runs in a worker. It
is not a hack grafted on; it is the choice that preserves the "futures resolve
without external events" invariant the wasm build already depends on.

The wasm backend is `cfg(target_family = "wasm")` over `web-sys`'s
`XmlHttpRequest`; the native backend is `cfg(not(target_family = "wasm"))` over
ureq. Args, schema, model, and render are shared, so `tools --json`,
completion, and `help curl` are identical across targets. Unlike the git crate
there is no `compile_error!` on wasm; curl is meant to be the first
cross-target tool.

### Wasm limits, stated up front

- **CORS and COEP gate every fetch.** `site/coi-sw.js` stamps
  `Cross-Origin-Embedder-Policy: require-corp` for the `SharedArrayBuffer`
  Ctrl-C path. COEP blocks cross-origin responses without a
  `Cross-Origin-Resource-Policy` header, and forbids `no-cors`. The playground
  can therefore only reach endpoints that send CORS and CORP headers, which is
  a narrower set than CORS alone. This is the interaction most likely to cap
  the playground's curl, and it is the first thing to probe before building.
- **No `--max-time` or `--connect-timeout`.** Sync XHR forbids setting a
  timeout (it throws), and `tokio::time` is unavailable. A slow server stalls
  the worker until the browser's own network timeout fires.
- **No preemption.** Ctrl-C tier-2 polls the interrupt flag between commands,
  not during a blocking sync XHR. A hung request hangs the tab; the browser
  network timeout is the only backstop. Tier-1 (terminate + respawn) is the
  recovery.
- **No client certs, no proxy, no cookie jar, no non-http(s) schemes.**
- **No unix sockets.** The browser has no filesystem sockets; `--unix-socket`
  is refused with a literate error (see "Literate errors").
- **Body clipped at ~1 MB** at the wasm boundary, and subject to kernel
  output limits.

## Crate shape

Modeled on `kaish-tools-git`, minus the git-specific containment and plumbing.

```
crates/kaish-tools-curl/
  Cargo.toml
  src/
    lib.rs          # Tool impl, argv routing, cfg split; compile_error! only if both backends are off
    config.rs       # CurlConfig (embedder-supplied: tool name, defaults, limits)
    error.rs        # CurlError -> exit code mapping, literate messages
    model.rs        # Response model (status, headers, body, url)
    render.rs       # text (--include/--head) and the --json object
    args.rs         # the clap struct, flattened GlobalFlags, the unsupported-flag refusals
    backend/
      mod.rs        # trait Backend { fn fetch(...) -> Response }
      ureq.rs       # cfg(not(wasm))
      xhr.rs        # cfg(wasm) — stubbed in cut 1, built later
  tests/
    support.rs      # the local server harness: 127.0.0.1 TCP + a unix-domain socket
    surface.rs       # GET/POST/headers/-i/-o/-L/--fail/exit codes over TCP
    unix_socket.rs   # the same surface over a unix-domain socket (--unix-socket)
    errors.rs        # the literate-error catalog, asserted verbatim
    tls.rs          # -k against a self-signed cert (optional fourth)
```

`CurlConfig` mirrors `GitConfig`: embedder-supplied, with a `tool_name` for
embedders who want a different name. The `Tool` impl declares `operations`:
`net.request` always, plus `fs.overwrite` when `-o`/`-O` is enabled. The ids
are free-form dotted strings (kaish `ToolSchema::operations`), so this is
correct without a `kaish-kernel` dep, the same call `kaish-tools-git` makes
for the read profile.

## Functional tests

The plan is to build **three or more** functional tests, each one standing up
a local server and curling it. The harness stays dep-light: a hand-rolled
`TcpListener` (and `std::os::unix::net::UnixListener`) responder in
`support.rs`, the way the git tests hand-roll fixtures, so no server framework
enters the published crate's dependency tree.

1. **Core surface over `127.0.0.1` TCP.** GET, POST with `-d`, custom `-H`,
   `-i`, `-I`, `-o`, `-L` follow, `--fail` exit 22, and every exit code in the
   table.
2. **The same surface over a unix-domain socket.** `--unix-socket <path>`
   against a `UnixListener` responder, including the placeholder-host URL form.
3. **The literate-error catalog.** Each unsupported flag hits the server-bound
   path and asserts the exact error string and exit code, so the messages an
   agent reads are the ones the doc promises.
4. **`-k` against a self-signed cert (optional).** A minimal rustls server with
   a self-signed certificate, asserted to succeed under `-k` and fail without
   it.

The wasm backend gets its own harness later (a worker that fetches a local
URL), gated on the two probes in "Probes still owed."

## Decisions and conventions

- **Tool named `curl`**, shadows any external `curl`; `with_tool_name` for
  embedders who want their own spelling. Same call git makes.
- **One URL.** Multiple URLs, `--next`, `--parallel` are refused with one
  error.
- **`-L` is opt-in**, matching curl, by inverting ureq's default.
- **`--json` is kaish's output flag**, not curl's request-body shorthand.
- **Unix sockets in.** `--unix-socket` is built native-only via a custom ureq
  transport; `--abstract-unix-socket` is deferred (CU5).
- **Exit codes mirror curl** for the cases covered, 1 for the rest.
- **No silent fallbacks.** Every unsupported curl feature is a parse-time
  refusal with a literate message; nothing is quietly dropped or degraded.
  This is the house rule `kaish-tools-git` keeps and `style.md` states.
- **No dual representations.** One response model; text and `--json` are two
  renders of it.
- **Deferrals live in `docs/issues.md`**, not inline TODOs.

## Effort

The native backend is small: a clap struct, a config, a ureq-backed
`execute`, the render, and the error catalog. ureq is probed, the exit-code
map is settled, and the `block_in_place_compat` seam is already proven by the
git crate. The added work over a bare 80/20 is `--unix-socket` (a custom
transport on ureq's unstable API) and the functional-test harness. Two or
three focused days for the native tool plus the three functional tests. The
wasm backend is a later follow-up; its uncertainty is concentrated in the two
probes below, not in the code.

## Probes still owed

Two claims in this doc are reasoned, not probed. The kaibo review and the
build should settle them empirically, the way `docs/git.md` settled gix.

- **Sync XHR completes under `block_on`.** The argument is structural (sync
  XHR blocks the thread; the future returns `Ready`). Build a one-call wasm
  probe that fetches a local URL through a worker before committing to the
  full backend. If it deadlocks, the wasm path is the bigger architecture
  change (async `execute`), not this doc.
- **COEP blocks the endpoints we care about.** Probe a real cross-origin fetch
  from the playground's cross-origin-isolated worker against a CORS-only and a
  CORS+CORP endpoint, and record which survives. This decides how useful
  playground curl is at all.

## Open in `docs/issues.md`

The deferred curl features and the wasm build are tracked there; this doc
links to them rather than listing them inline a second time.
