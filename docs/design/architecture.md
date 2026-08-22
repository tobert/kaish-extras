# kaish-git v2 — architecture

Status: **current** — proposed 2026-08-01 (Opus design agent); three co-architect
passes folded into the body 2026-08-02 (see
[Changelog / provenance](#changelog--provenance)). Inputs:
[../git.md](../git.md) (history, autopsy, design commitments),
[gix-plumbing-feasibility-2026-08.md](gix-plumbing-feasibility-2026-08.md)
(the source-read verification this design's dependency strategy rests on),
[gix-research-2026-08.md](gix-research-2026-08.md) (the earlier facade-era
gitoxide research — partly superseded, see [A.2](#a2-feature-axes)),
[safety-inventory-2026-08.md](safety-inventory-2026-08.md) (kaish safety
facilities). This document makes the calls; it does not survey options. Where a
call was close, the runner-up is named.

The approval-ledger design lives in kaish (pointer:
[approval-ledger.md](approval-ledger.md)). Everything write-shaped
here is marked **depends on ledger**; the read profile is designed to need
nothing from it.

---

## 0. Decisions at a glance

| # | Decision | Where |
|---|---|---|
| 1 | One crate, `kaish-tools-git` (the reserved name), public typed model module | [A](#a-crate-layout) |
| 2 | **No `gix` facade** — build on the plumbing crates ("Path 2"), twelve exact pins | [A2](#a2-feature-axes), [A4](#a4-pinning-and-the-dependency-tripwires) |
| 3 | Feature axes: `read` (default), `textdiff`, `worktree`, `commit`, `remote`, `parallel` | [A](#a-crate-layout) |
| 4 | wasm is a **compile error**, not a degraded build | [A](#a3-wasm), [F3](#f3-wasm-and-gix-sec) |
| 5 | No shell-out tier. Ever. If you want real git, run real git | [A](#a2-feature-axes) |
| 6 | Ten read verbs: `info status log show ls diff branch tag blame worktree list` | [B](#b-the-verb-surface) |
| 7 | Structured diff is the primary form; unified patch is a *rendering* of it | [B4](#b4-git-diff) |
| 8 | Bare `git diff` is git parity — index→worktree, unstaged only | [B4](#b4-git-diff) |
| 9 | Status: porcelain letters in the text surface, self-describing words in JSON | [B2](#b2-git-status) |
| 10 | `owns_output` is **not used anywhere** — typed `OutputData` + `rich_json` | [B10](#b10-output-discipline) |
| 11 | Profile config: embedder-supplied Rust struct, subtractive, no config file | [C](#c-the-profile-config) |
| 12 | Write profiles are **unconstructible** without a deliberate opt-in (type-level) | [C](#c3-write-profiles-are-type-gated) |
| 13 | Read-only proven by a `.git`-fingerprint test across the whole read surface | [D4](#d4-layer-4--the-proof-the-git-fingerprint-test) |
| 14 | Isolation **by construction** — we load only what we choose to load | [D2](#d2-layer-2--isolation-by-construction) |
| 15 | Default build cannot spawn: `gix-command` / `gix-transport` / `gix-filter` all absent, enforced by CI tripwires | [D3](#d3-layer-3--the-textconvfilter-attack-surface) |
| 16 | clap **tree for schema reflection**, flat **per-verb parse** at execute time | [E1](#e1-schema-tree-and-argv-routing) |
| 17 | No gix value ever crosses an `.await` — makes `!Send` a non-issue | [E3](#e3-blocking-calls-and-send-ness) |
| 18 | Repo discovery is **ceilinged at the VFS mount root** | [E2](#e2-the-resolve_real_path-bridge) |
| 19 | Identity from `GIT_AUTHOR_*` / `GIT_COMMITTER_*` kernel vars, no fallback | [E6](#e6-hermetic-identity) |
| 20 | Worktree create/remove: implement here on gix plumbing, then offer upstream | [F2](#f2-worktree-createremove) |
| 21 | Exactly **one** kaish PR is proposed, and the read profile ships without it | [G](#g-kaish-pr-cascade) |
| 22 | Read profile ships as 0.1.0 in nine small PRs, zero kaish changes | [H](#h-phasing) |

---

## A. Crate layout

### A.1 Crates and modules

**One crate: `crates/kaish-tools-git`** in the kaish-extras workspace, published
to the reserved crates.io name. The typed repository model is a *public module*
of that crate, not a second crate.

Runner-up (close): splitting `kaish-git-core` (gix wrapper + typed model) from
`kaish-tools-git` (the `Tool` impl), so kaibo could consume the model without the
tool surface. Declined for now because there is exactly one consumer and a second
crate buys a version-skew tail immediately (kaish-extras already pins kaish by
git rev; adding an internal version boundary doubles that bookkeeping). The
module boundary below is drawn *as if* the split will happen — `model` and `repo`
must not depend on `kaish_tool_api` — so the split is a mechanical move if a
second consumer appears.

```
crates/kaish-tools-git/
├── Cargo.toml
├── src/
│   ├── lib.rs        # pub use: GitTool, GitConfig, Profile, Verb, model::*
│   ├── config.rs     # GitConfig / Profile / Verb / Limits  (no gix, no kaish)
│   ├── model.rs      # typed results: Status, LogEntry, DiffFile, Hunk, …  (no gix, no kaish)
│   ├── repo.rs       # discovery + ReadRepo; the ONLY place the plumbing handles are assembled
│   ├── revspec.rs    # the accepted rev grammar (not gix's full revspec)
│   ├── pathspec.rs   # literal paths + simple globs → repo-relative matcher
│   ├── error.rs      # GitError → exit-code taxonomy
│   ├── render.rs     # model → OutputData (+ rich_json) and → unified-patch text
│   ├── tool.rs       # Tool impl: schema tree, verb routing, GlobalFlags, spans
│   ├── verbs.rs      # `pub(crate) mod` declarations (no mod.rs — kaish house rule)
│   └── verbs/
│       ├── info.rs status.rs log.rs show.rs ls.rs diff.rs
│       ├── branch.rs tag.rs blame.rs worktree.rs
│       └── write/        # #[cfg(any(feature="worktree", feature="commit"))]
└── tests/
    ├── support.rs        # fixture repos (real git as the oracle; see H)
    ├── readonly_fingerprint.rs   # the headline safety test (D.4)
    ├── hostile_repo.rs           # textconv/filter/include.path fixtures (D.3)
    └── <verb>_tests.rs
```

The dependency direction is strict: `config`/`model` depend on nothing of ours;
`repo` depends on `model` + gix; `render`/`tool` depend on everything and are the
only modules that mention `kaish_tool_api`. A reviewer can audit "can this crate
write?" by reading `repo.rs` and `verbs/write/` alone.

### A.2 Feature axes

**The dependency strategy comes first, because every axis below is shaped by
it: this crate does not use the `gix` facade.** The facade links `gix-command`
— pure-Rust `std::process::Command` spawn machinery — **unconditionally**, via
`gix-command → gix-transport → gix-protocol → gix`, reinforced by `gix-diff`
(textconv) and `gix-filter` (clean/smudge); `gix --no-default-features` does not
avoid it. That was found while scaffolding PR 0 and it falsified the original
"no spawn in a default build" claim outright (kaish-extras#4). The earlier
research missed it because it audited for C toolchains and network stacks, and
`gix-command` is neither.

**The call: build on gix's plumbing crates, skipping the facade** — "Path 2" in
the feasibility report and in the issue backlog. A source-read verification of
the pinned sources
([gix-plumbing-feasibility-2026-08.md](gix-plumbing-feasibility-2026-08.md))
found every premise this needs holds — object access sits behind `gix-object`'s
three find traits, one required method each (§1); `gix-traverse` /
`gix-revision` / `gix-revwalk` are generic over the object source and do not
depend on `gix-odb` at all (§2); `gix-index` decodes *and* encodes from bytes
(§4a); `gix-ref`'s read path is public byte parsers (§4b). The pin set links
**none** of `gix-command` / `gix-transport` / `gix-filter`, so the structural
claim is true again — and this time it is a property of the dependency graph
that CI asserts in one line ([A.4](#a4-pinning-and-the-dependency-tripwires)),
not a claim about configuration that must be re-proved on every gix bump.

What it costs: we own the convenience layer the facade would have given us —
repository assembly, a restricted rev-parse grammar, unified-patch rendering —
and twelve pins that move on independent schedules instead of one. What it buys
beyond the guarantee: the byte-level seams that make a kaish-vfs-backed *second*
implementation of object/ref/index access possible, and with it the browser
build ([A.3](#a3-wasm), [F.3](#f3-wasm-and-gix-sec)). Staging follows from that
— build native-first behind kaish-owned access traits, and treat VFS-backed
storage as a second implementation of those traits, not a refactor.

Runner-up (close): keep the facade and downgrade the no-spawn claim to a runtime
guarantee — repos opened `Permissions::isolated()`, no network verbs, hostile-repo
fixture proving no subprocess is created. Less code, ships sooner. It lost
because its guarantee is a claim about *configuration*, re-provable only per
release, and because it forecloses both the VFS path and wasm.

Mirroring kaish's own capability-axis discipline: opt-in, each compiles out
cleanly, and the default is the smallest useful thing.

| Feature | Default | Adds | Cost / risk |
|---|---|---|---|
| `read` | ✓ | the plumbing pin set ([A.4](#a4-pinning-and-the-dependency-tripwires)); all ten read verbs | none — offline, no C toolchain but `zlib-rs`, no spawn machinery linked at all |
| `textdiff` | — | line hunks from `gix-imara-diff` on raw blob bytes; `--patch` rendering | we render the patch ourselves; no textconv, no attributes machinery |
| `worktree` | — | worktree create/remove/lock/prune (**depends on ledger**) | writes refs and directories |
| `commit` | — | index staging + commit (**depends on ledger**) | writes objects and refs |
| `remote` | — | **unrevisited under Path 2** — the facade-era wiring (`gix/blocking-http-transport-reqwest` + direct `reqwest` `rustls-no-provider` + `rustls/ring`) assumes a facade we no longer have, and `gix-transport` is a tripwire-forbidden crate | re-derive before pursuing; never available on wasm |
| `parallel` | — | parallelism in the plumbing crates (and LRU pack caches) | spawns threads inside an embedded kernel |

Notes that are load-bearing:

- **The `read`↔`textdiff` split is no longer the no-subprocess boundary.** It
  was, under the facade, where `blob-diff → attributes → gix-command`. Under
  the plumbing, *no* build links spawn machinery, so `textdiff` carries no
  spawn risk: line hunks come from `gix-imara-diff` run directly on blob bytes,
  skipping textconv and `.gitattributes` by construction — arguably the more
  correct answer for an agent, which wants raw content. What survives of the
  split is staging: `textdiff` gates the hand-written unified-patch renderer
  ([F.1](#f1-unified-patch-assembly), [H](#h-phasing) PR 6), which is the real
  work. **Open:** whether `textdiff` stays a feature axis once that renderer
  lands, or folds into `read`.
- **Rename detection does not exist without `gix-diff`'s `blob` feature**, and
  `blob` is exactly what pulls `gix-command`. The whole `rewrites` tracker —
  `Rewrites`, `rewrites::Tracker`, `tree_with_rewrites` — is
  `#[cfg(feature = "blob")]`, and the similarity computation *is* the blob
  platform (feasibility §5.4). So renames are **exact-match-only** (same blob
  oid, new path) and are reported honestly; see [B.4](#b4-git-diff). This also
  corrects `gix-research-2026-08.md` §3(a) mitigation (ii), which proposed
  dropping to `tree_with_rewrites` without `blob` for name/status diff — that
  function is itself `blob`-gated. Without `blob` the available surface is
  `gix_diff::tree()` plus a `tree::Visit` delegate we supply.
- **`status` and `blame` are hand-composed**, because `gix-status` and
  `gix-blame` drag in the machinery this design exists to avoid: `status` is
  index + worktree walk (we own `.gitignore` and pathspec matching and the
  racy-clean stat rules), `blame` is revwalk + per-commit path-limited tree
  diff + line mapping. Both are L-tier under *either* dependency strategy
  (feasibility §5.2) — the fork never moved the biggest cost.
- **`parallel` stays off.** Without it most gix types are `!Send`, which is the
  strictly harder constraint; designing for it from day one (see
  [E.3](#e3-blocking-calls-and-send-ness)) means an embedder who needs the
  non-parallel build never hits a surprise, and the `block_in_place` discipline
  is identical either way. Threads inside kaibo/kaijutsu are also a cost nobody
  asked for. Revisit with benchmarks, not with vibes.
- **No shell-out tier.** The research suggested keeping `git(1)` as a fidelity
  fallback behind `subprocess`. Declined: a fallback that spawns real git
  forfeits every property this crate exists to provide (read-only-by-construction,
  hermetic identity, typed output, no hooks) and it would do so *inside* a verb
  the agent believes is safe. kaish already runs external commands perfectly well
  — an embedder who wants git(1) fidelity should invoke git(1), visibly, through
  the `subprocess` axis, not through a verb wearing our name.

### A.3 wasm

**kaish-tools-git does not compile for wasm targets.** `src/lib.rs` opens with:

```rust
#[cfg(all(target_family = "wasm", not(feature = "unsupported-wasm-loose-objects-only")))]
compile_error!(
    "kaish-tools-git does not support wasm: gix-sec's ownership check \
     special-cases WASI but not bare wasm32, and it is reachable through \
     gix-discover and gix-config, so this crate does not compile for \
     wasm32-unknown-unknown. Beyond that, the native storage path reads \
     through gix-odb and std::fs; a browser build needs the VFS-backed \
     implementation of the access traits. See kaish-extras GH #3. Build \
     kaish-web without the git tool."
);
```

The mmap premise this guard originally cited is **retired**: `gix-pack` 0.73
already carries byte constructors — `from_data()` on the pack data, pack index
*and* multi-pack-index file types, all generic over `T: Deref<Target = [u8]>`,
with only `Bundle`'s 69-line find pipeline mmap-hardwired (feasibility
§3.1–§3.3). The upstream ask we were going to file had already been built. Two
things actually block wasm, both tracked as **kaish-extras#3**:

1. **Compile: `gix-sec`.** The plumbing pin set builds clean for
   `wasm32-unknown-unknown` today — `gix-ref`, `gix-index`, `gix-lock`,
   `gix-tempfile`, `memmap2` and `gix-imara-diff` all tolerated in-tree, built
   rather than inferred (feasibility §6.2). The sole compile failure is
   `gix-sec`'s ownership check, which special-cases `target_os = "wasi"` to
   `Ok(true)` but not bare `wasm32`; it is reachable only through
   `gix-discover` and `gix-config`. The fix is a two-line cfg extension
   upstream ([F.3](#f3-wasm-and-gix-sec)).
2. **Runtime: storage.** The native build reads objects through
   `gix-odb::Store`, which is irreducibly path-and-mmap bound, and discovers
   through `gix-discover`, which is pure `std::fs` probing. A browser build
   needs the VFS-backed second implementation of the access traits — the same
   work that delivers git-over-kaish-VFS, not an increment on top of it.

Shipping before both land would be precisely the silent-degradation failure
mode the project rejects, and a runtime error per object would be worse (it
looks like a repo problem, not a build problem). A compile error names the real
cause once, at the only moment anyone can act on it. The deliberately ugly
escape feature exists so upstream work can be exercised without a fork; it is
not a supported configuration and CI does not build it. **Open:** its name
(`unsupported-wasm-loose-objects-only`) was coined for the retired mmap premise
and no longer describes anything — rename it the next time the guard moves.

`kaish-web` therefore keeps its current feature set and simply does not depend
on this crate. As [F.3](#f3-wasm-and-gix-sec) progresses the guard narrows and
eventually disappears.

### A.4 Pinning and the dependency tripwires

Twelve exact pins, not one. Every gix crate is 0.x with breaking minors roughly
monthly (0.82.0 of the facade was yanked), so each is pinned with `=`, and bumps
are their own PR with the full test suite as the gate:

| Crate | Pin | Notes |
|---|---|---|
| `gix-object` | `=0.63` | the find traits; the whole object seam |
| `gix-odb` | `=0.83` | native object store (the VFS path implements `Find` instead) |
| `gix-ref` | `=0.66` | ref store; byte parsers are what a VFS store would reuse |
| `gix-traverse` | `=0.60` | commit and tree walks |
| `gix-revision` | `=0.48` | `describe`, `merge_base` — **not** `spec` (see [B](#b-the-verb-surface)) |
| `gix-revwalk` | `=0.34` | the shared commit-graph cache |
| `gix-commitgraph` | `=0.38` | optional acceleration; `Option<&Graph>` everywhere, so `None` is first-class |
| `gix-discover` | `=0.54` | native discovery; dropped on the VFS path, which takes `gix-sec` with it |
| `gix-index` | `=0.54` | `State::from_bytes` / `write_to`, byte-only both ways |
| `gix-pack` | `=0.73` | generic over its backing bytes; `from_data` constructors |
| `gix-diff` | `=0.66` | **without the `blob` feature** — `blob` is what pulls `gix-command` |
| `gix-config` | `=0.59` | `from_bytes_no_includes`; do **not** use 0.51, it resolves a whole second-generation graph |

`gix-imara-diff` supplies line hunks under `textdiff`. The set resolves with no
duplicate crates (`cargo tree -d` → nothing to print).

CI tripwires, all `cargo tree`-based, all on the **default** feature set unless
noted:

| Tripwire | Must be | Why |
|---|---|---|
| `gix-command`, `gix-transport`, `gix-filter` | each `cargo tree -i` reports "did not match any packages" | the whole spawn/transport/filter class is structurally absent — this is decision 15, made testable |
| `aws-lc-sys`, `openssl-sys`, `native-tls`, `curl` | absent (incl. `--features remote`) | the cmake/C trap the research documented |
| build scripts | `zlib-rs` only | the no-C-deps claim, kept honest |

The wasm gate is [A.3](#a3-wasm)'s `compile_error!`, not a tripwire: `memmap2`
compiles for wasm perfectly well, so its presence proves nothing.

---

## B. The verb surface

Conventions that apply to every verb:

- **Tool name is `git`, configurable.** Registering as `git` deliberately shadows
  external git (kaish resolves builtins before PATH). That is the point for an
  agent surface, and a footgun for a human who expected porcelain — so
  `GitConfig::with_tool_name("kgit")` exists for embedders who want both.
  Signed off (Amy, 2026-08-01): the first consumer is kaibo, read-only, where
  shadowing is exactly what is wanted; when write verbs land, ours stays
  preferred and real git remains reachable by full path where exec is enabled.
- **`--repo <PATH>`** on every verb selects the repository; default is the cwd.
  No `-C` (git muscle memory is not a design input).
- **`--json`** via flattened `GlobalFlags` on every verb struct. Structured from
  day one — there is no verb whose JSON is a string blob.
- **`--limit <N>`** on every verb that can produce unbounded rows, with a
  per-verb default and a config-supplied hard cap. Truncation is *always*
  reported (`"truncated": true` and a stderr note), never silent.
- **Paths** are literal paths or simple globs (`*`, `**`, `?` via `kaish-glob`),
  resolved through kaish's cwd then made repo-relative. **No git pathspec magic**
  — `:(exclude)`, `:!`, `:(glob)`, `:/` are a loud usage error naming the
  unsupported syntax.
- **Revisions** accept a deliberately small grammar (`revspec.rs`): `HEAD`,
  `<branch>`, `<tag>`, `refs/...`, `<oid>` (full or ≥4-char unambiguous prefix),
  `<rev>~N`, `<rev>^`, `<rev>^N`, and `<rev>:<path>` for `show`. Everything else
  — `@{...}`, `^{/regex}`, `:/text`, `A..B`, `A...B` — is a loud usage error.
  We parse that grammar ourselves and do not link `gix-revision`'s `spec`
  module at all. `gix_revision::spec::parse` is a pure parser driving a
  caller-supplied delegate — it resolves nothing — and the facade's
  implementation of that delegate is 952 lines (feasibility §2.4, §5.3). An
  agent-facing tool should have a small, stated revspec grammar rather than
  git's baroque one; unsupported syntax exits with a specific error, never a
  wrong answer. `describe` and `merge_base` remain available from
  `gix-revision`; they are independent of `spec`.
- **No `--force`, no `--all`-that-means-danger, no flag that silently discards
  work.** v1's `checkout(force = true)` default is the anti-pattern this surface
  is organized against.

### B.1 `git info`

The honesty verb: what am I looking at, and what am I allowed to do.

| Flag | Type | Default | Meaning |
|---|---|---|---|
| `--repo` | path | cwd | repository selector |

Output (table: FIELD/VALUE; `rich_json` object):

```json
{"repo_root_vfs":"/mnt/repos/kaish","repo_root_real":"/home/…/kaish",
 "git_dir":"/home/…/kaish/.git","bare":false,"shallow":false,
 "ref_backend":"files","head":{"branch":"main","oid":"…","detached":false},
 "worktrees":2,"submodules":0,
 "gix_pins":{"gix-object":"0.63.0","gix-pack":"0.73.0","…":"…"},
 "capabilities":{"profiles":["read"],"verbs":["info","status",…],
                 "features":["read"],"limits":{"max_rows":1000,…}}}
```

`capabilities` is discoverability, not authority: the agent learns what it may
ask for without being able to change it. `ref_backend` is where a reftable repo
gets caught early — see [E.5](#e5-error-taxonomy). There is no single facade
version to report, so `gix_pins` names the plumbing crates actually linked
([A.4](#a4-pinning-and-the-dependency-tripwires)); **open:** whether that is the
whole set, or only the few an embedder would act on.

### B.2 `git status`

| Flag | Type | Default | Meaning |
|---|---|---|---|
| `--untracked <no\|normal\|all>` | enum | `normal` | untracked file reporting depth |
| `--ignored` | bool | false | include ignored entries |
| `--path <PATH>` | repeatable path/glob | all | restrict to paths |
| `--limit <N>` | int | 1000 | max entries |

```json
{"head":{"branch":"main","oid":"…","detached":false},
 "entries":[{"path":"src/lib.rs","orig_path":null,"kind":"file",
             "index":"modified","worktree":"none","conflicted":false}],
 "totals":{"staged":1,"unstaged":0,"untracked":2,"ignored":0,"conflicted":0},
 "clean":false,"truncated":false}
```

In JSON, `index`/`worktree` each take one of
`none|added|modified|deleted|renamed|copied|typechange|untracked|ignored`.

**Porcelain letters in the text surface, self-describing words in JSON**
(Amy, 2026-08-01). Parity is deep in base training and RL: a model reading a
text status expects git's `XY` pair and will read one correctly whether or not
we spell it out. The clarity pressure that argued for words is relieved by the
`--json` variants instead — `git status --json`, `git diff --json --staged`
carry self-describing key names and word-valued fields, so scripts get clarity
while prompts get trained-in muscle memory. **One shape per surface**: letters
in text, words in JSON, and no `--porcelain` flag matrix — there is no
`-s`/`--porcelain`/`--long`. Renames are first-class (`orig_path`) and
conflicts are a boolean, both of which v1's hand-rolled renderer got wrong.

Table rendering: the `XY` pair, then `PATH` (+ `← ORIG` on renames).

### B.3 `git log`

| Flag | Type | Default | Meaning |
|---|---|---|---|
| `--rev <REV>` | rev | `HEAD` | starting point |
| `-n`, `--limit <N>` | int | 20 | max commits |
| `--path <PATH>` | repeatable | all | history restricted to paths |
| `--since <DATE>` / `--until <DATE>` | RFC3339 or `YYYY-MM-DD` | — | time window |
| `--author <SUBSTRING>` | string | — | **literal substring** match on name/email |
| `--merges` / `--no-merges` | bool | both shown | parent-count filter |
| `--first-parent` | bool | false | follow first parent only |
| `--body` | bool | false | include full message body |
| `--stat` | bool | false | per-commit changed-file counts |
| `--patch` | bool | false | per-commit hunks (**`textdiff` only**) |

```json
{"rev":"HEAD","commits":[
  {"oid":"…","short_oid":"a1b2c3d","parents":["…"],
   "author":{"name":"Amy","email":"…","time":"2026-08-01T10:00:00+00:00"},
   "committer":{…},"summary":"fix the thing","body":null,
   "stat":{"files":3,"additions":40,"deletions":7,"lines_capped":0}}],
 "truncated":true}
```

No `--graph` (ASCII art is a human affordance and a non-goal). No `--grep`
(regex). Date parsing accepts two unambiguous forms and rejects everything else
loudly — git's `approxidate` ("2 weeks ago") is a pattern language in disguise.

Settled while building it (PR 3), each pinned by a test against real git:

- **`rev` echoes the caller's spelling**, not the oid it resolved to. An agent
  that asked for `HEAD~2` reads `HEAD~2` back.
- **Operands take git's shape**: `git log [<rev>] [-- <path>...]`. A positional
  before `--` is a **revision, always** — where git guesses (it tries the
  string as a rev, then as a path, and errors only when it is both or neither),
  we do not. One rule and a refusal that names the other spelling beats a
  heuristic that silently answers about a path when the caller meant a branch;
  it is the same reasoning that keeps the revision grammar small and refuses
  approxidate. A revision given twice — positional plus `--rev` — is a usage
  error rather than a silent pick. `status` takes only pathspecs, on either
  side of the marker; `info` takes no operands and says so.
- **Times carry the commit's own UTC offset** (`+09:00`), not a normalized `Z`.
  Git records the zone the author was in; that is a fact about the commit, and
  the instant is identical either way.
- **`--since` / `--until` are inclusive**, and neither stops the walk. Commit
  dates are not monotonic along ancestry — a rebase or a skewed clock can date a
  child before its parent — so breaking early would silently drop history behind
  the first out-of-window commit.
- **`--path` implements git's default history simplification**, which is a
  *traversal* rule and not only a reporting one. When a commit's tree matches
  some parent's under the paths, it introduced nothing there: it is not
  reported, and the walk follows **only that parent**, pruning the branches the
  merge did not take. Both halves are load-bearing. Reporting alone hides the
  merge but still walks the discarded branch, whose commits differ from *their*
  parents and get reported — so `git log --path f` would name a change that a
  reverting merge threw away. That pruning is why the walk is ours rather than
  `gix-traverse`'s: `Simple` enqueues every parent unconditionally, and the
  choice of which parent to follow cannot be expressed as a filter over it.
- **`--stat` is first-parent, and zero for a merge**, matching git's default of
  showing no diffstat for a merge. Line counts are bounded by the embedder's
  `max_blob_bytes`: a changed file whose blob is over the cap still counts in
  `files`, contributes no lines, and is counted in **`lines_capped`** — an
  honest lower bound rather than a lie or an unbounded read. Binary files and
  gitlinks are counted the same way, as git's shortstat also leaves them out of
  its line totals.
- **`--limit` and the filters interact honestly.** A filter cannot stop the walk
  (history is sorted by none of author, path, or reliably date), so a filtered
  log walks until it fills `--limit` or exhausts history, bounded by an internal
  examined-commit cap. Hitting either sets `truncated`.

### B.4 `git diff`

The most consequential design in the read profile, and the answer to the
maintainer's open question.

**Decision: the typed change model is primary; unified patch text is a rendering
of it, produced by us, gated behind `--patch`.** `--json` is *always* the
structured shape — even with `--patch`, where hunks join the structure rather
than replacing it with a string.

Rationale: an agent that receives `{"path": …, "status": "renamed",
"additions": 12}` can decide what to read next without parsing anything; an agent
that receives patch text must re-derive that structure from a format designed for
a pager, and will do it slightly wrong (renames, mode changes, binary markers,
`\ No newline at end of file`). Patch text still matters for two real jobs —
feeding `git apply` and showing a human — so we produce it, from the same model,
with a stated fidelity target ([F.1](#f1-unified-patch-assembly)).

Endpoint selection, deliberately not git(1)'s:

| Invocation | From | To |
|---|---|---|
| `git diff` | index | worktree |
| `git diff --staged` | `HEAD` | index |
| `git diff --from <A>` | `A` | worktree |
| `git diff --to <B>` | `HEAD` | `B` |
| `git diff --from <A> --to <B>` | `A` | `B` |

Bare `git diff` is **git parity** — index→worktree, unstaged changes only
(Amy, 2026-08-01). An earlier draft made it HEAD→worktree, reasoning that the
read profile has no `add`, so the index is not something the agent manipulates
and "what have I changed" is the only question bare `diff` can be asking. That
was reversed on evidence: a five-model survey (gemini-3.1-pro-preview,
gemini-3.5-flash, deepseek-v4-pro, claude-sonnet, claude-haiku — deliberately
spanning families and ability tiers, toolie class included) was **unanimous**
that bare `git diff` means unstaged-only, and every model's review reach was
the `git status` → `git diff` → `git diff --staged` triple. Nobody expects
HEAD→worktree. Amy: "a model assuming git parity may be reaching for the
shorter option intentionally — it's been like that for decades now." Diverging
would have silently handed every one of those models a wrong mental model of
the output. HEAD→worktree is one spelling away, as `git diff --from HEAD`.

`--from`/`--to` semantics are unchanged by that call, and every result — text
and JSON — still states its endpoints; parity removes a divergence, not the
honesty. `A..B` range syntax is not accepted (`--from`/`--to` is the one
spelling; two spellings for one concept is drift).

Other flags:

| Flag | Type | Default | Meaning |
|---|---|---|---|
| `--path <PATH>` | repeatable | all | restrict to paths |
| `--name-only` | bool | false | paths only, no counts |
| `--patch` | bool | false | include hunks (**`textdiff` only**; else exit 4) |
| `--context <N>` | int | 3 | context lines for `--patch` |
| `--find-renames` / `--no-find-renames` | bool | on | **exact-match** rename detection only — see below |
| `--limit <N>` | int | 500 | max files |

```json
{"from":{"kind":"index"},
 "to":{"kind":"worktree"},
 "files":[{"path":"src/lib.rs","old_path":null,"status":"modified",
           "binary":false,"old_mode":"100644","new_mode":"100644",
           "old_oid":"…","new_oid":"…","similarity":null,
           "additions":12,"deletions":3,
           "hunks":[{"old_start":10,"old_lines":3,"new_start":10,"new_lines":12,
                     "section":"fn open(",
                     "lines":[{"op":"context","text":"…"},
                              {"op":"delete","text":"…"},
                              {"op":"insert","text":"…"}]}],
           "truncated":false}],
 "totals":{"files":1,"additions":12,"deletions":3},
 "truncated":false}
```

`op` is a word, not a sigil — JSON consumers should never have to distinguish a
leading space from an empty line. Binary files carry `"binary": true` and no
hunks; we do **not** emit git's binary patch encoding (stated non-fidelity).

**Renames are exact-match-only, and `similarity` never carries a score.**
`gix-diff`'s rename tracker is `blob`-gated and `blob` pulls `gix-command`
([A.2](#a2-feature-axes)), so `--find-renames` reports a rename when a blob oid
reappears at a new path and nothing else. Copy detection is absent entirely.
This is a real fidelity regression from git(1) and it is permanent under this
dependency set, so it is reported rather than hidden — an agent that
believes a rename was *scored* would draw wrong conclusions from `deleted` +
`added` pairs that git would have folded. **Open:** what `similarity` carries
for an exact rename — `null`, or `100` for git-shaped consumers. The field
stays in the model either way; it will never carry a computed score.

Text rendering: default is a table (`STATUS  +ADD  -DEL  PATH`); with `--patch`
the text payload is the unified patch and the structure still rides along —
`ExecResult::with_output_and_text(structured, patch_text)`, so `git diff --patch |
grep` works while `git diff --patch --json` stays structured.

### B.5 `git show`

Positional `<REV>` (default `HEAD`). Behavior is chosen by the resolved object
type, and the type is always stated in the output:

- **commit** — metadata + diff against the first parent (same model as `diff`).
- **tag** — tag metadata, then the tagged object.
- **tree** — an entry listing (same shape as `ls`).
- **blob** (`HEAD:src/lib.rs`) — the file's bytes, via
  `ExecResult::success_text_or_bytes`, capped by `limits.max_blob_bytes` with a
  loud truncation notice.

The blob form is the highest-value verb here for agents: read a file at any
revision, with no checkout, no worktree, no writes. Flags: `--name-only`,
`--stat`, `--patch` (`textdiff`), `--limit`.

### B.6 `git ls`

Tree listing at a revision — pairs with `show <rev>:<path>` to make "read the
repo at revision X" complete without touching the working tree.

| Flag | Type | Default |
|---|---|---|
| positional `<REV>` | rev | `HEAD` |
| positional `<PATH>` | path | repo root |
| `--recursive` | bool | false |
| `--limit <N>` | int | 1000 |

Rows: `{path, kind: blob|tree|commit(submodule)|symlink, mode, oid, size}`.
(`kind: "commit"` is how a submodule gitlink appears; naming it plainly beats
hiding it.)

### B.7 `git branch` / `git tag`

**Listing only in the read profile.** Creation moves to the commit profile.

`git branch [--all] [--remote] [--contains <REV>] [--merged <REV>]
[--ahead-behind] [--limit N]` → rows
`{name, kind: local|remote, oid, is_head, upstream, ahead, behind}`.
`ahead`/`behind` are `null` unless `--ahead-behind` is passed (each costs a
merge-base + revwalk; making the cost opt-in keeps the default listing cheap).

`git tag [--contains <REV>] [--limit N]` → rows
`{name, oid, kind: lightweight|annotated, target_oid, tagger, message_summary}`.

### B.8 `git blame`

| Flag | Type | Default | Meaning |
|---|---|---|---|
| positional `<PATH>` | path (required) | — | file to annotate |
| `--rev <REV>` | rev | `HEAD` | **committed** revision to blame |
| `--lines <A:B>` | typed range | whole file | 1-based inclusive line range |
| `--limit <N>` | int | 2000 | max lines |

`-L` with its regex and offset forms does not exist; `--lines 40:80` is the whole
grammar.

Blame is hand-composed (revwalk + per-commit path-limited tree diff + line
mapping — [A.2](#a2-feature-axes)) and is committed-content-only. Rather than
refuse (annoying) or quietly blame stale content (dishonest), every result
carries `"blamed_rev": "<oid>"` and `"worktree_differs": true|false`, and a
stderr note fires when it is true. Rename-following across paths is not
implemented and is reported as `"follows_renames": false` in the payload — a
capability statement, not a footnote in a README. It is a different gap from
[B.4](#b4-git-diff)'s exact-match renames: that one is permanent under this
dependency set, this one waits on a rename-aware primitive existing at the
plumbing level at all (kaish-extras#11).

Rows: `{line, oid, short_oid, author, time, orig_line, text}`.

### B.9 `git worktree list`

Read-side worktree enumeration is genuinely read-only and available today, so it
belongs in the read profile even though the rest of `worktree` does not. Rows:
`{name, path_real, path_vfs, head_oid, branch, locked, lock_reason, prunable,
prunable_reason}`. `path_vfs` is `null` when the worktree lives outside any
mount — an agent should be told when it cannot reach a path it can see.

### B.10 Output discipline

**`owns_output` is not used by any verb.** It buys a bespoke `--json` envelope,
and costs the kernel's uniform envelope, its `--help` safety net, and its
output-format accounting. The only two shapes that tempted it — patch text in a
pipe, and raw blob bytes — are already served by `with_output_and_text` and
`success_text_or_bytes`, both of which keep `--json` structured. Typed
`OutputData` with `rich_json` for the richer machine shape is the pattern for
every verb, exactly as `grep` does it in kaish core.

### B.11 Later profiles (sketch only)

**Worktree profile** — `worktree add <path> [--rev REV] [--branch NAME]
[--lock --reason TEXT]`, `worktree remove <name>`, `worktree lock/unlock
<name>`, `worktree prune [--dry-run]`. Policy leanings already visible: lock is
the default for agent-created worktrees, `remove` never takes `--force`, and
`prune` defaults to `--dry-run`. **Depends on ledger** (except `list`).

**Commit profile** — `add <paths>`, `commit -m <MSG>`, `branch create <NAME>
[--rev REV]`, `tag create <NAME> [--rev REV] [-m MSG]`. **Depends on ledger.**
The natural ledger scope for a commit is the state transition the safety
inventory already identified as the right pattern: *approve `refs/heads/x`
moving `old_oid → new_oid`, verify at redemption, fail loud if the ref moved.*

**Never** — `push`, `fetch`, `pull`, `clone` (until `remote`, if ever),
`checkout`, `switch`, `restore`, `reset`, `clean`, `rebase`, `merge`,
`cherry-pick`, `revert`, `stash`, `gc`, `commit --amend`, and anything spelled
`--force`. Plumbing (`cat-file`, `rev-parse`, `update-ref`, `hash-object`) is out
of scope permanently, not deferred.

---

## C. The profile config

### C.1 Shape

An embedder-supplied Rust struct with a builder, consumed by the tool
constructor. **No config file.** A config that decides what an agent may do must
not be reachable from inside the sandbox, and a file adds a lookup path, a parse
failure mode, and a tempting write target. Runner-up (a kaish-style TOML/`.kai`
config) declined for exactly that reason.

```rust
#[non_exhaustive]
pub struct GitConfig { /* private */ }

impl GitConfig {
    /// The default and the only constructor that needs no approvals handle.
    pub fn read_only() -> Self;

    /// Subtract a verb from whatever the profiles allow. Subtraction only —
    /// there is no `with_verb`, so no config edit can widen the surface.
    pub fn without_verb(self, verb: Verb) -> Self;

    pub fn with_tool_name(self, name: impl Into<String>) -> Self;   // default "git"
    pub fn with_limits(self, limits: Limits) -> Self;

    /// Enable a write profile. Only callable on a config that already carries
    /// an approvals handle — see C.3.
    pub fn with_profile(self, profile: Profile) -> Self;
}

#[non_exhaustive] pub enum Profile { Read, Worktree, Commit }
#[non_exhaustive] pub enum Verb { Info, Status, Log, Show, Ls, Diff, Branch, Tag,
                                  Blame, WorktreeList, /* write verbs … */ }

pub struct Limits {
    pub max_rows: usize,          // 1000
    pub max_diff_files: usize,    // 500
    pub max_blob_bytes: u64,      // 8 MiB
    pub max_hunk_bytes_per_file: u64, // 256 KiB
    pub submodule_depth: u8,      // 1
}

pub fn tool(config: GitConfig) -> Result<GitTool, ConfigError>;
```

Registration in an embedder is one line inside `Kernel::with_backend`'s
`configure_tools` closure:

```rust
tools.register(kaish_tools_git::tool(GitConfig::read_only())?);
```

### C.2 Defaults

`GitConfig::read_only()` — profile `Read`, all ten read verbs, tool name `git`,
the `Limits` above. An embedder that writes nothing gets the safe thing by
typing nothing.

### C.3 Write profiles are type-gated

`Profile::Worktree` and `Profile::Commit` are unreachable from the ordinary
constructor. **The gate is a deliberate-opt-in marker, not an approvals
handle.** An earlier draft had the config carry an `ApprovalSink`; that
predates the ledger design and is superseded. Under the ledger (§D.1 of
[approval-ledger.md](approval-ledger.md), which lives in kaish) the gate call is
`ctx.request_approval(...)` on the portable `ToolCtx` — the tool holds no
approvals handle at all, and the fail-closed default means a write verb in a
ledger-less kernel refuses at runtime regardless. So the authorization plumbing
lives where the ledger puts it, and what remains here is the *embedder's*
acknowledgement that this build is meant to be able to write:

```rust
impl GitConfig {
    /// Acknowledge that this build enables write verbs. Returns a config on
    /// which `with_profile(Profile::Worktree | Profile::Commit)` is accepted.
    /// Authorization is not here — it is `ctx.request_approval` at the gate
    /// site, per the ledger design.
    pub fn with_write_profiles_acknowledged(self) -> WritableGitConfig;
}
```

`with_profile` for a write profile exists only on the returned type, so a build
that meant to be read-only does not fail at runtime with a permission error —
it does not compile. Same instinct as
[D.1](#d1-layer-1--the-code-does-not-exist): the safe property is structural,
not checked. **Open:** the constructor's name — bikeshed at implementation.
What is decided is that it is explicitly named and carries no authorization.

### C.4 Should this be a kaish kernel concept?

**Recommendation: no, not this. Something adjacent, later, yes.**

The "which verbs are exposed" half is already a kernel concept — it *is*
`ToolSchema.subcommands`. A tool that advertises only read verbs is not
dispatchable to write verbs, because `select_leaf` has nothing to route to.
Lifting `GitConfig` into the kernel would add a second, redundant gate over the
same property.

What *is* kernel-worthy, and what git makes visible for the first time, is a
**machine-readable effects marker per verb** — something like
`ToolSchema.effects: &[Effect]` with `Reads | Writes | Network | Spawns`. Today an
embedder's policy layer can classify shell *commands* (`Kernel::classify_command`)
but cannot classify a *subcommand of a plugin tool*; the latch's `(command,
paths)` scope vocabulary can't express "this verb writes a ref" either. That gap
is real, it is general (any capability-bearing plugin has it), and it should be a
kaish PR — but it should be designed *with* the ledger, since they need a shared
vocabulary, and it is not needed by the read profile at all. Deliberately not
implemented here; it is a joint follow-up with the ledger, and the ledger design
leaves it in the same place (see the [appendix](#appendix-the-issue-backlog)).

---

## D. Read-only enforcement

Five layers. The claim is not "we check a flag" — it is that in a default build
**the code that could write does not exist**, and we have a falsifiable test that
says so.

### D.1 Layer 1 — the code does not exist

`repo.rs` exposes a single newtype:

```rust
pub struct ReadRepo { /* private: object source, ref store, index, config */ }

impl ReadRepo {
    pub fn discover(real_path: &Path, ceiling: &Path) -> Result<Self, GitError>;
    // read accessors only: head(), rev_walk(), find_object(), status(), blame_file(), …
}
```

There is no facade `Repository` to hand out ([A.2](#a2-feature-axes)):
`ReadRepo` assembles the plumbing handles itself and never lends them, so no
caller can reach a ref transaction or an index writer. Every write-capable path
lives under `verbs/write/` behind `#[cfg(feature = "worktree")]` /
`#[cfg(feature = "commit")]` and constructs a *different* handle
(`WriteRepo`, which will take an approval token as a constructor argument). In
a default build those modules are not compiled.

A grep-able invariant, enforced by a test that scans the crate source: outside
`verbs/write/`, the write-shaped plumbing does not appear — `gix-ref`'s
transaction API (`transaction`, `prepare`, `commit`), `gix-index`'s writers
(`write_to`, `write`), and `gix_object::Write`. Crude, cheap, and it fails the
day someone reaches for one in the wrong place.

### D.2 Layer 2 — isolation by construction

There is no `Permissions` to set. `gix::open::Options::isolated()` was a facade
API, and the facade is gone ([A.2](#a2-feature-axes)) — with it goes the trap
that motivated the explicit call, namely that `Permissions::default()` is
`secure()` and `secure()` is currently *identical to* `all()`. Under the
plumbing there is no cascade to opt out of, because **nothing is loaded that we
did not choose to load**: no system config, no user config, no `GIT_*`
environment reading, no attributes stack, no credential helpers. Isolation stops
being a setting and becomes the shape of the code.

Repo-local `.git/config` still has to be read — `core.repositoryformatversion`,
`extensions.objectformat`, `core.bare`, `core.worktree`,
`core.precomposeUnicode` — and it is read deliberately:

```rust
// bytes we fetched ourselves; includes are NOT followed by gix-config
let cfg = gix_config::File::from_bytes_no_includes(&bytes, meta, opts)?;
// any `include.path` / `includeIf` is resolved by us, or refused (see D.3)
```

`gix-config`'s only `std::fs` sites are the path-based comfort constructors and
the `include.path` machinery; the byte constructor reaches none of them
(feasibility §6.1). This maps exactly onto kaish's hermetic-env doctrine: the
kernel never reads `std::env`, and neither does our git. The hostile-repo
fixtures in [D.3](#d3-layer-3--the-textconvfilter-attack-surface) are what keep
it honest across pin bumps.

### D.3 Layer 3 — the textconv/filter attack surface

Repo-local `.git/config` and `.gitattributes` are attacker-controlled the moment
you open a repo you did not create — which is the *normal* case for a
codebase-analysis agent. The answer is structural: the machinery that would act
on them is not linked.

- **No build can spawn**, in any feature combination the tripwires cover:
  `gix-command`, `gix-transport` and `gix-filter` are each absent from the
  dependency tree, so there is no `std::process::Command` path to reach. A
  `diff.<driver>.textconv` or `filter.*.clean` declaration is inert text —
  nothing reads it, because nothing that could act on it exists. Enforced by
  the `cargo tree -i` tripwires ([A.4](#a4-pinning-and-the-dependency-tripwires)),
  not by intention. This is the property the facade could not give us, and it
  is why [A.2](#a2-feature-axes) went the way it did.
- **`textdiff` is not an exception.** Line hunks come from `gix-imara-diff`
  applied to blob bytes we fetched ourselves; there is no pipeline to
  configure, no driver set to empty out, and no `.gitattributes` consultation
  anywhere in the path.
- **The hostile-repo fixture** (`tests/hostile_repo.rs`) runs anyway, on every
  build: a repo whose `.git/config` declares `diff.pwn.textconv` pointing at a
  script that creates a sentinel file, and whose `.gitattributes` maps
  `* diff=pwn`. Every diff/show verb runs against it; the test asserts the
  sentinel does not exist and the output is the internal diff. Same fixture
  family covers `filter.*.clean/smudge` and `core.hooksPath` (nothing here runs
  hooks — an assertion worth pinning, since it is a *feature*). A tripwire
  proves absence in *our* graph; the fixture proves behavior, and it is what
  catches a pin bump that quietly adds an edge.
- **`include.path` escape: retired by construction.** Repo-local config can say
  `include.path = ../../../etc/…`. Because config is parsed with
  `from_bytes_no_includes` and includes are resolved by us
  ([D.2](#d2-layer-2--isolation-by-construction)), there is no library code
  path that follows one. We refuse an absolute or `..`-bearing include with a
  loud unsupported-repo error (exit 4) — a decision we make, not a behavior we
  hope gix has.

### D.4 Layer 4 — the proof: the `.git` fingerprint test

The flagship claim ("true read-only git, which command-line git cannot offer")
deserves a test that can actually fail.

`tests/readonly_fingerprint.rs`: build a fixture repo with packed objects, a
worktree with staged/unstaged/untracked changes, and multiple branches. Take a
recursive fingerprint of `.git` — every path, its size, its mtime, and its
content hash. Run **every read verb** with a representative flag matrix. Take the
fingerprint again. Assert byte-identical, mtime-identical, and no new paths.

This catches the whole class the design intent names: index stat-cache
refreshes, reflog appends, `gc --auto`, pack-index rebuilds, `commit-graph`
writes. It is the test that makes the read profile a claim rather than a hope,
and it is parameterized over the verb list, so a new verb that skips it is a
compile error in the fixture table.

### D.5 Layer 5 — the VFS boundary, stated honestly

Two facts that must not be confused, because confusing them creates false
confidence:

1. **The read profile as shipped does not read through `kaish-vfs`.** It
   operates on real host paths behind the `localfs` axis, through
   `resolve_real_path`, and is unavailable under `NoLocal`, on `MemoryFs`
   mounts, and against a non-disk-backed embedder backend. That is a property
   of *this implementation*, not of gitoxide: the "not possible" answer
   upstream (Discussion #1150) is about `gix::Repository`, which
   [A.2](#a2-feature-axes) replaces. The plumbing has a coherent byte-level
   seam — `gix-object`'s find traits, `gix-pack`'s `from_data`, `gix-index`'s
   `State::from_bytes`/`write_to`, `gix-ref`'s parsers, `gix-config`'s byte
   constructors — so a VFS-backed *second* implementation of the access traits
   is buildable, and it is what the browser build waits on
   ([A.3](#a3-wasm), kaish-extras#3). Ref **writes** are the exception:
   transactions are `gix-lock`-bound to `std::fs`, so a VFS-backed write path
   is a real design problem, deferred with the write profiles
   ([F.2](#f2-worktree-createremove)).
2. **Mounting a repo `LocalFs::read_only` does not make kaish-git read-only.**
   It makes kaish's *own* file verbs read-only on that path, which is worth
   doing, but git goes around the VFS entirely on the native path. Layers 1–4
   are what make kaish-git read-only. The embedder guide will say this in these
   words, because the opposite belief is the dangerous one.

The `GitVfs` v2 idea from the capture doc (mounting repository *objects* into
kaish's VFS, so byte budgets and output limits govern object access) points the
other direction and is compatible with all of the above — out of scope for the
read profile, tracked as kaish-extras#12.

---

## E. Integration with kaish

### E.1 Schema tree and argv routing

v1 predated `ToolSchema.subcommands` and paid for it with one flat grab-bag clap
struct. v2 uses the tree properly, with one non-obvious split:

**The clap tree exists for schema reflection. Execution parses per-verb, flat.**

```rust
// schema(): build the tree from ONLY the verbs this config enables.
let mut cmd = clap::Command::new(&self.name).about("…");
if cfg.has(Verb::Status) { cmd = cmd.subcommand(StatusArgs::command().name("status")); }
if cfg.has(Verb::WorktreeList) {
    cmd = cmd.subcommand(Command::new("worktree")
        .subcommand(WorktreeListArgs::command().name("list")));
}
schema_tree_from_clap(&cmd, &self.name, "…", EXAMPLES)
```

The schema is built from the config, so a disabled verb is not merely rejected —
it is absent from `tools --json`, from `help git`, and from completion, and
`select_leaf` has nothing to route to.

At execute time, feeding the whole `to_argv()` to a clap *tree* would break:
`to_argv()` emits a `--` before positionals, and a `--` ahead of a subcommand name
defeats clap's subcommand parsing. So:

```rust
// 1. route the verb words off the typed positionals (mirrors select_leaf)
let (verb, rest) = route(&self.schema(), &args.positional)?;
// 2. rebuild argv without them, parse with the LEAF's flat Parser
let mut leaf = args.clone();
leaf.positional = rest;
let argv = leaf.to_argv()?;                    // exit 2 on ToolArgvError
let parsed = StatusArgs::try_parse_from(once("git status".into()).chain(argv))
    .map_err(|e| ExecResult::failure(2, format!("git status: {e}")))?;
parsed.global.apply(ctx);
```

House rules that apply and are easy to get wrong:

- `#[command(flatten)] global: GlobalFlags` on **every** verb struct (the kernel
  merges root params into the leaf lookup, so `--json` binds at any depth — but
  the leaf's own clap parser must accept it too).
- **Read `Value`-typed positionals off `args.positional`**, never off the clap
  struct: `to_argv()` stringifies. Each verb struct carries an
  `#[arg(hide = true)] operands: Vec<String>` validation-only sink.
- No `trailing_var_arg` / `allow_hyphen_values` anywhere — no verb is a
  passthrough.
- **Router drift test**: for every verb path the config enables, build the argv,
  and assert our `route()` and the kernel's `select_leaf` select the same leaf.
  This is the `classify_command` anti-drift instinct applied to a plugin.

### E.2 The `resolve_real_path` bridge

Still the right seam, with one refinement v1 lacked:

```rust
let vfs_path = ctx.resolve_path(repo_arg.unwrap_or("."));
let real = ctx.backend().resolve_real_path(&vfs_path)
    .ok_or_else(|| GitError::NotRealPath(vfs_path.clone()))?;   // exit 4
// Ceiling discovery at the containing mount's real root so `git status` in a
// memory mount can never discover the host's repo two directories up.
let mount = longest_prefix_mount(ctx.backend().mounts(), &vfs_path);
let ceiling = ctx.backend().resolve_real_path(&mount.path)
    .ok_or_else(|| GitError::NotRealPath(mount.path.clone()))?;
// `ReadRepo::discover` does not stop at this one ceiling comparison — every
// directory, leaf and content-named path it goes on to open is contained the
// same way. See the invariant below.
let repo = ReadRepo::discover(&real, &ceiling)?;
```

Correction from the co-architect pass: v1 did **not** have an upward-escape bug —
it used `git2::Repository::open(&root)` (old `git_vfs.rs:49`), which opens only
an exact repo root and never searches upward; the cost was ergonomic (`git
status` from a subdirectory failed). v2 *introduces* upward discovery for
ergonomics, and the ceiling is what keeps that new capability from becoming the
escape v1 never had: gix's discovery takes ceiling directories, and we seed them
from the mount root. This needs no kaish change — `mounts()` plus
`resolve_real_path()` on the mount path is enough.

We also keep the VFS path alongside the real path in every output
(`repo_root_vfs` / `repo_root_real`, `path_vfs` on worktrees) so an agent can act
on what it sees.

**The containment invariant `discover` actually holds.** A repository owns
every byte under its own `.git`, symlinks included, and the paths it names
for itself (`commondir`'s contents, `objects/info/alternates` entries) are
attacker-controlled the moment a repository we did not create is opened — the
normal case for a codebase-analysis agent. `repo.rs` enforces containment
over three primitives, not the one comparison above:

- **Directories** — `git_dir`, `common_dir`, `work_dir`, and the ceiling
  itself: `std::fs::canonicalize` then `starts_with(ceiling)`.
- **Fixed-name leaves** under an already-canonical, already-checked parent
  (`commondir`, `config`, `shallow`, `refs`, `reftable`, `objects`,
  `worktrees`, `.gitmodules`): `lstat`, no follow; a symlinked leaf is
  resolved, ceiling-checked, and refused *before* it is read if it escapes
  (the `open_leaf` helper).
- **Content-named paths** — the `commondir` file's own contents, and each
  `objects/info/alternates` entry: `canonicalize` then ceiling-check, with
  "escapes" and "does not resolve" made indistinguishable on purpose, so the
  refusal can never be read as an existence oracle for the host (the
  `contain` / `guard_alternates` helpers).

- **Working-tree paths named by index entries** — every stage-0 path `git
  status` compares against the working tree: screened lexically per
  `/`-separated segment (each segment exactly one ordinary component, so no
  absolute, `.`, `..`, empty, NUL, or — on a platform where `\` separates — a
  segment that is secretly two), then its parent chain resolved *one component
  at a time* down from the canonical working tree root, before the leaf is
  `lstat`ed through that canonical parent (the `WorktreePaths` helper in
  `verbs/status.rs`). A leaf-only check is not enough here: `lstat` does not
  follow the final component, but the kernel resolves every component before
  it, so an entry `evil/x` under a symlinked `evil` reads a host file with no
  symlinked leaf anywhere in sight.

  The walk is component-wise rather than one `canonicalize` over the whole
  chain, for the reason `contain` exists. A whole-chain `canonicalize` answers
  `NotFound` for a symlink whose target is absent and succeeds for one whose
  target is present, so an escaping chain refused (exit 4) in the second case
  and reported an ordinary deletion (exit 0) in the first — one observable bit
  saying whether an arbitrary host path exists, with the repository choosing
  the path. Walking component-wise moves the decision onto whether a symlink is
  *present* in the chain, which the repository planted and already knows;
  escaping and dangling then give the identical refusal. A symlink that stays
  inside the working tree is legitimate and is followed.

The residual carve-out, stated honestly: a symlinked leaf that gitoxide opens
*internally* — loose objects, individual ref files, `HEAD`, packs, and the
per-directory `.gitignore` files `gix-worktree`'s ignore `Stack` reads as
`git status` descends the working tree — is not intercepted by any of the
above, because nothing here wraps every `open` gitoxide makes. The
`.gitignore` reads belong in that list and not in a footnote: they are the one
carve-out path that reaches into the *working tree* rather than `.git`, the
`Stack` consults them on every descent (`--untracked no` does not avoid them,
it only stops us reporting what they matched), and a working tree is where a
symlink is easiest to plant. They stay inside the mount only because the
working tree root is ceiling-checked; a `.gitignore` symlinked out of it would
be followed. What status opens *itself* — `.git/index`, `info/exclude`, and
every path an index entry names — is contained above and is not in the
carve-out. Closing the remainder needs platform-level containment
(`openat2(RESOLVE_BENEATH)`), which belongs in a kaish VFS seam, not this
crate — tracked as kaish #276 ("VFS seam: RESOLVE_BENEATH-scoped mount view
for symlink containment"). The TOCTOU between a canonicalize/ceiling-check
here and gitoxide's later open is inherent to that design and is at parity
with kaish's own `LocalFs`, which canonicalizes and ceiling-checks the same
way before every open.

### E.3 Blocking calls and Send-ness

gix is entirely blocking, and without `parallel` most gix types are `!Send`.
One rule dissolves both problems:

> **No gix value ever crosses an `.await`.** Every verb opens the repo, does its
> work, and produces an owned value from `model.rs`, all inside one closure.

```rust
let model: StatusModel = block_in_place_compat(|| {
    let repo = ReadRepo::discover(&real, &ceiling)?;
    verbs::status::run(&repo, &opts)          // returns owned model, no gix types
})?;
```

`block_in_place_compat` is a small helper: `tokio::task::block_in_place` on a
multi-thread runtime, a direct call on a current-thread one (where
`block_in_place` panics), with a `tracing::debug!` breadcrumb noting which path
ran. Same work, different scheduling — not a semantic fallback.

v1 held a `std::sync::Mutex` across blocking C calls on the runtime. v2 holds no
lock at all: `ReadRepo` is constructed and dropped inside the closure, per verb.
Repository-handle caching is deliberately not done in v1 of this design; if
profiling demands it, the cache stores something owned and `Send`, not a gix
handle.

**Cancellation is the one honest weakness.** gix's long operations accept a
`&AtomicBool` interrupt flag, but the portable `ToolCtx` exposes no cancellation
handle (the kernel's token is an `ExecContext` field). Until that seam exists
([G.1](#g1-toolctx-cancellation-handle-proposed)), a long `blame` or unbounded
`log` runs to completion, and `--limit` plus the kernel output cap are the only
bounds. This is stated in the embedder doc rather than papered over.
`ctx.patient(budget)` is used for `blame` and full-history `log` so the script
watchdog does not kill a legitimately slow read.

### E.4 Spans

- `#[tracing::instrument(level = "info", name = "git.verb", fields(verb, repo, rev))]`
  at each verb boundary — one span per invocation.
- A child `debug` span `git.gix` around the `block_in_place` closure, recording
  objects walked / bytes read / elapsed. This is where a slow repo shows up.
- **Not** a span per object read — the kernel deliberately uses a breadcrumb
  event rather than a span at its own hot dispatch seam, and we respect that.
- `ExecResult.baggage` is stamped with `git.repo` and `git.head_oid` so an
  embedder's trace can correlate a tool call with a repository state. Egress
  merge uses `.entry().or_insert()`, so tool-emitted entries win — safe to set.
- When the write profiles land, the ledger call sites (request / wait /
  authorize / execute / settle) are each a span, per the capture doc: the OTel
  story and the audit story are the same story.

### E.5 Error taxonomy

Exit codes, chosen to sit inside kaish's existing contract (2 = POSIX usage,
3 = kernel spill, 124 = timeout, 130 = cancel):

| Code | Class | Examples |
|---|---|---|
| 0 | success | — |
| 1 | git-level failure | no such revision; path not tracked; ambiguous oid prefix; not a repository |
| 2 | usage | clap parse failure; unknown verb; unsupported revspec form; pathspec magic; bad `--lines` range |
| 3 | *(reserved by kaish for output spill — never produced by us)* | |
| 4 | environment unsupported | path is not on a real filesystem; **reftable ref backend**; unknown `extensions.*`; `--patch` on a build without `textdiff`; repo config declares an escaping `include.path`; submodule beyond depth cap |
| 5 | verb not enabled by profile | belt-and-braces; unreachable through the schema |
| 124 / 130 | timeout / cancel | produced by the kernel, never manufactured by us |

Rules, non-negotiable:

- **Never a silent fallback.** A reftable repository produces
  `git: unsupported ref backend 'reftable' at <path> — kaish-git reads the
  'files' backend only (gitoxide gix-reftable is unimplemented)` and exit 4. It
  never falls back to reading `.git/refs` and answering with a stale or empty
  ref list. Same posture for shallow repos where a verb needs full history, and
  for a `--patch` request on a build without `textdiff`.
- **Truncation is reported**, in JSON (`truncated: true`) and on stderr.
- **Errors name the repository and the operation**, so an agent reading only
  stderr can act.

### E.6 Hermetic identity

Commit identity comes from kernel scope variables and nowhere else. Names are
git's own, so an embedder seeding them is self-documenting:

| Variable | Required for a commit | Notes |
|---|---|---|
| `GIT_AUTHOR_NAME` | yes | read via `ctx.var()` |
| `GIT_AUTHOR_EMAIL` | yes | |
| `GIT_COMMITTER_NAME` | no | defaults to author |
| `GIT_COMMITTER_EMAIL` | no | defaults to author |
| `GIT_AUTHOR_DATE` | no | RFC3339; enables reproducible fixtures/tests |
| `GIT_COMMITTER_DATE` | no | defaults to author date, else system clock |

The kernel never populates these from the OS environment, so the embedder must
seed them (`KernelConfig::with_var`). **There is no fallback**: a commit attempted
without author identity fails loud, naming the two variables to set. v1 called
`repo.signature()`, which read the host's `~/.gitconfig` through libgit2 — the
exact hermeticity leak this replaces. Nothing in the plumbing reads user or
system config ([D.2](#d2-layer-2--isolation-by-construction)), and the author
and committer signatures are fields we fill in on the commit object we build —
there is no path back to host config even if something wanted one.

---

## F. The gaps plan

### F.1 Unified patch assembly

**Decision: assemble it ourselves from the typed model, targeting
`git apply` compatibility rather than byte-identity with `git diff`.**

Emitted: `diff --git a/x b/x`, `old mode`/`new mode`, `similarity index` +
`rename from`/`rename to`, `index <old>..<new> <mode>`, `--- a/x` / `+++ b/x`,
`@@ -a,b +c,d @@ <section>`, and `\ No newline at end of file`. The `index` line
is included because `git apply -3` uses it.

Stated non-fidelity (documented in the embedder guide, not discovered later):
color; `--word-diff`; whitespace-config-driven rendering
(`diff.*.whitespace`, `core.autocrlf`); git's binary patch encoding (we emit
`Binary files a/x and b/x differ` and set `binary: true`); and **rename
detection**, which is not a heuristic divergence but an absence — exact-match
only, no similarity scoring, no copy detection ([B.4](#b4-git-diff)).

Test strategy: golden fixtures for the shape, plus an opt-in `compat-tests`
feature that, when real git is on PATH, pipes our patch through
`git apply --check` against the fixture repo and asserts it applies. That is a
falsifiable fidelity claim rather than an assertion of good intentions.

→ kaish-extras#7.

### F.2 Worktree create/remove

**Decision: implement here, against `gix-ref`/`gix-discover` plumbing, behind the
`worktree` feature — then offer it upstream.**

Rejected alternatives: waiting for upstream (unowned, and this is the pillar of
the kaibo-coder concept — v1's strongest feature); shelling out to git(1)
(forfeits every guarantee, per [A.2](#a2-feature-axes)).

Scope discipline for the first cut: non-bare repositories, files ref backend,
no submodules, linked worktrees only. The on-disk contract is small and
well-specified — a `.git` file containing `gitdir:`, and
`$GIT_DIR/worktrees/<name>/{gitdir,commondir,HEAD,ORIG_HEAD,locked}`. Everything
outside that scope is a loud exit 4. Locking is the default for
agent-created worktrees, and `prune` defaults to `--dry-run`.

Ref writes go through `gix-ref` transactions, built on `gix-lock`'s `.lock`-file
protocol against `std::fs` (feasibility §4b). That works natively and is what we
want for a worktree profile on real paths; it is a wall only for a VFS- or
wasm-backed *write* path, where reimplementing the atomicity guarantee is the
part you least want to hand-roll. Deferred with the ledger.

→ kaish-extras#8 (implement) and kaish-extras#9 (offer upstream).

### F.3 wasm and `gix-sec`

**Decision: gate wasm off with a `compile_error!` ([A.3](#a3-wasm)) and take the
one remaining upstream fix ourselves. No patched fork.**

The upstream ask has changed shape. The `gix-pack` non-mmap fallback we were
going to file **already exists**: 0.73's `from_data` constructors are generic
over `T: Deref<Target = [u8]>` for pack data, pack index and multi-pack-index
alike, and only `Bundle`'s 69-line find pipeline is mmap-typed (feasibility
§3.1–§3.3). It is the facade and `gix-odb::Store` that go straight to mmap, not
the plumbing. So the sole *compile* blocker left is `gix-sec`: its ownership
check special-cases `target_os = "wasi"` to `Ok(true)` but not bare `wasm32`,
and it reaches us only through `gix-discover` and `gix-config`. That is a
two-line cfg extension to a special case already present, and kaish-extras is
the "known downstream consumer" Byron said the work needed.

A fork pin in a workspace that also builds `kaish-web` risks two copies of a gix
crate in one graph and buys a maintenance tail for a target we cannot ship until
the VFS-backed access-trait implementation lands anyway. Upstream-first is both
the honest and the faster path.

One smaller ask survives, of the same shape as the retired one:
`gix-commitgraph::File` stores a concrete `memmap2::Mmap` and has no byte
constructor (feasibility §3.5). Impact is performance only — every consumer
takes `Option<&Graph>`, so `None` is a first-class mode — which makes it a
lower-stakes, better-precedented request than the one that turned out to be
already built.

→ kaish-extras#3 (tracking: the `gix-sec` fix plus the VFS-backed read path)
and the upstream `gix-sec` PR to GitoxideLabs.

### F.4 tree↔index diff (for `--staged`)

gix has no tree↔index diff. **Decision: build the tree's index in memory, then
tree↔tree.** It is correct and costs an in-memory index build. With bare
`git diff` now reading index→worktree ([B.4](#b4-git-diff)), this path is
exercised by `--staged` alone.

→ kaish-extras#10.

### F.5 blame limitations

No rename-following, no worktree content, no shallow history, single file. All
are reported *in the payload* (`follows_renames: false`, `worktree_differs`,
`blamed_rev`) rather than only in prose.

→ kaish-extras#11.

---

## G. kaish PR cascade

Conservative by intent. The existing seams cover almost everything: `ToolCtx`,
`KernelBackend` (including `resolve_real_path` and `mounts`), `GlobalFlags`,
`ToolSchema.subcommands`, `schema_tree_from_clap`, `ExecResult` + `OutputData` +
`rich_json`, `ctx.patient`, and `ExecResult.baggage` are all sufficient as-is.

### G.1 `ToolCtx` cancellation handle (proposed)

The one genuine gap. gix's long operations take a `&AtomicBool` interrupt flag;
the portable tool API exposes no way to learn that execution was cancelled (the
token is an `ExecContext` field, reachable only through the unsupported
`as_any_mut` downcast). Proposed shape, deliberately minimal and poll-only so it
commits the kernel to nothing:

```rust
trait ToolCtx {
    /// Whether this execution has been cancelled (kernel token, embedder
    /// token, or `ExecuteOptions::interrupt`). Cheap; safe to poll in a loop.
    /// Default impl returns false.
    fn is_cancelled(&self) -> bool { false }
}
```

A blocking tool bridges it to gix by spawning a small watcher that flips an
`AtomicBool`, or by polling directly in a progress callback. It is general (any
blocking third-party tool wants it), tiny, and defaulted so nothing breaks.

**The read profile ships without it**, degraded exactly as described in
[E.3](#e3-blocking-calls-and-send-ness). This is a follow-on PR, not a blocker.

**Land it with the ledger's `ToolCtx` work, not separately.** The ledger's own
kaish PR adds `request_approval` and friends to the same trait; two independent
PRs would reshape `ToolCtx` twice for embedders who implement it by hand.
`is_cancelled` is small enough to land in that PR, or immediately behind it.

### G.2 Nothing else is required for the read profile

Stated explicitly because it is the point: **phases 1–9 of
[H](#h-phasing) need zero kaish changes.** kaish-extras is an honest external
embedder; the read profile is the proof that the public API is sufficient for a
non-trivial plugin.

### G.3 Depends on ledger (not proposed here)

This was a demand list for the ledger designer. **The ledger design answers it
point-for-point**, so it now reads as a dependency statement:

- A portable approval API on `ToolCtx` — the ledger's `ctx.request_approval(...)`,
  which makes a plugin a first-class gate producer instead of a downcasting
  squatter. ✅ satisfied.
- A ref-shaped scope vocabulary: git's interesting resource is
  `(ref, old_oid, new_oid, reachability)`, which `(command, paths)` cannot
  express. ✅ satisfied — the ledger's resources are shaped, not path-shaped.
- Approve-a-transition-and-verify-at-redemption semantics, following the
  kernel's own `cas_overwrite` pattern. ✅ satisfied — transition conditions.
- `ToolSchema` effects markers (see [C.4](#c4-should-this-be-a-kaish-kernel-concept)) —
  the one item still open, and still a joint follow-up rather than a git PR.

### G.4 Verify-at-implementation, likely non-issues

- `MountInfo` carries the VFS mount path and `read_only` but not a real root; we
  get the real root via `resolve_real_path(mount.path)`. Sufficient — no PR. If
  it turns out not to be, it is a one-field addition.
- Out-of-tree tools cannot contribute `kaish-help` fragments. The schema's
  description + examples carry the help surface adequately. Noted, not proposed.

---

## H. Phasing

Small PRs, TDD, each independently landable and each shipping something. Every
PR runs clippy `--all-targets` clean and adds tests that can fail.

| PR | Content | Gate / proof |
|---|---|---|
| 0 | Workspace member, `Cargo.toml`, kaish pin, the twelve plumbing pins, CI job, dependency tripwires, wasm `compile_error!` guard, doc skeleton. No verbs. | tripwires green (`gix-command`/`gix-transport`/`gix-filter` each unmatched); `cargo build` for wasm fails with our message |
| 1 | `config.rs`, `model.rs` skeleton, `repo.rs` (repository assembly + ceilinged discovery), `error.rs`, `git info`, the fixture harness, **the `.git` fingerprint test** | fingerprint test green over `info`; ceiling test proves discovery cannot escape a mount |
| 2 | `git status` | conflict + untracked-mode fixtures; **the rename fixture asserts exact-match renames only**, not git's similarity behavior; fingerprint extended |
| 3 | `git log` (+ `--stat`, `--first-parent`, path filter) | limit/truncation reported; date-parse rejections are loud |
| 4 | `git ls` + `git show` (commit/tag/tree/blob, no patch) | blob byte-cap; `show HEAD:path` round-trips binary content |
| 5 | `git diff` structured (name/status + counts, `--staged` via F.4) | bare `diff` is index→worktree; endpoints stated in every result; `--patch` without `textdiff` exits 4 |
| 6 | `textdiff` feature: hunks from `gix-imara-diff` + unified-patch rendering | **hostile-textconv fixture**; `git apply --check` compat test |
| 7 | `git branch`, `git tag`, `git worktree list`, `git blame` | `worktree_differs` marker; `--ahead-behind` opt-in cost |
| 8 | `GitConfig` plumbing end-to-end, `git info` capability reporting, `docs/embedding-git.md` | disabled verb absent from `tools --json` and unroutable; router-vs-`select_leaf` drift test |
| 9 | **Publish `kaish-tools-git` 0.1.0** — read profile complete, zero kaish changes | — |

Then, in order and each gated on the one before:

10. kaish PR: `ToolCtx::is_cancelled` ([G.1](#g1-toolctx-cancellation-handle-proposed)), landed with the ledger's `ToolCtx` work → wire interrupt flags into `blame`/`log`.
11. Upstream `gix-sec` cfg fix, then the VFS-backed implementation of the access traits → narrow the wasm guard (kaish-extras#3).
12. **Ledger ships in kaish** (separate track) → `ctx.request_approval` at the gate sites, `WriteRepo`, the write-profile opt-in constructor ([C.3](#c3-write-profiles-are-type-gated)).
13. `worktree` profile (F.2) — create/remove/lock/prune, ledger-gated.
14. `commit` profile — `add`/`commit`/`branch create`/`tag create`, ledger-gated, ref-transition scoped.
15. `remote`, if ever.

---

## Appendix: the issue backlog

Per the house rule, deferrals discovered outside an active PR go to GitHub
Issues, not inline TODOs. The design-time backlog was filed on 2026-08-01 and
carries the real numbers below; two items were retired rather than filed.

**kaish-extras** (github.com/tobert/kaish-extras)

| # | Issue |
|---|---|
| [#3](https://github.com/tobert/kaish-extras/issues/3) | wasm blocked on the upstream `gix-sec` cfg fix plus a VFS-backed read path (tracking). Absorbs the two wasm items this appendix originally listed separately, and the upstream `gix-sec` ask. Its body still argues the retired `gix-pack` mmap premise — stale, see [A.3](#a3-wasm). |
| [#4](https://github.com/tobert/kaish-extras/issues/4) | the `gix` facade pulls `gix-command` unconditionally — the finding that falsified the original no-spawn premise and led to [A.2](#a2-feature-axes). |
| [#7](https://github.com/tobert/kaish-extras/issues/7) | diff: enumerate and pin known divergences from `git(1)` patch output. |
| [#8](https://github.com/tobert/kaish-extras/issues/8) | implement linked worktree create/remove/lock/prune on gix plumbing. |
| [#9](https://github.com/tobert/kaish-extras/issues/9) | offer the worktree lifecycle implementation upstream to gitoxide (tracking). |
| [#10](https://github.com/tobert/kaish-extras/issues/10) | `--staged` builds a temporary index — revisit if gix adds tree↔index diff. |
| [#11](https://github.com/tobert/kaish-extras/issues/11) | blame: follow renames (blocked on `gix-blame`). |
| [#12](https://github.com/tobert/kaish-extras/issues/12) | `GitVfs` v2 — mount repository objects into kaish's VFS (byte budgets over object access). |
| [#13](https://github.com/tobert/kaish-extras/issues/13) | repository-handle caching, if profiling shows per-verb open is hot. |
| [#14](https://github.com/tobert/kaish-extras/issues/14) | `gix` version-bump policy and the exact-pin review checklist. |

**Retired, not filed**

- **`include.path` escape handling** — retired by construction. Config is parsed
  with `from_bytes_no_includes` and includes are resolved by us, so no library
  code path follows one ([D.2](#d2-layer-2--isolation-by-construction),
  [D.3](#d3-layer-3--the-textconvfilter-attack-surface)).
- **`gix-pack` read-into-memory fallback (upstream)** — retired: `gix-pack` 0.73
  already ships the `from_data` byte constructors we were going to ask for
  ([F.3](#f3-wasm-and-gix-sec)).

**gitoxide (upstream)** — not filed yet

- `gix-sec`: extend the WASI ownership-check special case to bare `wasm32`. The
  only wasm compile blocker; tracked from our side by #3.
- `gix-commitgraph::File::from_data` — a byte constructor mirroring `gix-pack`'s.
  Performance only, since `Option<&Graph>` makes `None` first-class
  ([F.3](#f3-wasm-and-gix-sec)).
- An explicit "never spawn external filters/textconv" switch for `blob-diff` was
  on this list under the facade. We are no longer the consumer who would ask —
  nothing here links `blob` — so it is left here only in case the facade path
  ever returns.

**kaish** — not filed yet

- `ToolCtx::is_cancelled()` — portable cancellation poll for blocking tools
  ([G.1](#g1-toolctx-cancellation-handle-proposed)). Lands with the ledger's own
  `ToolCtx` PR, not separately.
- `ToolSchema` effects markers for policy layers — **design with the ledger**,
  not before ([C.4](#c4-should-this-be-a-kaish-kernel-concept)).

---

## Changelog / provenance

**2026-08-02 — co-architect notes folded into the body.** This document carried
three appended co-architect notes (Fable, 2026-08-01, plus Amy's sign-offs).
They are now part of the text, and the superseded passages they corrected are
deleted rather than left standing beside them. What changed:

- **The dependency strategy.** The `gix` facade links `gix-command`
  unconditionally, which falsified the original "no spawn in a default build"
  claim (kaish-extras#4). After a source-read verification
  ([gix-plumbing-feasibility-2026-08.md](gix-plumbing-feasibility-2026-08.md))
  the fork resolved to the plumbing crates, and the structural guarantee — plus
  the tripwires that prove it — came back with it ([A.2](#a2-feature-axes),
  [A.4](#a4-pinning-and-the-dependency-tripwires),
  [D.3](#d3-layer-3--the-textconvfilter-attack-surface)).
- **wasm.** The `gix-pack` mmap premise is retired; the blockers are `gix-sec`
  and the VFS-backed read path ([A.3](#a3-wasm), [F.3](#f3-wasm-and-gix-sec)).
- **Isolation and rename detection.** `Permissions::isolated()` was a facade API
  and is replaced by isolation by construction
  ([D.2](#d2-layer-2--isolation-by-construction)); rename detection is
  exact-match-only, which also corrects `gix-research-2026-08.md` §3(a)
  mitigation (ii) ([B.4](#b4-git-diff)).
- **Amy's sign-offs (2026-08-01).** Bare `git diff` is git parity
  ([B.4](#b4-git-diff)); the tool is named `git` and shadows external git
  ([B](#b-the-verb-surface)); status speaks porcelain letters in text and
  self-describing words in JSON ([B.2](#b2-git-status)).
- **The ledger.** `ApprovalSink` is superseded by `ctx.request_approval` on the
  portable `ToolCtx`; the type gate becomes a deliberate-opt-in marker
  ([C.3](#c3-write-profiles-are-type-gated), [G.3](#g3-depends-on-ledger-not-proposed-here)).

The notes' full text — including the reasoning that did not survive into the
body, and the fork as it stood while it was open — is in this repo's git
history: `git log --follow docs/design/architecture.md`, at and before the
commit that added this section.

**2026-08-21 — PR 4 (`git ls` + `git show`) landed; one deviation from the
literal text above, kept deliberate rather than silent.** §B.6 spells `git
ls`'s row `kind` in git's own object vocabulary, `blob|tree|commit(submodule)`.
The implementation reuses [`EntryKind`](#b2-git-status) instead —
`file`/`dir`/`symlink`/`commit`, the vocabulary `git status` already
established — rather than introduce `blob`/`tree` as second names for "file"
and "dir". AGENTS.md's "one term, one meaning" wins over matching git's own
words verbatim: the surface must not call the same idea `dir` in one verb and
`tree` in another. `EntryKind::Dir`'s doc comment, which said it is "only
produced for a collapsed untracked directory" ([B.2](#b2-git-status)), is now
false and was corrected — `ls` and `show`'s tree form both produce it for an
ordinary subtree row in a non-recursive listing. Everything else in B.5/B.6
landed as written: the revspec grammar's colon split happens in the caller
(`show`/`ls`), not in `resolve_commit`, so `log` still refuses
`<rev>:<path>` with the exact message that promises `show` supports it; bare
`@` resolves to `HEAD` (closing the L4 backlog entry); and the blob form's
cap declines the whole blob rather than serving a truncated prefix, matching
`log --stat`'s existing blob-cap discipline in `verbs/log.rs`.

**2026-08-22 — PR 8 (the embedder boundary) landed:
[`docs/embedding-git.md`](../embedding-git.md), `git info`'s capability report
pinned against the schema, and the E.1 gate made verb-set-agnostic.** Three
things worth recording:

- **The router drift test could not call `select_leaf` directly.** §E.1 asks
  for a test that "assert[s] our `route()` and the kernel's `select_leaf`
  select the same leaf." `scheduler::pipeline::select_leaf` is `pub(crate)` to
  `kaish-kernel`, unreachable even as a dev-dependency — consistent with this
  crate's "depends on nothing but `kaish-tool-api` + `kaish-types`" posture
  ([G.2](#g2-nothing-else-is-required-for-the-read-profile)), which turned out
  to hold even under a dev-only dependency add. `tests/router_kernel_drift.rs`
  builds a real `kaish_kernel::Kernel`, registers the tool, and drives it
  through `Kernel::execute` instead — the kernel's actual dispatch path,
  including `select_leaf`, exercised rather than called into directly. Every
  case there iterates `Verb::ALL`, so it needs no change when the sibling `git
  diff` PR's `Verb::Diff` lands.
- **`help git`'s Examples section was not filtered by config**, and disabling
  a verb left its example — `git info`, `git show …` — still advertised as
  usable. `EXAMPLES` is now filtered per-config (`examples_for` in
  `src/tool.rs`) before the schema is built, and two missing entries (`status`,
  `log` had none at all, so neither could ever be named in `help git` even
  when enabled) were added alongside the fix. Found by the drift test's own
  negative control, not by inspection — the gate this PR exists to build
  caught a gap in what it was gating.
- **Two claims in this document do not match the current test suite**, found
  while writing the embedder doc and checked against actual tests before being
  written down rather than assumed: the [D.3](#d3-layer-3--the-textconvfilter-attack-surface)
  hostile-textconv *behavioral* fixture ("runs anyway, on every build") does
  not exist — only the dependency-absence tripwire does, which is real and
  enforced in CI, but is a narrower claim; and [E.3](#e3-blocking-calls-and-send-ness)'s
  `ctx.patient(budget)` is not called anywhere in `src/`, so an unbounded
  `log` walk today has only `--limit` and the kernel's output cap between it
  and completion. Neither is fixed here — both are `docs/issues.md` entries,
  and `docs/embedding-git.md` states the actual, narrower guarantee rather
  than repeat this document's more optimistic framing.
