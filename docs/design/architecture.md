# kaish-git v2 — architecture

Status: **proposed** (2026-08-01, Opus design agent); co-architect pass done same
day — see the "Co-architect notes" appendix for corrections and the
reconciliation with the approval-ledger design. Inputs: [../git.md](../git.md)
(history, autopsy, design commitments),
[gix-research-2026-08.md](gix-research-2026-08.md) (verified gitoxide findings),
[safety-inventory-2026-08.md](safety-inventory-2026-08.md) (kaish safety
facilities). This document makes the calls; it does not survey options. Where a
call was close, the runner-up is named.

The approval-ledger design is produced separately
([approval-ledger.md](approval-ledger.md)). Everything write-shaped
here is marked **depends on ledger**; the read profile is designed to need
nothing from it.

---

## 0. Decisions at a glance

| # | Decision | Where |
|---|---|---|
| 1 | One crate, `kaish-tools-git` (the reserved name), public typed model module | [A](#a-crate-layout) |
| 2 | Feature axes: `read` (default), `textdiff`, `worktree`, `commit`, `remote`, `parallel` | [A](#a-crate-layout) |
| 3 | wasm is a **compile error**, not a degraded build | [A](#a3-wasm), [F3](#f3-gix-pack-mmap-on-wasm) |
| 4 | No shell-out tier. Ever. If you want real git, run real git | [A](#a2-feature-axes) |
| 5 | Ten read verbs: `info status log show ls diff branch tag blame worktree list` | [B](#b-the-verb-surface) |
| 6 | Structured diff is the primary form; unified patch is a *rendering* of it | [B4](#b4-git-diff) |
| 7 | `owns_output` is **not used anywhere** — typed `OutputData` + `rich_json` | [B10](#b10-output-discipline) |
| 8 | Profile config: embedder-supplied Rust struct, subtractive, no config file | [C](#c-the-profile-config) |
| 9 | Write profiles are **unconstructible** without an approvals handle (type-level) | [C](#c3-write-profiles-are-type-gated) |
| 10 | Read-only proven by a `.git`-fingerprint test across the whole read surface | [D4](#d4-the-proof-the-git-fingerprint-test) |
| 11 | `Permissions::isolated()` explicitly; `secure()` is a trap | [D2](#d2-layer-2--gix-open-permissions) |
| 12 | Default build cannot spawn: `gix-command` absent, enforced by a CI tripwire | [D3](#d3-layer-3--the-textconvfilter-attack-surface) |
| 13 | clap **tree for schema reflection**, flat **per-verb parse** at execute time | [E1](#e1-schema-tree-and-argv-routing) |
| 14 | No gix value ever crosses an `.await` — makes `!Send` a non-issue | [E3](#e3-blocking-calls-and-send-ness) |
| 15 | Repo discovery is **ceilinged at the VFS mount root** | [E2](#e2-the-resolve_real_path-bridge) |
| 16 | Identity from `GIT_AUTHOR_*` / `GIT_COMMITTER_*` kernel vars, no fallback | [E6](#e6-hermetic-identity) |
| 17 | Worktree create/remove: implement here on gix plumbing, then offer upstream | [F2](#f2-worktree-createremove) |
| 18 | Exactly **one** kaish PR is proposed, and the read profile ships without it | [G](#g-kaish-pr-cascade) |
| 19 | Read profile ships as 0.1.0 in nine small PRs, zero kaish changes | [H](#h-phasing) |

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
│   ├── repo.rs       # discovery + ReadRepo newtype; the ONLY place gix::open is called
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

Mirroring kaish's own capability-axis discipline: opt-in, each compiles out
cleanly, and the default is the smallest useful thing.

| Feature | Default | Adds | Cost / risk |
|---|---|---|---|
| `read` | ✓ | `gix` with `sha1, index, revision, status, blame`; all ten read verbs | none — offline, no C toolchain but `zlib-rs`, no `gix-command` |
| `textdiff` | — | `gix/blob-diff`: line hunks, `--patch` rendering | **links `gix-command`** (subprocess-capable machinery) — see [D.3](#d3-layer-3--the-textconvfilter-attack-surface) |
| `worktree` | — | worktree create/remove/lock/prune (**depends on ledger**) | writes refs and directories |
| `commit` | — | index staging + commit (**depends on ledger**) | writes objects and refs |
| `remote` | — | `gix/blocking-http-transport-reqwest` + direct `reqwest` (`rustls-no-provider`) + `rustls/ring` | ~doubles the build; `cc` via ring; never available on wasm |
| `parallel` | — | `gix/parallel` (and LRU pack caches) | spawns threads inside an embedded kernel |

Notes that are load-bearing:

- **The `read`↔`textdiff` split is exactly the no-subprocess concern.** The
  research found `blob-diff → attributes → gix-command`. The default build
  therefore does *name/status-level* diff via `gix-diff`'s `tree_with_rewrites`
  and never links the spawn machinery. A CI tripwire (`cargo tree -p
  kaish-tools-git -i gix-command` must find nothing on default features) turns
  that from an intention into a gate. If `status` turns out to pull `attributes`
  too — plausible, and unverified in the research — the tripwire fails on the
  first PR and we deal with it then rather than shipping a quiet contradiction.
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
    "kaish-tools-git does not support wasm: gix-pack requires mmap, which is a \
     stub on WASI, so every packed object fails to load at runtime. See \
     kaish-extras GH #<wasm-tracking>. Build kaish-web without the git tool."
);
```

Rationale: gix *compiles* for `wasm32-wasip1` today and then returns
"object not found" for every object in a packfile — i.e. for essentially all
history in any real repo. That is precisely the silent-degradation failure mode
the project rejects, and a runtime error per object would be worse (it looks like
a repo problem, not a build problem). A compile error names the real cause once,
at the only moment anyone can act on it. The deliberately ugly escape feature
exists so upstream work can be exercised without a fork; it is not a supported
configuration and CI does not build it.

`kaish-web` therefore keeps its current feature set and simply does not depend on
this crate. When [F.3](#f3-gix-pack-mmap-on-wasm) lands upstream, the guard
narrows to `wasm32-unknown-unknown` (blocked separately on `gix-sec`) and
eventually disappears.

### A.4 Pinning and the dependency tripwires

`gix = "=0.86.0"` — exact-pinned, as the research recommends (0.x with monthly
breaking minors; 0.82.0 was yanked). Bumps are their own PR with the full test
suite as the gate.

CI tripwires, all `cargo tree`-based, all on the **default** feature set unless
noted:

| Tripwire | Must be | Why |
|---|---|---|
| `gix-command` | absent | no spawn path in a no-subprocess build |
| `aws-lc-sys`, `openssl-sys`, `native-tls`, `curl` | absent (incl. `--features remote`) | the cmake/C trap the research documented |
| `memmap2` | present only with a non-wasm target | catches an accidental wasm build |
| build scripts | `zlib-rs` only | the no-C-deps claim, kept honest |

---

## B. The verb surface

Conventions that apply to every verb:

- **Tool name is `git`, configurable.** Registering as `git` deliberately shadows
  external git (kaish resolves builtins before PATH). That is the point for an
  agent surface, and a footgun for a human who expected porcelain — so
  `GitConfig::with_tool_name("kgit")` exists for embedders who want both.
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
  This is where gix's `revparse-regex` feature would have been needed; refusing is
  cheaper than a regex dependency and matches "no regex hell".
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
 "gix_version":"0.86.0",
 "capabilities":{"profiles":["read"],"verbs":["info","status",…],
                 "features":["read"],"limits":{"max_rows":1000,…}}}
```

`capabilities` is discoverability, not authority: the agent learns what it may
ask for without being able to change it. `ref_backend` is where a reftable repo
gets caught early — see [E.5](#e5-error-taxonomy).

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

`index`/`worktree` each take one of
`none|added|modified|deleted|renamed|copied|typechange|untracked|ignored`.
**Words, not porcelain letters** — an agent parsing `XY` two-character codes is a
bug generator, and the letters are only compact for humans reading a terminal.
There is no `-s`/`--porcelain`/`--long`: one shape, plus `--json`. Renames are
first-class (`orig_path`) and conflicts are a boolean, both of which v1's
hand-rolled renderer got wrong.

Table rendering: `INDEX  WORKTREE  PATH` (+ `← ORIG` on renames).

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
   "author":{"name":"Amy","email":"…","time":"2026-08-01T10:00:00Z"},
   "committer":{…},"summary":"fix the thing","body":null,
   "stat":{"files":3,"additions":40,"deletions":7}}],
 "truncated":true}
```

No `--graph` (ASCII art is a human affordance and a non-goal). No `--grep`
(regex). Date parsing accepts two unambiguous forms and rejects everything else
loudly — git's `approxidate` ("2 weeks ago") is a pattern language in disguise.

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
| `git diff` | `HEAD` | worktree |
| `git diff --staged` | `HEAD` | index |
| `git diff --from <A>` | `A` | worktree |
| `git diff --to <B>` | `HEAD` | `B` |
| `git diff --from <A> --to <B>` | `A` | `B` |

Bare `git diff` meaning **HEAD→worktree** diverges from git(1) (index→worktree).
Justified because the read profile has no `add` — the index is not something the
agent manipulates, so "what have I changed" is the only question bare `diff` can
be asking. The divergence is made unmissable: every result, text and JSON,
states its endpoints. `A..B` range syntax is not accepted (`--from`/`--to` is the
one spelling; two spellings for one concept is drift).

Other flags:

| Flag | Type | Default | Meaning |
|---|---|---|---|
| `--path <PATH>` | repeatable | all | restrict to paths |
| `--name-only` | bool | false | paths only, no counts |
| `--patch` | bool | false | include hunks (**`textdiff` only**; else exit 4) |
| `--context <N>` | int | 3 | context lines for `--patch` |
| `--find-renames` / `--no-find-renames` | bool | on | rename/copy detection |
| `--limit <N>` | int | 500 | max files |

```json
{"from":{"kind":"commit","rev":"HEAD","oid":"…"},
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

gix's blame is committed-content-only. Rather than refuse (annoying) or quietly
blame stale content (dishonest), every result carries
`"blamed_rev": "<oid>"` and `"worktree_differs": true|false`, and a stderr note
fires when it is true. Rename-following across paths is absent upstream and is
reported as `"follows_renames": false` in the payload — a capability statement,
not a footnote in a README.

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

`Profile::Worktree` and `Profile::Commit` are unreachable without an approvals
handle. The mechanism (shape to be finalized against the ledger design):

```rust
impl GitConfig {
    /// Attach the embedder's approval sink. Returns a config on which
    /// `with_profile(Profile::Worktree | Profile::Commit)` is accepted.
    pub fn with_approvals(self, sink: Arc<dyn ApprovalSink>) -> ApprovingGitConfig;
}
```

`with_profile` for a write profile exists only on `ApprovingGitConfig`. A build
that forgot approvals does not fail at runtime with a permission error — it does
not compile. This is the same instinct as [D.1](#d1-layer-1--the-code-does-not-exist):
the safe property is structural, not checked.

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
vocabulary, and it is not needed by the read profile at all. Filed as an issue,
deliberately not implemented here.

---

## D. Read-only enforcement

Five layers. The claim is not "we check a flag" — it is that in a default build
**the code that could write does not exist**, and we have a falsifiable test that
says so.

### D.1 Layer 1 — the code does not exist

`repo.rs` exposes a single newtype:

```rust
pub struct ReadRepo { inner: gix::Repository }   // field private

impl ReadRepo {
    pub fn discover(real_path: &Path, ceiling: &Path) -> Result<Self, GitError>;
    // read accessors only: head(), rev_walk(), find_object(), status(), blame_file(), …
}
```

`ReadRepo` never hands out `&gix::Repository`, so no caller can reach
`commit_as`, `edit_references`, or an index writer. Every write-capable path
lives under `verbs/write/` behind `#[cfg(feature = "worktree")]` /
`#[cfg(feature = "commit")]` and constructs a *different* handle
(`WriteRepo`, which will take an approval token as a constructor argument). In
a default build those modules are not compiled.

A grep-able invariant, enforced by a test that scans the crate source: outside
`verbs/write/`, the identifiers `commit_as`, `edit_references`, `write_object`,
`index_mut` do not appear. Crude, cheap, and it fails the day someone reaches for
one in the wrong place.

### D.2 Layer 2 — gix open permissions

Every open goes through one function, and it is explicit about isolation because
the research found `Permissions::default()` is `secure()` and `secure()` is
currently *identical to* `all()`:

```rust
let mut opts = gix::open::Options::isolated();     // NOT default(), NOT secure()
// system/user/`GIT_*` config and env-driven behavior are all off; repo-local
// `.git/config` is still loaded (gix requires it for correctness) — see D.3.
```

This maps exactly onto kaish's hermetic-env doctrine: the kernel never reads
`std::env`, and neither does our git. A regression test asserts the open options
are `isolated()` (and will fail loudly if a future gix release changes what
`isolated()` includes, because the hostile-repo fixtures in D.3 exercise it).

### D.3 Layer 3 — the textconv/filter attack surface

Repo-local `.git/config` and `.gitattributes` are attacker-controlled the moment
you open a repo you did not create — which is the *normal* case for a
codebase-analysis agent. `isolated()` does not help here: repo-local config is
always loaded.

- **Default (`read`) build: no spawn is possible**, because `gix-command` is not
  in the dependency tree at all. Enforced by the `cargo tree` tripwire
  ([A.4](#a4-pinning-and-the-dependency-tripwires)), not by intention.
- **`textdiff` build**: the crate constructs the blob-diff pipeline itself with an
  **empty driver set** and no textconv, rather than accepting a
  config-derived pipeline. If, at implementation time, the pipeline cannot be
  shown by source inspection to be driver-free, `textdiff` does not ship and an
  upstream ask for an explicit no-spawn switch is filed instead. That is a real
  gate, not a hedge.
- **The hostile-repo fixture** (`tests/hostile_repo.rs`), which runs on every
  build that enables `textdiff`: a repo whose `.git/config` declares
  `diff.pwn.textconv` pointing at a script that creates a sentinel file, and
  whose `.gitattributes` maps `* diff=pwn`. Every diff/show verb runs against it;
  the test asserts the sentinel does not exist and the output is the internal
  diff. Same fixture family covers `filter.*.clean/smudge` and
  `core.hooksPath` (gix does not run hooks — an assertion worth pinning, since
  it is a *feature* here).
- **`include.path` escape**: repo-local config can `include.path = ../../../etc/…`.
  If gix's isolated permissions do not already refuse includes escaping the repo,
  we refuse to open a repo whose config declares an absolute or `..`-bearing
  include, with a loud unsupported-repo error, and file the upstream question.

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

1. gix **cannot** read through `kaish-vfs` (upstream: not possible). kaish-git
   operates on real host paths behind the `localfs` axis, through
   `resolve_real_path`. It is unavailable under `NoLocal`, on `MemoryFs`
   mounts, and against a non-disk-backed embedder backend — that is a hard
   architectural constraint, not a phase-1 shortcut.
2. Therefore **mounting a repo `LocalFs::read_only` does not make kaish-git
   read-only.** It makes kaish's *own* file verbs read-only on that path, which
   is worth doing, but gix goes around the VFS entirely. Layers 1–4 are what make
   kaish-git read-only. The embedder guide will say this in these words, because
   the opposite belief is the dangerous one.

The `GitVfs` v2 idea from the capture doc (mounting repository *objects* into
kaish's VFS, so byte budgets and output limits govern object access) points the
other direction and is compatible with all of the above — but it is out of scope
for the read profile and is filed as an issue rather than designed here.

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
exact hermeticity leak this replaces. `isolated()` open plus explicitly-passed
signatures (`commit_as`) means gix has no path back to host config even if it
wanted one.

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
(`diff.*.whitespace`, `core.autocrlf`); exact rename-detection heuristics; git's
binary patch encoding (we emit `Binary files a/x and b/x differ` and set
`binary: true`).

Test strategy: golden fixtures for the shape, plus an opt-in `compat-tests`
feature that, when real git is on PATH, pipes our patch through
`git apply --check` against the fixture repo and asserts it applies. That is a
falsifiable fidelity claim rather than an assertion of good intentions.

→ Issue: **"diff: enumerate and pin known divergences from git(1) patch output"**.

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

→ Issues: **"implement linked worktree create/remove/lock/prune on gix plumbing"**
and **"offer worktree lifecycle upstream to gitoxide"**.

### F.3 gix-pack mmap on wasm

**Decision: gate wasm off with a `compile_error!` and pursue the upstream fix
ourselves. No patched fork.**

A fork pin in a workspace that also builds `kaish-web` risks two gix versions in
one graph and buys a maintenance tail for a target we cannot ship anyway
(networking does not build for wasm either). The upstream change is small,
precedented (contributor `uberroot4` already did it), and Byron's stated
condition was a real downstream consumer — which kaish-git is. Upstream-first is
both the honest and the faster path.

→ Issues: **"no wasm support until gix-pack has a non-mmap fallback (tracking)"**
(kaish-extras) and the upstream PR to GitoxideLabs. Separately, the
`wasm32-unknown-unknown` `gix-sec` cfg gap is tracked as its own issue, since it
gates any future browser-hosted git in `kaish-web`.

### F.4 tree↔index diff (for `--staged`)

gix has no tree↔index diff. **Decision: `index_from_tree()` then tree↔tree**, as
the research suggests. It is correct and costs an in-memory index build.

→ Issue: **"`--staged` builds a temporary index; revisit if gix adds tree↔index"**.

### F.5 blame limitations

No rename-following, no worktree content, no shallow history, single file. All
are reported *in the payload* (`follows_renames: false`, `worktree_differs`,
`blamed_rev`) rather than only in prose.

→ Issue: **"blame: follow renames (blocked on gix-blame)"**.

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

### G.2 Nothing else is required for the read profile

Stated explicitly because it is the point: **phases 1–9 of
[H](#h-phasing) need zero kaish changes.** kaish-extras is an honest external
embedder; the read profile is the proof that the public API is sufficient for a
non-trivial plugin.

### G.3 Depends on ledger (not proposed here)

Listed so the ledger designer sees the demand, not as PRs to open now:

- A portable approval API on `ToolCtx` (`request_approval(...) -> Result<Approval, ExecResult>`),
  so a plugin is a first-class gate producer instead of a downcasting squatter.
- A ref-shaped scope vocabulary: git's interesting resource is
  `(ref, old_oid, new_oid, reachability)`, which `(command, paths)` cannot express.
- Approve-a-transition-and-verify-at-redemption semantics, following the
  kernel's own `cas_overwrite` pattern.
- `ToolSchema` effects markers (see [C.4](#c4-should-this-be-a-kaish-kernel-concept)) —
  design with the ledger so they share a vocabulary.

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
| 0 | Workspace member, `Cargo.toml`, kaish pin, CI job, dependency tripwires, wasm `compile_error!` guard, doc skeleton. No verbs. | tripwires green; `cargo build` for wasm fails with our message |
| 1 | `config.rs`, `model.rs` skeleton, `repo.rs` (isolated open + ceilinged discovery), `error.rs`, `git info`, the fixture harness, **the `.git` fingerprint test** | fingerprint test green over `info`; ceiling test proves discovery cannot escape a mount |
| 2 | `git status` | rename + conflict + untracked-mode fixtures; fingerprint extended |
| 3 | `git log` (+ `--stat`, `--first-parent`, path filter) | limit/truncation reported; date-parse rejections are loud |
| 4 | `git ls` + `git show` (commit/tag/tree/blob, no patch) | blob byte-cap; `show HEAD:path` round-trips binary content |
| 5 | `git diff` structured (name/status + counts, `--staged` via F.4) | endpoints stated in every result; `--patch` without `textdiff` exits 4 |
| 6 | `textdiff` feature: hunks + unified-patch rendering | **hostile-textconv fixture**; `git apply --check` compat test |
| 7 | `git branch`, `git tag`, `git worktree list`, `git blame` | `worktree_differs` marker; `--ahead-behind` opt-in cost |
| 8 | `GitConfig` plumbing end-to-end, `git info` capability reporting, `docs/embedding-git.md` | disabled verb absent from `tools --json` and unroutable; router-vs-`select_leaf` drift test |
| 9 | **Publish `kaish-tools-git` 0.1.0** — read profile complete, zero kaish changes | — |

Then, in order and each gated on the one before:

10. kaish PR: `ToolCtx::is_cancelled` ([G.1](#g1-toolctx-cancellation-handle-proposed)) → wire gix interrupt flags into `blame`/`log`.
11. Upstream: `gix-pack` non-mmap fallback → narrow the wasm guard.
12. **Ledger design lands** (separate track) → `ApprovalSink`, `WriteRepo`, `ApprovingGitConfig`.
13. `worktree` profile (F.2) — create/remove/lock/prune, ledger-gated.
14. `commit` profile — `add`/`commit`/`branch create`/`tag create`, ledger-gated, ref-transition scoped.
15. `remote`, if ever.

---

## Appendix: issues to file at design time

Per the house rule, deferrals discovered outside an active PR go to GitHub
Issues, not inline TODOs.

**kaish-extras**

1. wasm unsupported until `gix-pack` has a non-mmap fallback (tracking).
2. `wasm32-unknown-unknown`: `gix-sec` cfg gap blocks any browser-hosted git.
3. diff: enumerate and pin known divergences from `git(1)` patch output.
4. Implement linked worktree create/remove/lock/prune on gix plumbing.
5. Offer worktree lifecycle upstream to gitoxide.
6. `--staged` builds a temporary index; revisit if gix adds tree↔index diff.
7. blame: follow renames (blocked on `gix-blame`).
8. `include.path` escape handling in repo-local config — confirm gix behavior, refuse if needed.
9. `GitVfs` v2: mount repository objects into kaish's VFS (byte budgets over object access).
10. Repository-handle caching, if profiling shows per-verb open is hot.
11. `gix` version-bump policy and the exact-pin review checklist.

**gitoxide (upstream)**

12. `gix-pack`: read-into-memory fallback where mmap is unsupported (we are the downstream consumer Byron asked for).
13. `gix-sec`: extend the WASI ownership-check special case to bare `wasm32`.
14. Question: an explicit "never spawn external filters/textconv" switch for `blob-diff`.

**kaish**

15. `ToolCtx::is_cancelled()` — portable cancellation poll for blocking tools ([G.1](#g1-toolctx-cancellation-handle-proposed)).
16. `ToolSchema` effects markers for policy layers — **design with the ledger**, not before ([C.4](#c4-should-this-be-a-kaish-kernel-concept)).
---

## Co-architect notes (Fable, 2026-08-01)

Pass done after the approval-ledger design landed; findings, none blocking the
read profile:

1. **Endorsed as-is:** no shell-out tier (the reasoning — a verb wearing our
   name must keep our guarantees — beats the research doc's fallback-tier
   suggestion); wasm as `compile_error!`; `owns_output` unused; the `.git`
   fingerprint test; the dependency tripwires; bare `git diff` meaning
   HEAD→worktree (flagged for Amy's sign-off below, since it is an agent-facing
   semantic divergence from git(1), but the no-index-in-read-profile argument
   holds and every result states its endpoints).

2. **Factual correction applied in §E.2**: v1 had no upward-discovery escape
   (it used `Repository::open`, which does not search); v2's ceiling guards the
   discovery capability v2 itself introduces. The design was right; the history
   was wrong.

3. **§C.3 reconciliation with the ledger design.** `ApprovalSink` predates the
   ledger doc. Under [approval-ledger.md](approval-ledger.md) §D.1, the gate
   call is `ctx.request_approval(...)` on the portable `ToolCtx` — the tool
   does not hold an approvals handle at all, and the fail-closed default means
   a write verb in a ledger-less kernel refuses at runtime regardless. The
   type-gate on `GitConfig` therefore stops being "carry the sink" and becomes
   a deliberate-opt-in marker: `with_profile(Profile::Commit)` stays
   unreachable except through an explicitly-named constructor
   (`GitConfig::with_write_profiles_acknowledged(...)` or similar — bikeshed at
   implementation), preserving "a build that forgot approvals does not
   compile" in spirit while the actual authorization plumbing lives where the
   ledger puts it. §G.3's demand list is otherwise satisfied point-for-point
   by the ledger design (request_approval, ref-shaped resources, transition
   conditions; effects markers stay a joint follow-up).

4. **ToolCtx churn coordination.** This doc's §G.1 (`is_cancelled`, sync,
   defaulted) and the ledger's PR 5 (`request_approval` etc., async, defaulted)
   both touch the `ToolCtx` trait. Land them as one coordinated kaish PR series
   so the trait changes shape once, not twice; `is_cancelled` is small enough
   to ride in ledger PR 5 or immediately after it.

5. **For Amy's sign-off** (agent-facing semantics): (a) bare `git diff` =
   HEAD→worktree; (b) tool name `git` shadowing external git by default in
   subprocess-enabled kernels (`with_tool_name` exists, but the default is the
   decision); (c) porcelain-letter-free status words — endorsed here, but it is
   the largest muscle-memory break in the surface.

---

## Co-architect note 2 — the gix-command finding and the facade-vs-plumbing fork (Fable, 2026-08-01, pre-reboot)

**Verified during PR 0, and it falsifies a load-bearing decision.** Decision #12 /
§A.2 / §D.3 claimed a default build "cannot spawn because `gix-command` is
absent." It is not absent. The `gix` **facade** crate links `gix-command`
(pure-Rust `std::process::Command` spawn machinery) **unconditionally** — even
`gix --no-default-features` pulls it via `gix-command → gix-transport →
gix-protocol → gix`, reinforced by `gix-diff` (textconv) and `gix-filter`
(clean/smudge). Confirmed by `cargo tree -i gix-command` on the skeleton crate.
Filed as **kaish-extras#4**. The prior gix-research doc missed it because it
audited for C toolchains and network stacks; `gix-command` is pure-Rust process
spawn, invisible to that audit.

**This opens a fork. The decision is OPEN pending research (see below).**

**Path 1 — facade + runtime guarantee.** Keep `gix::Repository`; accept
`gix-command` is linked; downgrade §D.3 from "spawn code not linked" to "spawn is
unreachable at runtime, proven": repos open `Permissions::isolated()` so no
config/attribute drives a textconv/filter/transport spawn, there are no network
verbs, and the hostile-repo fixture proves no subprocess is created. Change the
§A.4 tripwire from "`gix-command` absent" (unachievable) to "no network/C
transport backend present on default features" (which passed: no
aws-lc/openssl/curl/cc). Less code, ships sooner. Loses: the structural
guarantee, the VFS hook, and the wasm path (facade goes straight to `std::fs` and
mmaps packs).

**Path 2 — plumbing layer.** Rebuild the convenience/`Repository` layer on gix's
low-level crates, skipping the facade. **Empirically verified spawn-free:** a
probe depending on `gix-object 0.63`, `gix-odb 0.83`, `gix-ref 0.66`,
`gix-traverse 0.60`, `gix-revision 0.48`, `gix-revwalk 0.34`,
`gix-commitgraph 0.38`, `gix-discover 0.54`, `gix-index 0.54`, `gix-pack 0.73`,
and `gix-diff 0.66` **without** its `blob` feature pulls **none** of
`gix-command`/`gix-transport`/`gix-filter` (`cargo tree -i` → "did not match any
packages"). Line hunks come from `gix-imara-diff` run directly on blob bytes
(skips textconv — arguably more correct for an agent: raw content). `status`/
`blame` are hand-composed (index+worktree-walk; revwalk+line-diff) since
`gix-status`/`gix-blame` pull the machinery. More code, but potentially delivers
THREE things the facade cannot, together: the genuine "spawn code not linked"
guarantee; **git-over-kaish-vfs** (§5 "deeper fs hooks" — back object storage with
VFS); and a **wasm path** as a side effect (owning pack IO = read packs through
VFS into memory instead of mmap, sidestepping the gix-pack blocker).

**Open research (LAUNCHED 2026-08-01, LOST TO REBOOT — RE-RUN):** an Opus
source-reading agent was verifying Path 2's feasibility: (1) is object access
abstractable behind `gix_object::Find` so we can back it with kaish-vfs; (2) are
gix-traverse/revision/diff generic over a custom object source; (3) can gix-pack
decode from supplied bytes (VFS) rather than mmap → wasm path; (4) how
file-coupled are gix-ref/gix-index; (5) the rebuild-surface effort tier vs the
facade. **This did not return before reboot — re-launch it.** The brief is
reconstructable from this note (versions above; sources in
`~/.cargo/registry/src/`). Until it returns, do not finalize the fork or unblock
PR 0's dependency strategy.

**Recommendation is deferred to that research.** If the readers are generic over
object storage and refs/index are tractable, Path 2 is the more-kaish bet and
worth the extra code (strong guarantee + VFS + wasm in one). If refs/index prove
hard-wired to `std::fs`, Path 1 with the runtime guarantee is the honest answer.
Amy's call once the evidence is in.
