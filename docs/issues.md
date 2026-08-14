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
- **CU3 — `--max-time` / `--connect-timeout` on wasm.** Sync XHR forbids a
  timeout and `tokio::time` panics on `wasm32-unknown`, so neither flag can be
  honored. Documented as a refusal; no fix without the async-`execute` path.
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
- **CU7 — unstable ureq transport API for `--unix-socket`.** ureq 3.x has no
  first-class unix-socket connect; the build implements a `Transport` over
  `std::os::unix::net::UnixStream` through ureq's `unversioned::transport`
  module, which carries no semver guarantee. A ureq 4.x bump could break it;
  pin ureq and revisit on minor bumps.
