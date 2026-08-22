# kaish-git — history, autopsy, and design intent

This is the durable home for kaish's git story. The original implementation
(`kaish-tools-git`: a `git` builtin + `GitVfs` backend over libgit2) was removed
from kaish core in kaish PR #8 and will be **reinvented here** as a deliberately
shallow, safety-first git surface. kaish core keeps a one-line pointer to this
document; everything else lives and evolves here.

**Registering the reinvented crate today?** This document is history and
design intent — read [`docs/embedding-git.md`](embedding-git.md) instead for
the current API: every `GitConfig` knob, the read-only story stated as five
falsifiable layers, the mount-table requirement for a linked worktree, and
the known limitations an adoption decision should see first. Appendix A below
reproduces the *old* libgit2-era embedding guide for provenance only — it
describes `GitVfs` and a write-capable builtin, neither of which exist in the
reinvention.

## Provenance

All in the kaish repo's history — the code is recoverable, not lost:

| What | Where |
|---|---|
| Extraction into its own crate | kaish commit `bd693e5` ("extract kaish-tools-git — lift libgit2 out of the kernel") |
| Full removal (BREAKING) | kaish commit `1923155` (PR #8), released in 0.9.0 |
| Last version of the builtin | `git show 1923155^:crates/kaish-tools-git/src/git_tool.rs` (685 lines) |
| Last version of GitVfs | `git show 1923155^:crates/kaish-tools-git/src/git_vfs.rs` (1220 lines) |
| Old embedder guide | `git show 1923155^:docs/EMBEDDING-GIT.md` (reproduced in Appendix A below) |
| Orphaned crates.io name | `kaish-tools-git` 0.8.4 is still published under our ownership (kaish GH #119) — the name is reserved for the revival |

Removal rationale (from the commit message): git is a complex, heavyweight
subsystem (libgit2, a C dependency) that doesn't belong in kaish core; embedders
can provide their own; "it can probably also ship as a standalone repo someday,
which I'll probably do if it's introduced into kaibo." That someday is now, and
this repo is the standalone home.

## What we had (autopsy)

### The shape

Two halves in one crate, deliberately outside the kernel:

- **`GitVfs`** — a `kaish_vfs::Filesystem` implementation wrapping `LocalFs`
  plus a `Mutex<git2::Repository>`. File ops delegated to LocalFs (with `.git`
  hidden from directory listings); git ops as inherent methods:
  `open`/`clone`/`init`, `status`/`status_summary`, `add`/`add_path`/`reset_path`,
  `commit`, `log(count)`, `diff` (HEAD→workdir-with-index, patch text),
  `current_branch`/`branches`/`create_branch`, `checkout`, and a full worktree
  suite (`worktrees`, `worktree_add` with any committish, `remove`, `lock`,
  `unlock`, `prune`).
- **`Git` builtin** — a `Tool` written against portable `ToolCtx`, routing
  subcommands by hand: `init`, `clone`, `status`, `add`, `commit`, `log`,
  `diff`, `branch`, `checkout`, `worktree {list,add,remove,lock,unlock,prune}`.
- **The bridge** — `KernelBackend::resolve_real_path()` maps VFS paths to real
  filesystem paths where repos live. This was the key embedding abstraction
  (kaijutsu's worktrees-as-mounts pattern) and it survives in kaish today; the
  builtin refused to run anywhere `resolve_real_path` returned `None`.

### What was good (keep these ideas)

- **Worktree-first.** The worktree suite was the most complete part — list,
  add-at-any-committish, lock-with-reason, prune. That matches how agents
  actually use git (isolated worktrees per task) and is a pillar of the
  kaibo-coder concept below.
- **The `resolve_real_path` bridge** cleanly separated "where the VFS thinks
  files are" from "where the repo really is." Still the right seam.
- **Small verb set.** Ten subcommands, no plumbing. The instinct was right even
  in v1; v2 doubles down.
- **A `Filesystem` over a repo** is a genuinely good idea we barely exploited —
  v1 only wrapped the working tree. See "deeper fs hooks" below.

### What was wrong (do not repeat)

- **`checkout()` defaulted to `force=true`** — silently discarding uncommitted
  work. `checkout_with_options(target, force)` existed, but the default path a
  script hit was destructive. The exact class of foot-gun the reinvention
  exists to eliminate.
- **No read-only story.** Every mutating verb was unconditionally available.
  Nothing connected to kaish's confirmation/approval machinery.
- **Hermetic-env leak.** `repo.signature()` reads host gitconfig; libgit2 reads
  `~/.gitconfig`, system config, and credential helpers on its own. kaish's
  kernel never reads OS env — the git tool quietly did, through C code we
  didn't control.
- **Blocking C calls under async.** Sync git2 operations (clone especially —
  network I/O) ran on the tokio runtime without `block_in_place`, holding a
  `std::sync::Mutex` across them.
- **Flat flag namespace.** One clap struct held a grab-bag (`-s -m -c -b -n -f
  --oneline --porcelain --author --reason`) shared by every subcommand;
  the tool predates kaish's per-subcommand `ToolSchema.subcommands` (which
  landed in the same 0.8.0 release but was never adopted here).
- **Text-only output.** Hand-rolled imitation of git's human formats;
  no typed `OutputData`, so `--json` gave nothing structured, and the status
  rendering was incomplete (no renames, no conflicts).
- **libgit2 is a C dependency** — the stated reason for removal: build weight,
  no wasm path, an opaque surface area we couldn't feature-gate.

## Design intent for the reinvention

Captured 2026-08-01 from a design conversation with Amy. These are the
commitments; an architecture doc will follow as the design firms up.

### 1. Intentionally shallow

kaish-git is a curated subset of git, not a git clone. `diff`, `status`, `log`,
`show`-class read operations are in; **plumbing is out entirely** (no
`cat-file`, no `rev-parse` incantations, no `update-ref`), and there is **no
regex hell** — predictable, typed flags in the kaish style, per-subcommand clap
schemas, structured `--json` output from day one. bash/zsh compatibility is a
high bar and it is not our bar.

### 2. Read-only as a real, enforceable mode

The flagship use case: **kaibo wants to offer true read-only git access**, which
is practically impossible with command-line git (nearly every porcelain verb can
write somewhere — reflogs, gc, index refresh, hooks). With a pure-Rust git
library we can construct a repository handle that *cannot* write, and expose
only read verbs through the tool schema. Read-only isn't a flag we check — it's
a capability the write paths don't exist in.

### 3. A simple config that maps out the surface

A small, declarative config (embedder-supplied, kaish-style) maps which verbs
and capabilities are exposed: e.g. a `read` profile (status/log/diff/show), a
`worktree` profile adding worktree management, a `commit` profile adding
add/commit. Profiles compose; the default is the read profile. This is the
"configurable environment that is safe to operate" idea — and we suspect a
safe-subset git would serve most humans well too, but that comes later.

### 4. Pure Rust, wasm, and our TLS stack

Previous research (notes lost) identified a git crate that compiles to wasm and
can use our TLS solution — re-verified 2026-08-01: it's **gitoxide (`gix`)**,
and it's the only real candidate (git2 is C and its wasm door closed in 2020;
everything else is dead or the wrong shape). Full verified findings in
[design/gix-research-2026-08.md](design/gix-research-2026-08.md); the headlines:

- **The read profile needs zero transport features** — local-repo status, log,
  diff, blame, revwalk are fully offline. Empirically verified: the read-only
  feature set compiles with no TLS, no network crates, and the only
  build-script dependency is pure-Rust `zlib-rs`.
- **It compiles clean for `wasm32-wasip1` today** (verified by probe build) —
  but the wasm story is *compile-gated-and-known-degraded*: `gix-pack` mmaps
  packfiles and memmap2 stubs out on WASI, so packed objects fail to load at
  runtime. One small, precedented upstream fix (read-into-memory fallback in
  `gix-pack`) unblocks it, and gitoxide's maintainer has said he'd take it
  given a real downstream consumer — kaish-git would be that consumer. Note
  the browser playground (`kaish-web`) targets `wasm32-unknown-unknown`, where
  gix currently fails on a small `gix-sec` cfg gap — a second, even smaller
  upstream candidate; kaish-wasi's `wasm32-wasip1` compiles today.
- **Network later**: the bare `blocking-http-transport-reqwest` feature +
  direct reqwest dep with `rustls-no-provider` + ring as process default —
  kaibo's exact wiring, verified free of aws-lc/openssl/curl. Trap: gix's
  `-rust-tls` suffixed features and reqwest 0.13's plain `rustls` feature both
  pull aws-lc-rs (cmake).
- Two traps for the safety story: gix's `Permissions::secure()` is currently
  identical to `all()` — we must open with `isolated()` explicitly — and the
  `blob-diff` feature transitively links subprocess-spawning machinery
  (`gix-command`, driven by repo-local config and `.gitattributes`). The
  no-subprocess build wants name/status-level diff via `gix-diff`'s
  `tree_with_rewrites` instead.
- Known gix gaps to design around: no `git diff`-compatible unified-patch
  formatter (we assemble hunks ourselves), no worktree create/move/remove
  (see kaibo-coder below), no checkout/reset, no reftable (must error loudly
  on reftable repos, never answer wrong), blame is single-file/committed-only.

### 5. Deeper fs hooks

One hard constraint from the research: **gix cannot read through a virtual
filesystem** (upstream: "not possible yet") — it goes straight to `std::fs`,
so kaish-git operates on real host paths behind the `localfs` axis, and the
old `resolve_real_path` bridge remains the seam. The deeper-hooks idea
therefore points *up*, not down: `GitVfs` v2 can expose repository *objects*
into kaish's VFS — mounting a commit's tree read-only, diffing against VFS
overlays, letting kaish's byte budgets and output limits govern object access —
even though we can't push a MemoryFs *under* gix. Everything we can do to
create a configurable environment that is safe to operate.

### 6. Lean into kaish's safety facilities

kaish has grown real agent-safety machinery since v1 was written: capability
feature axes, the confirmation latch (`set -o latch`, GH #92/#96), trash-backed
deletes, VFS byte budgets, `SpillMode::Memory`, the suspendable watchdog and
`ToolCtx::patient`, hermetic env, and OpenTelemetry trace/baggage plumbing.
kaish-git should be the showcase consumer of all of it — and where the
facilities fall short, the need cascades back into kaish as PRs (same
maintainer, same day, that's the workflow).

### 7. Approvals: latch today, a ledger tomorrow

A full survey of kaish's safety facilities, with the deep-dive on the latch,
lives in [design/safety-inventory-2026-08.md](design/safety-inventory-2026-08.md).
The verdict: the latch is a well-built *confirmation* mechanism — a speed bump
for ten hardcoded filesystem gate sites — not an *authorization system*. The
gaps are structural: the gate API isn't reachable from the portable tool API
(a plugin would have to downcast through an unsupported hatch); the nonce *is*
the only record and evaporates on GC (no audit trail of who approved what,
when, under which policy); the `(command, paths)` scope vocabulary can't
express git's real resources (refs, remotes, "this push is fast-forward");
confirmation is replay-not-resume with a 60s TOCTOU window; and `set +o latch`
is reachable from script code, so the honest security model is "prevent
accident," not "constrain a confused agent." The kernel's own
`cas_overwrite` already demonstrates the right stronger pattern — approve a
*specific* state transition and verify it at redemption — which for git
becomes: approve old-oid→new-oid on a named ref, fail loud if the ref moved.

Writeable git therefore needs a deeper kaish facility as a prerequisite:

- **A double-entry ledger**: simple, not cryptographic — a correct and
  easy-to-use state machine with an authorization handoff. Every privileged
  operation posts a request entry; a matching authorization entry (human, or
  policy-automated for pre-approved classes) must exist before the operation
  proceeds; the implementation side *always* calls it, unconditionally.
- **Automatable approval side**: policy can auto-authorize scoped classes
  (e.g. "commits to branches matching `agent/*` in worktrees the agent
  created") while everything else waits for a human.
- **Spans everywhere**: nearly every ledger call site is a natural span
  boundary — request, wait, authorize, execute, settle — so the OTel story and
  the audit story are the same story.

If this lands, it lands as kaish kernel PRs (a git plugin shouldn't own the
authorization primitive), and kaish-git's write profile becomes its first
consumer.

### 8. kaibo-coder (later, tempting)

kaibo is already a strong read-only subagent; a **kaibo-coder** would be
strictly opt-in and add limited committing machinery with full control over
worktrees: the agent works in worktrees kaish-git created and locked, commits
through the ledger-gated write profile, and never touches the primary working
tree. Worktree control + shallow commit surface + approval ledger is the whole
shape — no push, no rebase, no history rewriting in v1 of that idea.

One honest gap: gix has commit creation ready, but **no worktree
create/move/remove** (our v1's strongest feature, via libgit2). Options, in
preference order: implement against `gix-ref`/`gix-discover` plumbing here,
upstream it, or delegate worktree lifecycle to the shell-out fallback tier
behind `subprocess` while everything else stays pure-Rust. Decide at
architecture time, not implementation time.

## Non-goals

- Reimplementing git porcelain faithfully (use real git via `subprocess` for that).
- Plumbing commands, `eval`-style escape hatches, pattern-language flags.
- Push/pull/fetch in the first iteration (read-only first, commits later,
  network last — if ever).
- Being a general git library — this is a *tool surface* for agents, with
  gitoxide underneath.

## Open questions

- Where the verb-profile config lives: kaish-git's tool config, or a kaish
  kernel concept other capability-bearing tools could share?
- Worktree create/remove: gix plumbing here, upstream, or shell-out tier
  (see §8).
- Unified-patch output for `diff`/`show`: hand-assemble from gix hunks (how
  faithful must it be for agent consumption? possibly less faithful than for
  humans — typed/structured diff output may serve agents better than patch
  text).
- The gix-pack mmap fallback: do we carry a patched fork until upstream lands
  it, or gate wasm builds off entirely in the meantime?
- Ledger design: kernel facility vs. tool-api trait; storage (VFS-visible under
  `/v/`? in keeping with `/v/jobs/N/latch`); relationship to `set -o latch`
  (does latch become a degenerate single-entry case of the ledger?).
- How `GitVfs` v2's object mounts interact with `resolve_real_path` and
  OverlayFs.
- Identity/signature policy for commits under hermetic env (author comes from
  kernel config, never host gitconfig).

---

## Appendix A: the original EMBEDDING-GIT.md (recovered, for reference)

Recovered verbatim from kaish `1923155^:docs/EMBEDDING-GIT.md`. APIs described
here no longer exist; kept for the embedding patterns (especially the
`resolve_real_path` worktree mapping) that inform v2.

```markdown
# Embedding kaish: Git Integration

How an embedder wires git into kaish — the `git` builtin against custom
mount layouts, and direct `GitVfs` access for repository plumbing. For the
core embedding guide (kernel construction, configuration, custom tools),
see EMBEDDING.md.

Everything here requires the **`git` capability feature** on `kaish-kernel`
(not part of the default feature set):

    kaish-kernel = { version = "0.8", features = ["git"] }

The implementation lives in the `kaish-tools-git` crate; `kaish-kernel`
re-exports `GitVfs`, `FileStatus`, `StatusSummary`, `LogEntry`, and
`WorktreeInfo` at its root when the feature is on.

## Custom Backend for Git Operations

The key to getting git operations "for free" is implementing
`resolve_real_path()` on your `KernelBackend`. It tells kaish how to map
VFS paths to the real filesystem paths where git repositories live.

(Example: kaijutsu-style worktrees — a custom backend delegating file I/O to
`LocalBackend` over a `VfsRouter` with `/mnt/repos` mounted on a worktrees
root, plus:)

    fn resolve_real_path(&self, path: &Path) -> Option<PathBuf> {
        // /mnt/repos/kaish/src/main.rs → ~/.local/share/kaijutsu/worktrees/kaish/src/main.rs
        if let Ok(rest) = path.strip_prefix("/mnt/repos") {
            return Some(self.worktrees_root.join(rest));
        }
        None
    }

### How Git Operations Work

When a script runs `git status`:

1. The `git` builtin receives the current working directory (e.g., `/mnt/repos/kaish`)
2. It calls `backend.resolve_real_path(&cwd)`
3. Your backend returns the real path (e.g., `~/.local/share/kaijutsu/worktrees/kaish`)
4. kaish opens a `GitVfs` at that real path
5. Git operations work directly on the worktree

The `git` builtin operates on the **real filesystem** — it won't work on
memory-only mounts where `resolve_real_path` returns `None`.

## Direct GitVfs Access

For lower-level git operations, use `GitVfs` directly: `open`, then
`current_branch()`, `status()`, `add(&["src/*.rs"])`, `commit(msg, author)`,
`log(n)`; worktrees via `worktrees()`, `worktree_add(name, path,
Some("branch"))`, `worktree_lock(name, Some("reason"))`, `worktree_unlock`,
`worktree_remove(name, force)`, `worktree_prune()`.

`GitVfs` also provided `clone`, `init`, `diff`, `branches`, `create_branch`,
`checkout`, `status_summary`, `add_path`, and `reset_path`.

## Best Practices

1. **Use `resolve_real_path()`** — this is the key abstraction. Map your
   VFS paths to real paths where git repos live.
2. **Direct `GitVfs` for complex operations** — for operations beyond what
   the `git` builtin provides, use `GitVfs` directly.
3. **Handle worktrees vs bare repos** — the `git` builtin works on
   worktrees (real files). If you use bare repos internally, map VFS paths
   to worktree paths, not bare repo paths.
```
