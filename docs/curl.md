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

**Building MVP.** The cross-model review blockers have been resolved:

1. **Egress policy — `AllowEgress` trait (CU8).** An embedder-supplied trait
   the tool calls per-request-per-hop. Default: deny-by-empty allowlist with
   opt-in loopback/link-local. No method widens the set past what the
   constructor grants — subtractive like `GitConfig`.
2. **Exit codes (CU11).** Malformed URL now maps to **exit 7** ("could not
   connect") with a literate error naming the real cause. The kernel does not
   own a fixed set of numeric codes; that's repl policy. See kaish note below.
3. **Literate-refusal posture (CU14–CU18).** Unsupported flags are parsed
   at construction and refused with clear messages — no silent no-ops.
   This keeps the door open for later implementation when need arises.

Built so far: the crate skeleton, test harness, and `CurlConfig` with egress
and redirect controls. The HTTP surface (argument parsing, ureq backend, Tool
impl, renderers) follows.

The wasm section is **corrected** — see CU3. Sync XHR in a Web Worker permits
both `timeout` and `responseType`, so the limits asserted there are overstated
in this draft.

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
| `-d, --data <data>` | Body, `Content-Type: application/x-www-form-urlencoded` unless overridden, implies POST. Repeatable, joined with `&`. `@path` reads the file through the shell's filesystem (`ToolCtx::resolve_path` + `backend().read()`, never the host) and removes **every** CR and LF byte from it, as curl does. A file that cannot be read is exit 26 with no request. A body file over `Limits::max_response_bytes` (checked by `stat` before the read, and again on the joined body) is exit 1 with no request. | `@-` (stdin) is refused with exit 2: `ToolCtx` has no stdin (docs/issues.md CU47). A bare `-` is the literal body `-`, as in curl. |
| `--data-binary <data>` | Like `-d`, and `@path` keeps every byte of the file. The Content-Type is the same form type as `-d`: curl 8.23 sends it for this flag too (an earlier version of this table said it did not; the oracle test in `tests/features.rs` disproves that). | `@-` (stdin) is refused like `-d @-`. A bare `-` is the literal body `-`, as in curl. |
| `--data-raw <data>` | Like `-d` with `@` literal: `--data-raw @f` sends the four characters `@f`. Same form Content-Type. | None. |
| `--data-urlencode <data>` | curl's grammar: `content`, `=content`, `name=content`, `@path`, `name@path`. The first `=` or `@` decides. Only the content is percent-encoded (a space is `+`, as in curl) and the name is sent as typed. The file forms encode the file's bytes, newlines included (`%0A`). | `@-` (stdin) is refused like `-d @-`. |
| `-G, --get` | Sends the `-d`, `--data-binary`, `--data-raw` and `--data-urlencode` data on the URL's query string, joined with `&` and added after any query already there. The method stays GET (or `-X`, or HEAD with `-I`), no body and no Content-Type are sent. Data a URL cannot carry (a space, a control byte, `#`, non-ASCII) exits 2 with no request. | curl exits 3 for such data, and this build exits 2 and names `--data-urlencode`. A request with no data flag now sends no `Content-Length`, as curl does (it sent `0`). |
| `-F, --form <name=content>` | One multipart/form-data part, repeatable, implies POST. `name=text`; `name=@path` sends the file with its basename as `filename` and a Content-Type from the name shown (`gif jpg jpeg png svg txt htm html pdf xml`, any case; otherwise `application/octet-stream`); `name=<path` sends the file's bytes as a plain value; `;type=MIME` and `;filename=NAME` follow, and a value in double quotes may hold a `;` (`\"` and `\\` inside quotes). A text part given `;filename=` is sent as a file part. Names and filenames escape `"` as `%22`. The boundary is random per request and checked against every part, so no file can contain it. A `Content-Type` set with `-H` wins, and gains `; boundary=` if it names a multipart type without one. Files are read through the VFS inside `Limits::max_response_bytes`, and the finished body is checked against it too. A file that cannot be read, or a directory, is exit 26. | One file per part: `@a,b` (curl sends nested multipart/mixed) exits 2. `;headers=`, `;encoder=`, `;format=`, an unknown `;key=`, a repeated or empty `;type=`, an empty field name, and stdin (`@-`, `<-`) exit 2, where curl ignores or accepts them. `-F` with a data flag, `-G`, or `-I` exits 2 (curl accepts `-G` and ignores it). A directory is exit 26 (curl sends something). The boundary text differs from curl's. Compared part by part against curl 8.23 in `tests/features.rs`. |
| `-T, --upload-file <file>` | The file's bytes are the body, sent with PUT unless `-X` says otherwise, with a `Content-Length` and no `Content-Type` of its own (set one with `-H`). A URL whose path ends in `/` and has no query gains the file's basename (`-T sub/a.txt http://h/u/` is `PUT /u/a.txt`; `http://h` is `PUT /a.txt`). Read through the VFS inside `Limits::max_response_bytes`; an unreadable file or a directory is exit 26. An empty file is sent as `Content-Length: 0`. | One `-T` per invocation. `-T -` (stdin) and `-T` with `-G` exit 2 (curl ignores `-G`); with `-d`, `-F` or `-I` curl exits 2 as well. The upload is held in memory, not streamed. |
| `-C, --continue-at -` | Resume the `-o` / `-O` file: sends `Range: bytes=<size>-` for a file that exists and is not empty (no `Range`, and a normal download, for one that is missing or empty), and appends the body through `backend().append()` **only** for a 206 whose `Content-Range` starts at `<size>`. A 416 is exit 0 with the file unchanged, even under `-f` (curl treats it as already complete). A 200 (the server ignored Range) is exit 33, and so is any other status, or a 206 for different bytes; the file is left as it was in every one of these. Needs `-o` or `-O`. | Only `-C -`; `-C <offset>` exits 2. `-C -` without `-o` / `-O` exits 2 (curl starts over without saying so), as do `-i`, `-I` and `-H Range: ...` with it. A non-200 error status exits 33 where curl would append the error page. Compared against curl 8.23 for the 206, 200, 416, missing and empty cases in `tests/features.rs`. |
| `-H, --header <h:v>` | Request header. Repeatable. | None. |
| `-i, --include` | Print response headers above the body, starting with `HTTP/1.1 <code> <reason>` (the standard phrase for the code). With `-o <file>`, writes headers **into the file** alongside the body, matching curl's behavior. | None. |
| `-I, --head` | HEAD; print headers only. With `-o <file>`, the headers are what the file holds. | None. |
| `-o, --output <file>` | Write the body to a VFS path, through `ToolCtx::resolve_path` + `backend().write()` — never the host filesystem. Headers go into the same file if `-i` is also given (or alone, with `-I`), and the body is written byte for byte. `-o -` writes to stdout, not to a file named `-`. The whole body is held in memory before it is written, inside `Limits::max_response_bytes`; nothing streams. Prints nothing on stdout, as curl does (this build has no progress meter to print); the byte count and path are in the result's baggage as `curl.bytes` and `curl.file`. | None. |
| `-O, --remote-name` | Write the body to a file named after the **last non-empty segment of the URL path** (query and fragment excluded; not percent-decoded, as in curl 8.23: `c%20d.txt` stays that) in the shell's cwd, replacing a file of that name, through the same VFS write as `-o`. The name comes from the URL as typed, not from a redirect target. Prints nothing on stdout. A URL with no such segment (`http://h/`) exits 2 with no request. `-O` with `-o` exits 2. | curl falls back to the name `curl_response` for a URL with no segment; this build refuses (Amy's ruling, 2026-11-06). `-J` / `--remote-header-name` stays refused: a server must not choose the file name. |
| `--output-dir <dir>` | Puts the `-o` or `-O` file under `<dir>`, which must exist and be a directory: otherwise exit 23, naming the path. Without `-o` or `-O` it exits 2. | curl ignores it without `-o`/`-O`. |
| `-L, --location` | Follow redirects, up to `--max-redirs` (default 50). Hops are driven by this build, not by ureq, so **the egress allowlist is evaluated on every one** — a redirect cannot leave the embedder's policy. User/password are dropped on a cross-host hop. | Opt-in (matches curl's `-L`). Embedder can set a config default for auto-follow. |
| `--max-redirs <n>` | Redirect cap. | None. |
| `-u, --user <user[:pass]>` | Basic auth, `Authorization: Basic`. A `-H Authorization:` from the caller wins over it, and an embedder-injected `Authorization` wins over both; one header goes out. | None. |
| `-A, --user-agent <ua>` | `User-Agent`. | None. |
| `-e, --referer <url>` | `Referer`. | None. |
| `-k, --insecure` | Skip TLS certificate verification. **Off unless the embedder sets `CurlConfig::with_insecure_permitted(true)`**, and refused with a literate error while it is off — an agent that can silence verification can be talked into it. `help curl` lists the flag only where it is permitted. | Gated by the embedder rather than always available. |
| `-f, --fail` | Exit on HTTP status >= 400 instead of printing the body. Uses curl-compatible exit codes. | None. |
| `--fail-with-body` | Like `-f`: exit 22 on status >= 400 (stderr `curl: the requested URL returned error: 404`), and the body prints to stdout, or goes to the `-o` file. | Real curl's stderr has a `(22)` in it. |
| `-w, --write-out <fmt>` | Text printed on stdout after the body. Variables: `%{http_code}`, `%{response_code}`, `%{time_total}` (seconds, six decimals, whole request), `%{size_download}` (decoded body bytes), `%{url_effective}`, `%{content_type}`, `%{redirect_url}`, `%{num_redirects}`. Escapes `\n \r \t` and `%%`. Printed even when `-f` stops the body. `-o /dev/null` discards the body (the kernel's `/dev/null` device). | Any other variable, `%header{}`, and `@file` exit 2 with the supported set, where curl warns and prints nothing. Refused with `--json` (exit 2). Compared against real curl 8.23 in `tests/compat.rs`. |
| `-m, --max-time <s>` | Whole-request timeout, applied to the ureq agent. Always set: the `CurlConfig` default (30s) applies when the flag is omitted, and the flag is **clamped to the embedder's ceiling** — it can lower the budget, never raise it. A deadline that fires is exit **28**, not exit 7. | WASM: refuses with literate error (no `tokio::time`). |
| `--connect-timeout <s>` | Connect-phase timeout. | WASM: refuses with literate error. |
| `--unix-socket <path>` | Connect to an AF_UNIX socket instead of the URL host; the host in the URL is a placeholder the `Host` header still needs (e.g. `http://localhost/`). The path is resolved through `ToolCtx::resolve_path`, mapped by `backend().resolve_real_path()`, and **canonicalized** before it is required to fall inside the mount it was reached through — so a symlink out of the mount is refused, which a lexical prefix test would not catch. | Native, unix-family targets only; wasm and non-unix refuse with a literate error. `--abstract-unix-socket` deferred (CU5). |
| `--json` | **Not curl's request-body shorthand.** It is kaish's global output flag for every tool (see next section). | curl 7.82 `--json` (request body) refused with literate error. |


### Egress: names and addresses

`AllowEgress::permit` sees the URL, and `AllowEgress::permit_address` sees each
address DNS returned, before any socket opens. Both matter, because a name and
an address need not agree: `internal.example` can resolve to `127.0.0.1` or to
a metadata endpoint, by a hostile zone, by rebinding, or by mistake. curl
supplies ureq with exactly the addresses the policy approved, so nothing is
resolved a second time between the check and the connect.

`AllowByList` applies the same loopback and link-local opt-ins to addresses
that it applies to names, with one exception: an address written into the
allowlist itself is permitted, because the embedder named that address on
purpose and there is no name-to-address gap left to close.

**Name the allowlist; do not inherit it.** `CurlConfig::default()` denies
egress by an empty `AllowByList`, so an embedder that changes nothing reaches
nothing. Build the policy explicitly anyway:

```rust
CurlConfig::default()
    .with_limits(Limits { max_time: 10.0, ..Limits::default() })
    .with_allow_egress(AllowByList::new().with_allowed_hosts(["api.internal.example"]))
```

The reason is not style. Between 2026-08-15 and 2026-08-20 this default was
`AllowAll` while this document, the module header, and `CurlConfig::new`'s own
doc all said deny — and the unit test asserting it was named
`..._with_deny_by_default_egress` and asserted `Allowed`. A cross-model review
found it. The one embedder in flight was unaffected, because it named its
allowlist at a single call site and never read the default. An explicit
allowlist survives its dependency's default being wrong; an inherited one is
only as good as the last person to check.

Set `max_time` explicitly too, under whatever timeout encloses the call, so a
slow request surfaces as curl exit 28 inside that budget rather than the outer
timeout firing and hiding the cause. The 30s default is a safety net, not a
fit.

### Headers the embedder supplies

`CurlConfig::with_injected_headers` adds headers to every request that the
agent can neither read nor remove — a credential for an internal API, a tenant
header an audit trail needs. A caller `-H` naming the same header is dropped
rather than sent beside it, so the far end never has to choose. That includes
`-u`: an injected `Authorization` replaces the `-u` credentials too, so one
`Authorization` goes out. Without an injected one, a caller's `-H Authorization:`
wins over `-u`, as in curl 8.23. Like `-u`
credentials, injected headers stop at a change of host; so do
`Authorization`, `Cookie`, and `Proxy-Authorization` set by the caller, because
stripping only the `-u` form would leave `-H` as the way around the rule.

`-s`/`--silent` and `-S`/`--show-error` are **accepted**. There is no progress
meter here, so the request has nothing to do — that is a request satisfied, not
a flag ignored. One divergence, deliberate: real curl's `-s` also silences
*error* messages, and this build always reports errors, because an agent that
cannot see a failure cannot act on it. `curl -sSL <url>` therefore behaves
exactly as an agent expects, and `curl -s <url>` still prints the error when
one happens.

Short flags combine: `-sSL`, `-iL`, `-fL` are read as the flags they spell.
A cluster containing a value-taking flag (`-so out.txt`) is refused, naming
the flag that broke it.

This build refuses the following flags at parse time with literate errors:
`-J`, `--compressed`, `--proxy`/SOCKS,
`--cookie`, `--verbose`, `--retry`, `--cert`/`--key`,
`--resolve`, `--config`, `--netrc`, `--parallel`, `--next`, and — because the backend
does not implement them rather than because they are out of scope — `-k`/`--insecure`
and `--unix-socket`. Each carries a message
that names the unsupported flag and the supported alternative. See "Literate errors" below.
A flag this build does not recognize is **refused**, not ignored: an agent
that typed something must never be left believing it took effect. So is a
second URL — one per invocation, and curl says so rather than fetching the
first and dropping the rest — and a flag whose value is missing.

## Output, and the `--json` collision

kaish has a global `--json` flag that means **structured output** on every
tool. `curl` 7.82 added a `--json` that means **a JSON request body**. Same
spelling, opposite meaning. This build keeps kaish's convention intact:

- `curl` flattens `GlobalFlags` like every builtin, so `--json` sets the output
  format. `curl --json <url>` returns a structured object (and only then does
  the result carry structured data; see below):

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

Without `--json`, the output is the body text and the result carries no
structured data. That is what makes `curl -s <url> | jq -r .name` read the
body and `x=$(curl <url>)` bind the text. The result used to carry the
response object on every response, and a pipe hands the next tool that object
in place of the text, so `curl <url> | jq -r .name` printed `null` with exit 0
(survey SW-1). Only `--json` sets it. `tests/compat.rs` pins all three
(`| jq`, `$(...)`, and `--json | jq .status` as the control).

## Exit codes

kaish-curl mirrors curl's numeric exit codes for the cases it covers, and maps
the rest to 1. State the number so an agent can branch on it.

| Code | Cause |
|---|---|
| 0 | Success (any HTTP status unless `--fail`). |
| 2 | A flag combination refused before any request: `-d @-` and the other stdin forms (not supported). |
| 26 | A file named by `-d @path` and its relatives could not be read (missing, a directory, unreadable). No request was sent. |
| 6 | Host not found. |
| 7 | Could not connect — **and** URL malformed (bad scheme, bad syntax). curl uses 3 for a malformed URL; kaish's kernel reserves 3, so this build maps malformed URLs here with an error naming the real cause (CU11). |
| 22 | `--fail` or `--fail-with-body` and HTTP status >= 400. |
| 23 | The `-o` / `-O` file could not be written, or the `--output-dir` directory does not exist. |
| 28 | `--max-time` or `--connect-timeout` exceeded. |
| 33 | `-C -` got an answer that cannot be appended to the file (200, another status, or a 206 for other bytes). The file is unchanged. |
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

Cookies:

```
curl: '--cookie' is not supported. Send cookies with '-H Cookie:<value>'.
```

Multiple URLs, `--next`, `--parallel`:

```
curl: only one URL per invocation is supported. Call curl once for each URL.
```

`--verbose`:

```
curl: '--verbose' is not supported, and no request was sent. This build prints
no request or connection trace. Use 'curl -i <url>' to print the response
headers above the body. The request headers are the ones you set with '-H'
plus 'User-Agent: kaish-curl'.
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
  the boundary `kaish-tools-git` uses for blocking gix. No embedder IO-driver or
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
- **Body read respects the budget.** Both `curl -o <file>` and `curl <url>`
  read the body into memory through `Body::as_reader()`, bounded by
  `Limits::max_response_bytes`: a body over it is exit 1 and nothing is
  written. `-o` then writes the bytes through `ToolCtx::backend().write`, so
  the embedder's VFS budget applies as well. Nothing streams, so the cap is
  also the largest file `-o` can save. Stdout output is subject to the kernel
  output limits (and the 1 MB wasm clip).
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
    compat.rs        # real kernel + loopback server; compares with real curl where installed
    tls.rs          # -k against a self-signed cert (optional fourth)
```

`CurlConfig` mirrors `GitConfig`: embedder-supplied, with a `tool_name` for
embedders who want a different name. The `Tool` impl declares `operations`:
`net.request` always, plus `fs.overwrite` (declared for `-o` and `-O`). The ids
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
map is settled, and the `block_in_place_compat` boundary is already proven by the
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
