# Deferred work

In-tree issue tracker: known divergences, limitations, and parked work that is
deliberately out of scope for the PR that discovered it. Entries move out when
fixed (delete the entry in the fixing PR) or when they graduate to a GitHub
issue because someone outside the repo needs the link.

## git status — divergences from git

Found by the PR #23 cross-model review (deepseek-v4-pro C-series); each needs a
fixture asserted against real `git status --porcelain` before fixing.

- **C1 — staged half, file↔dir typechange: investigated, not reproduced.**
  The hypothesis was that a file↔dir typechange between HEAD and the index
  reports `A` where git reports `T`. Three constructions were tried against
  real git as the oracle (`status.rs`'s
  `c1_a_staged_sibling_index_does_not_make_git_report_typechange`, plus two
  discarded ones using plain `git add`/`update-index`, both of which those
  tools refuse to write as a same-path pair): a real directory replacing a
  tracked file staged with plain `git add`, the mirror direction, and a
  hand-written index holding `foo` and `foo/a.txt` as siblings at once (a
  shape ordinary git tooling will not write, but reads back the same as we
  do). In every case git's own diff machinery treats the vanished directory
  prefix as *absent* from the old side, not a same-path type change, and
  reports `A` — agreeing with us. No fixture produced a `T` from real git.
  Recommend closing this entry rather than carrying it as still open; the
  worktree half (C2) is the real divergence in this area.
- **C2 — unstaged half, file→dir reports `Typechange`, confirmed, and now
  characterized by a named test.** Under `--untracked all`, git reports ` D`
  for the file plus `??` for the directory's contents; we report only
  `Typechange` on the file's path and never descend into the new directory at
  all (`walk_untracked_and_ignored` skips it because the path is already
  tracked). Default (`normal`) mode does not show the divergence — git's own
  normal mode is equally silent about the new directory's content there.
  Pinned by
  `status.rs::c2_a_file_replaced_by_a_directory_diverges_from_git_in_all_mode`,
  which asserts our exact output against a live git oracle; it goes red the
  moment either side's behavior changes.
- **C3 — ignored directory containing tracked files, confirmed, and now
  characterized by a named test.** Emitted as `!!` and not descended; git
  descends (tracked wins over ignore), reports the tracked files' states, and
  never emits the directory itself. Pinned by
  `status.rs::c3_an_ignored_directory_holding_a_tracked_file_diverges_from_git`,
  which asserts our exact output against a live git oracle; it goes red the
  moment either side's behavior changes.
- **C7 — an empty untracked directory is reported as `??`, confirmed, and now
  characterized by a named test.** Git has no concept of a directory as a
  trackable thing — only blobs are entries — so a directory with nothing
  inside it produces no output at all. We report it as `??`. Found against a
  real repository (`crates/kaish-vfs/tests` in a real `kaish` checkout, via
  `big_repo.rs`), minimized and pinned by
  `status.rs::c7_an_empty_untracked_directory_is_reported_where_git_reports_nothing`,
  which asserts our exact output against a live git oracle; it goes red the
  moment either side's behavior changes.
- **C8 — a directory wholly ignored by its own nested `.gitignore` is reported
  as `??`, confirmed, and now characterized by a named test.** A never-tracked
  directory whose own `.gitignore` ignores everything under it (that
  `.gitignore` file included) has nothing left inside that qualifies as
  untracked-and-not-ignored, so git reports nothing under the default view
  (and `!!` only for its contents, never the directory, under `--ignored`). We
  report the directory itself as `??`. Distinct from C3: C3 is a *tracked*
  file inside an ignored directory; this is a wholly-untracked directory whose
  *contents*, not the directory itself, are what an ignore rule names. Found
  against `.crush/` — a tool cache dir with `.gitignore` containing `*` — and
  reproduced identically in all three real repositories tried (`kaish`,
  `kaibo`, `kaish-extras`, via `big_repo.rs`), minimized and pinned by
  `status.rs::c8_a_directory_wholly_ignored_by_its_own_nested_gitignore_is_reported_untracked`,
  which asserts our exact output against a live git oracle; it goes red the
  moment either side's behavior changes.

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

- **G4 — `guard_alternates` may be a 1-bit existence oracle: probed, not an
  oracle.** A repository with a relative `objects/info/alternates` entry
  pointing outside the mount is refused by `contain` (exit 4) when the target
  exists. The open question was what `gix_odb::alternate::resolve` does when
  the target does *not* exist: git's native behavior is to silently drop a
  broken alternate, and if gix did the same, `contain` would never be reached
  and the store would open (exit 0) — the same outside-vs-nonexistent split G2
  had. Probed in
  `hostile_repo.rs::an_alternates_entry_cannot_report_whether_its_outside_target_exists`:
  both cases (existing outside target, nonexistent outside target) exit 4 with
  byte-identical error messages. `gix_odb::alternate::resolve` itself errors on
  an unresolvable entry rather than silently dropping it, so `guard_alternates`
  never sees an empty chain to wave through. Not an oracle. Safe to close.

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
  matches *or* has examined `MAX_COMMITS_EXAMINED` (100k) commits. Confirmed
  bounded against real repositories in
  `big_repo.rs::an_unmatched_filter_is_bounded_not_hung`: a history smaller
  than the cap (every candidate repo on this machine) completes the *whole*
  walk and correctly reports `truncated: false` — an honest "nothing matched",
  not a guess; only a history at or past 100k commits would see `truncated:
  true`. Either way the walk finishes (well under a second on a ~1400-commit
  repository) rather than hanging. A commit-graph-backed date cutoff
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

- **L5 — commits with a tied committer instant can order differently from
  git.** Found against a real repository
  (`big_repo.rs::log_oid_sequence_matches_git_at_several_limits`,
  `KAISH_GIT_BIG_REPO=$HOME/src/kaish`, 2026-08-21): two merge commits
  (`f8c1b508`, `3a74e505`), neither an ancestor of the other, share the exact
  same committer instant to the second. Git's walk orders them one way; our
  walk (a `BinaryHeap` keyed on committer time alone, log.rs) has no secondary
  tiebreaker, so a tie resolves however the heap's internal structure happens
  to pop it — not necessarily git's order. Narrow (needs two unrelated
  same-second commits) but real: fast scripted merges, sub-second-granularity
  clocks, or a CI-driven series can all produce one. Fix wants a tiebreaker
  that matches git's own (likely a form of insertion/discovery order along the
  walk) added to `Pending`'s ordering.

- **L6 — `--stat` line counts can differ from `git show --numstat` by a small
  amount on a real diff.** Found against three real repositories via
  `big_repo.rs::stat_matches_git_numstat_on_a_sample_of_commits`
  (`KAISH_GIT_BIG_REPO`, 2026-08-21), and it reproduces in *both* directions —
  not a one-way "off by one":

  | Repository | Commit | We report | git sums to |
  |---|---|---|---|
  | kaish | `264bc8431f9ec8aa7d927fc04a10968b51ce1d4a` | 178 additions | 177 |
  | kaibo | `f06d86cee8d04be5a52c53f8957edd8fec790062` | 113 additions | 117 |
  | kaish-extras | `4cc0d1604f7bce9a3535a861612b527b5fbecd9b` | 829 additions | 826 |

  Every file in every case ends with a real trailing newline on both sides of
  its commit, so not the missing-final-line edge case. `gix-imara-diff`'s
  Myers implementation and git's own do not always produce the same edit
  script when a hunk admits more than one minimal alignment (nearby repeated
  or blank lines, common in real code) — the *edit distance* can match while
  the *split* between which lines count as added differs by a handful either
  way. Not isolated to a specific file or line yet; that is the fix's first
  step, not something the fixtures above pin further. Same family as L1/L3,
  found only because real, organically-grown diffs exercised it — no
  synthetic fixture in this suite has hit it.

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

## kaish-tools-image — a proposed third tool crate

Proposed 2026-08-21 by the kaibo session, on Amy's instruction to record it
rather than build it. Not scheduled; recorded so the driver is not lost.

**The driver is real.** kaibo is widening its Stability AI coverage from 3
routes to about 25. `edit/inpaint` and `edit/erase` each require a **mask
image**, `control/style-transfer` takes two input images, and every route
carries its own pixel-dimension constraints. An agent driving that API has to
make and measure images between calls. Today it cannot: kaibo compiles
`subprocess` out, so shelling out to ImageMagick is not available, and a cmake
or C dependency would end kaibo's musl static single binary. A kaish tool
bundle is the composable answer — `img` in a pipeline costs one call where a
bespoke host-side tool costs several.

**Pure Rust is the whole constraint, not a preference.** The bundle must link
under musl with a Rust toolchain and nothing else. Gate it the way
`kaish-tools-git` gates spawn machinery: a CI tripwire over `cargo tree`
asserting no `cc` or cmake build script entered the graph, matching kaibo's
own aws-lc/openssl exclusion. It belongs in the bundle's PR 0, the way git's
tripwires did.

**Give that tripwire a negative control, and treat that as part of building
it.** A check of this shape has a specific failure mode: "the search found
nothing" and "the command errored, so the search found nothing" are the same
exit 0, so the gate goes green while proving nothing. kaibo's musl release
gate failed exactly this way — it printed "Failed to find zig", exited 0, and
a green check meant nothing for months. kaish's 0.14.0 approvals-ledger probe
was the same shape: a probe of `/v/approvals` reported "not found" whether or
not the feature worked, so the all-clear was empty. The fix is a case where
the check provably reports differently when its subject is broken — assert a
crate that IS in the graph is found, alongside asserting the forbidden ones
are not, so a broken invocation fails the job instead of passing it.

Operations, in the order they pay rent:

1. **probe** — dimensions, mime, byte size. The cheapest, and every other
   decision depends on it: an agent cannot do arithmetic on bytes it cannot
   measure.
2. **rasterize SVG to PNG** — the high-value one. Models write SVG well, so
   "draw the mask as SVG, rasterize, send" is a good loop for inpaint and
   erase.
3. **resize / crop / pad** — preflight against a route's dimension limits.
   Turns a 400 into a result.
4. **composite and alpha operations** — an alpha channel is how a mask travels.
5. **format convert** — png, jpeg, webp.

Crate picks, all pure Rust:

- `image` — decode and encode png/jpeg/webp/gif, resize, crop, composite.
  Default features are already C-free (zune-jpeg, `png`, `image-webp`). WebP
  **encode** is lossless-only in pure Rust, and AVIF encode pulls `ravif` —
  leave that feature off.
- `tiny-skia` — a pure-Rust 2D rasterizer, the Skia subset: paths, fills,
  strokes, gradients.
- `resvg` — SVG to PNG, on tiny-skia.
- `cosmic-text` or `ab_glyph` — only if text rendering lands in scope.

**An audio sibling has the same shape**, if it is ever wanted: `symphonia` to
decode, `hound` for wav, `rubato` to resample. mp3 **encode** is LAME and
therefore C, so wav output only. Stability's audio-inpaint route takes
`mask_start` and `mask_end` in **seconds**, so a duration probe is the minimum
useful audio tool and it is nearly free.

**Out of scope, and a separate decision: whether kaibo ever mounts the
bundle.** kaibo's kaish VFS is deliberately blind to its media CAS, so a
builtin registered there could reach only project files and ephemeral
`MemoryFs` scratch. Bridging that to a media call is a new write surface and
needs its own gate. Amy's near-term plan on the kaibo side is to expose the
routes to the client model directly and learn the real usage patterns before
deciding what to bring in-house. The bundle can exist without that decision.

**That decision has a second precondition, and it is a hard one: version
unification.** This workspace pins the kaish crates to one minor, and pre-1.0
a kaish minor is a breaking release, so a caret range cannot span two of them.
Any bundle built here is therefore mountable by an embedder only while both
sit on the **same kaish minor** — otherwise the graph carries two copies of
every kaish crate and the `Tool` trait does not match. kaibo is on
`kaish-kernel` 0.14.1 heading for 0.16 while this workspace is on 0.15, so
today the answer is no on arithmetic alone, before any VFS question is
reached. Whoever picks this up should check the minor first and discover it
here rather than at link time.

The kaibo session has an endpoint-by-endpoint input-shape table pulled from
the live OpenAPI spec; ask for it when this starts.
