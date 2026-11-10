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

**git's own spellings, when they mean what git means** (Amy, 2026-10-01).
Terminal-Bench runs through kaijutsu showed models typing `git log --oneline`,
`git branch -a` and `git diff A B`, and each refusal cost a model call. So a
spelling git has is accepted when this build computes the answer git would
give: `-a`/`-r` on `branch`, `--cached` and two-revision operands on `diff`,
`--oneline` and `A..B` ranges on `log`, and `-p` for `--patch` on `diff`. A flag that asks for
something this build does not compute (`--graph`, `--decorate`, `log --all`) is refused, and the refusal
says it is not supported and what to type instead. Never accepted and ignored:
a flag that changes nothing reads as a flag that worked. A flag that asks for
what this build already prints is honored, not ignored: `status -s` asks for
git's short format, and the text output is that format.

The 2026-10-11 survey (`docs/design/git-survey-2026-10.md`) added
`status -s`/`--short`/`--porcelain`, `branch --show-current`,
`branch --list`/`tag --list` with git's wildcard patterns, a file list on
`git show <commit>` (git's `--name-status`; none for a merge, with the
command that answers it on stderr), and `git <verb> --help` answered from the
schema. A git command this build has no verb for is refused by name: a write
command says this git is read-only, and a read command (`rev-parse`,
`ls-files`, `blame`, `cat-file`, …) names the enabled verb that answers the
same question.

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

The write side is designed in
[design/git-write-profile.md](design/git-write-profile.md). It does not wait for
a ledger: it keeps `GitConfig::read_only()` as the default, adds a separate
`commit` profile behind the `commit` cargo feature, and leaves per-command
approval to the embedder through kaish `plan` (that document, section 2). The
double-entry ledger described in the safety inventory stays a possible kaish
facility; nothing in the write profile depends on it. Phase status is in
[embedding-git.md](embedding-git.md), "The commit profile (in progress)".

### 8. kaibo-coder (later, tempting)

kaibo is already a strong read-only subagent; a **kaibo-coder** would be
strictly opt-in and add limited committing machinery with full control over
worktrees: the agent works in worktrees kaish-git created and locked, commits
through the commit profile (design/git-write-profile.md), and never touches the primary working
tree. Worktree control + shallow commit surface + approval ledger is the whole
shape — no push, no rebase, no history rewriting in v1 of that idea.

One honest gap: gix has commit creation ready, but **no worktree
create/move/remove** (our v1's strongest feature, via libgit2). Options, in
preference order: implement against `gix-ref`/`gix-discover` plumbing here,
upstream it, or delegate worktree lifecycle to the shell-out fallback tier
behind `subprocess` while everything else stays pure-Rust. Decide at
architecture time, not implementation time.

## `git diff` — what the built verb does

The design is [architecture.md B.4](design/architecture.md#b4-git-diff); this
is what shipped, for a reader who wants the behavior without the argument.

**The structure is the answer.** Every result is a list of changed files with
a status, the modes and oids on each side, and added/deleted line counts.
`--json` carries all of it. Patch text is a rendering of the same model, built
by `--patch` under the `textdiff` build feature — see "`--patch`" below; a
build without the feature exits 4 and names it.

**Five endpoint pairs, named by flag or by git's own operands**, and every
result states which pair it used — on the first line of the text output and in
`from`/`to` in `--json`:

| Invocation | From | To |
|---|---|---|
| `git diff` | index | worktree |
| `git diff --staged` (`--cached`) | `HEAD` | index |
| `git diff --from <A>`, `git diff <A>` | `A` | worktree |
| `git diff --to <B>`, `git diff ..<B>` | `HEAD` | `B` |
| `git diff --from <A> --to <B>`, `git diff <A> <B>`, `git diff <A>..<B>` | `A` | `B` |

Bare `git diff` is git parity — unstaged changes only. The operand spellings
mean what they mean in git. Before `--`, a word is a revision when it resolves
as one and a path when the index tracks it (a glob counts); a word that is
both is refused until `--` settles it, and so is a word that is neither,
because an empty diff would read as "nothing changed". Revisions come before
paths. `A...B` compares from the merge base, which this build does not
compute, so it is refused saying so. Naming the ends twice — `git diff A
--from B`, or a revision beside `--staged` — is refused rather than guessed.

**`HEAD~1` needs kaish 0.18.** kaish 0.17 refused `~` or `^` inside an
unquoted word, so `HEAD~1` was a parse error before git ever saw it. This
build requires 0.18, where it parses; the examples here still quote it,
which works on every kaish.

```sh
kaish> git diff                                   # unstaged
kaish> git diff --staged --json                   # staged, structured
kaish> git diff v0.1.0 HEAD -- src                # two revisions, one directory
kaish> git diff 'HEAD~1'                          # HEAD~1 against the working tree
kaish> git diff src/lib.rs                        # one tracked file, unstaged
kaish> git diff --name-only                       # paths and statuses only
kaish> git diff --patch                           # the unified patch
```

The text surface is a table under the endpoint line:

```
index → worktree
STATUS  +ADD  -DEL  PATH
M       1     0     a.txt
D       0     1     keep.txt
2 files changed, 1 insertion(+), 1 deletion(-)
```

Letters in the text surface, words in `--json` — the same split `git status`
uses, and the same letters `git diff --name-status` prints, `R100` included.

### `--patch`

Needs the `textdiff` cargo feature, which is on by default. `git info` lists
it under `capabilities.features`; a build with it turned off exits 4 on
`--patch` and on `--context`, naming the feature.

An embedder may install its own patch renderer
(`docs/embedding-git.md`, "Rendering `--patch` yourself"). The text is then
that renderer's: what it looks like and whether `git apply` takes it are its
choices, and `help git diff` says so in `--patch`'s description. `--json` is
unchanged either way — it is this build's model and this build's hunks.

With it, `--patch` changes what the **text** payload is and adds to what
`--json` carries:

- **Text becomes the patch and nothing else** — no endpoint line, no summary
  — so `git diff --patch | git apply` needs no preamble skipped. The
  endpoints move to stderr and stay in `--json`.
- **`--json` gains a `hunks` array on every file**, and `op` on each line is
  a word (`context` / `delete` / `insert`), never a sigil. `text` carries no
  sigil either, so a JSON consumer never has to tell a leading space from an
  empty line.
- **`--context <N>`** sets the context lines around each hunk, default 3.
  Without `--patch` it is a usage error; there are no hunks for it to size.

```sh
kaish> git diff --patch                                  # unstaged, as a patch
kaish> git diff --patch 'HEAD~1' HEAD                   # one commit's patch
kaish> git diff --patch --context 0 -- src/lib.rs        # changes only, no context
kaish> git diff --patch --json                           # hunks in the model
kaish> git diff --patch 'HEAD~1' HEAD | git apply --check -
```

A hunk in `--json`:

```json
{"old_start":4,"old_lines":7,"new_start":4,"new_lines":7,
 "section":"fn open(path: &str) -> Result<()> {",
 "lines":[{"op":"context","text":"    let v5 = 5;"},
          {"op":"delete","text":"    let v6 = 6;"},
          {"op":"insert","text":"    let v6 = 600;"}]}
```

A line that ends its side of the file without a trailing newline carries
`"no_newline": true`, which the patch spells `\ No newline at end of file`.
The field is absent, not `false`, everywhere else.

**How close to `git diff --patch` it is.** For `--staged` and for
`--from A --to B` — the pairs where both sides are objects — the patch is
byte-identical to git's over the inputs the tests compare: an add, a delete,
an exact rename, a mode flip, a mode and content change in one file, a binary
file, both empty-file transitions, a file born empty, a trailing newline lost
on one side and on both, CRLF content, a two-hunk source file, and paths
holding a space, a double quote, a tab, and non-ASCII bytes. `--context <N>`
is compared to `git diff -U<N>` at five widths, and `git apply --check`
accepts the result. Six things it does not do:

- **No `index` line for a working-tree side.** Working-tree content has no oid
  in the model, so the line `git apply -3` reads is omitted rather than
  invented. An ordinary `git apply` does not need it.
- **No binary patch encoding.** A binary file renders
  `Binary files a/x and b/x differ`, which is also what `git diff` prints
  without `--binary` — and `git apply` refuses such a patch from git's output
  as readily as from ours.
- **Content that is not valid UTF-8** but holds no NUL byte is text to git and
  to this build, and its hunks carry U+FFFD where the invalid bytes were, so
  that patch will not apply.
- **A path that is not valid UTF-8** carries U+FFFD the same way. The tree
  walk builds every path lossily and the renderer takes a `&str`, so the bytes
  git would have C-quoted are gone before the patch is written. A path whose
  bytes *are* UTF-8 is C-quoted exactly as git quotes it, non-ASCII included.
- **Section headings use git's default rule** — the nearest preceding line
  starting with a letter, `_` or `$` — never a `diff.<driver>.xfuncname`
  pattern, which lives in `.gitattributes` and nothing here reads. Where a
  driver applies, git's heading names a different line than ours.
- **Abbreviated oids are seven characters** and are not grown when seven is
  ambiguous in the repository, where git widens them. `git apply -3` cannot
  resolve such a prefix and falls back to a direct apply; plain `git apply`
  reads no oid.

**`git log --patch` is not this.** It exits 4 in both builds, naming
`git diff --patch '<commit>~1' <commit>` as the spelling for one
commit's patch. `git log --stat` still gives every commit's counts.

### What it does not do

- **Renames are exact-match only.** A rename is a blob oid reappearing at a
  new path, so `similarity` is always `100` — measured, since the two sides
  are byte-identical — and never a score between. A file that was *edited and
  moved* has a different oid, never pairs, and is reported as a delete plus an
  add where git would fold the pair as (say) `R087`. **Copy detection does not
  exist at all.** This is permanent under this dependency set: `gix-diff`'s
  rename tracker is behind its `blob` feature, and `blob` pulls `gix-command`,
  the subprocess-spawn machinery this crate must not link.
- **No per-commit patches.** `git log --patch` exits 4 even with `textdiff`
  on: a `--limit` of commits times `max_diff_files` times
  `max_hunk_bytes_per_file` has no third cap bounding it, and adding one is a
  `GitConfig` change (docs/issues.md, "git log --patch").
- **Unmerged paths are left out.** A conflicted path has no stage 0 to
  compare, so it is skipped, counted in `unmerged`, and named on stderr. Git
  reports a `U` row instead; this surface has no unmerged row shape yet.
- **Working-tree sides carry no oid.** The content is not in the object store,
  so `old_oid`/`new_oid` are `null` there rather than an oid `git show` could
  not find. `git diff --raw` prints all-zeros in the same place.

### Bounds

`--limit` (default 500, and the embedder's `max_diff_files` is a hard ceiling
`--limit` can only lower) is applied **before** any blob is read for line
counts, so it bounds that reading and not only the reporting. It does not
bound the working-tree hashing: a comparison against the working tree hashes
every tracked file `--path` selects before the limit applies. `--path` is
applied to the candidate set before the working tree is hashed, so it is the
bound on that work. Each blob read is
bounded by `max_blob_bytes`: a file over it counts in `files`, contributes no
lines, and is marked `lines_capped`. A *working-tree* file over the cap is a
loud refusal (exit 1) rather than a skipped row, because the comparison has to
hash every tracked file to know what changed and that comparison is the whole
answer. `git status` does not follow that rule: it declines the one path,
marks it `blob_capped`, and reports every other path (`docs/issues.md`
**P16** asks whether `diff` should follow it).

Under `--patch` a third cap applies: `max_hunk_bytes_per_file` (256 KiB by
default) bounds the hunk text one file may produce. It is measured before a
hunk's lines are built, so the memory a cap would have trimmed is never
allocated, and it stops at whole hunks — a half-hunk is not a patch. A file it
cut is marked **`hunks_capped`**, with its counts still exact. A file over
`max_blob_bytes` is marked **`lines_capped`** instead, and its counts are
`null` — nothing was read, so there is nothing to count. Two caps, two flags,
one question each.

## `git branch` and `git tag` — what the built verbs do

The design is [architecture.md B.7](design/architecture.md#b7-git-branch--git-tag);
this is what shipped. A read-only tool lists. A tool with the commit profile
also creates (next section, "Creating refs").

```sh
kaish> git branch                                 # local branches
kaish> git branch --all --json                    # local and remote-tracking
kaish> git branch --contains 1a2b3c4              # which branches carry a fix
kaish> git branch --merged main                   # what is already in main
kaish> git branch --ahead-behind                  # counts against each upstream
kaish> git tag                                    # every tag
kaish> git tag --contains v0.1.0 --json           # tags descended from a tag
```

The text surface:

```
BRANCH          OID      UPSTREAM
  feature/side  1b716f1
* main          d0467d0  origin/main [ahead 1, behind 1]
  old           26c410d  origin/main
```

With `--all` the text names a remote-tracking branch `remotes/origin/main`, as
git's `-a` does, and `--remote` names it `origin/main`. A symbolic ref prints
its target and no oid: `remotes/origin/HEAD -> origin/main`.

**A branch row** is `{name, kind, oid, is_head, symref, upstream,
upstream_gone, ahead, behind}`. `kind` is `local` or `remote`, and the name has
its namespace prefix removed — `main`, `origin/main`, in `--json` under every
flag. `symref` is the short name a symbolic ref such as `origin/HEAD` points
at, or `null`. `upstream` is the short name git's
`branch.<name>.remote` plus `branch.<name>.merge` plus the remote's `fetch`
refspec resolve to; it is reported whether or not that ref exists, and
`upstream_gone` says which, the way git renders `[gone]`.

**A tag row** is `{name, oid, kind, target_oid, target_kind, tagger,
message_summary}`. The two oids are the pair git's `%(objectname)` and
`%(*objectname)` name: `oid` is the object the ref points at, `target_oid` is
what it ultimately points at once every tag object in the chain is peeled.
For a lightweight tag they are equal and `kind` is `lightweight`; there is no
tagger and no message, because there is no tag object to carry them.

### Cost, and why `--ahead-behind` is opt-in

**A plain listing reads refs and decodes no commit.** `commits_examined` in
`--json` is 0, and that is the number to watch: it reports how many commit
objects the invocation read, and three flags make it non-zero.

- **`--contains` and `--merged` are filters.** A filter has to judge every
  candidate ref *before* `--limit` truncates, or the truncation would cut rows
  the filter never looked at. Lowering `--limit` therefore does not lower
  their cost. `--contains` memoizes per commit, so asking it of forty branches
  costs the union of their histories, not the sum.
- **`--ahead-behind` is decoration**, so it runs only on the rows that survive
  truncation, and lowering `--limit` does lower its cost. It reads each
  reported branch's history and its upstream's, to the roots. That is more
  than the divergence, and it is deliberate: the cheaper walk stops at the
  common part in commit-time order, which is only correct if committer time
  increases from parent to child. It does not have to — a scripted import
  gives every commit one instant, and a clock can run backwards — and the
  cheap walk answered `behind 2` where git answers 1 in both cases. Exact
  counts cost the full read.

Every one of those walks is metered against a shared budget of **100,000
commit reads per invocation**. Passing it is a **refusal**, exit 1, naming the
limit — not a shorter listing. A filter that gave up looking would report a
branch as not matching, which is a wrong answer wearing a success code.

### What they do not do

- **No sort or filter flags beyond the ones listed.** No `--sort`, no
  `--points-at`, no glob pattern. Rows come back in full-refname order, which
  puts `refs/heads/` before `refs/remotes/` and sorts alphabetically inside
  each.
- **`git branch` does not mark a branch checked out in another working tree.**
  Git prints `+` for that; `git worktree list`'s `branch` column is where it
  lives here.
- **`refs/remotes/<remote>/HEAD` is named `origin/HEAD`**, not `origin` — a
  deliberate divergence from git's `%(refname:short)`, which shortens that one
  ref to the bare remote name. `origin` reads like a branch called origin.
- **A lightweight tag's `message_summary` is `null`.** Git's
  `%(contents:subject)` falls back to the *target commit's* subject there,
  which is a line nobody wrote about the tag.

`git worktree list` has one divergence of its own: **a bare repository is not
a row.** Git lists the bare git directory with a `bare` marker; a bare
repository has no working tree, so this listing reports only the linked
worktrees it owns.

## `git worktree list` — what the built verb does

The design is [architecture.md B.9](design/architecture.md#b9-git-worktree-list).
Read-side enumeration is genuinely read-only, so it ships in the read profile
even though create, remove, lock and prune wait on the ledger.

```sh
kaish> git worktree list
kaish> git worktree list --json
```

`worktree` is a group rather than a verb: a bare `git worktree` is a usage
error naming what the group holds. The text surface:

```
PATH                  HEAD                  STATE
/mnt/repo             main
/mnt/repo/nested/wt   wt-inside-branch
/mnt/wt-locked        wt-locked-branch      locked: held for review
/mnt/wt-gone          wt-gone-branch        prunable: the working tree directory no longer exists
```

A row is `{name, path_real, path_vfs, head_oid, branch, locked, lock_reason,
prunable, prunable_reason}`. The main working tree comes first and has
`name: null` — it has no registration under `.git/worktrees/`. The linked ones
follow in **path** order, which is git's own; a registration's name and its
path can disagree after a `git worktree move`.

### The two nulls, and what they mean

**`path_vfs` is `null` when the working tree is outside every mount.** An
agent should be told when it cannot reach a path it can see, rather than
handed a VFS path that resolves to something else.

**`prunable` is `null` for the same worktree**, and that is the part worth
reading twice. Deciding whether a registration outlived its directory means
stat-ing a path the *repository* chose, and answering that for a path outside
the mount is one bit about an arbitrary host path, per registration — the
existence oracle this crate's containment refuses everywhere else. So the row
names the path and says it did not look.

Everything else on the row still comes through, because all of it lives under
the common dir, inside the mount: the registration name, the branch, the HEAD
oid, the lock and its reason. Each of those files is opened through the same
ceiling check `git info` uses, so a `gitdir` or `HEAD` symlinked out of the
mount is refused rather than followed.

The same null covers a worktree that *is* inside the mount but whose `.git` is
a symlink pointing out of it. The recorded path is walked one component at a
time from the mount root, and every component — the `.git` leaf included — is
classified before it is traversed: a symlink that stays inside is followed, and
one that escapes or dangles ends the walk with the identical answer, so neither
can be told from the other.

## `git add`, `rm`, `mv`, `restore --staged` and `reset` — what the built verbs do

These exist only in a tool built with the `commit` feature and given a
`WritableGitConfig` (`docs/embedding-git.md`, "The commit profile"). They write
the index, and `rm` and `mv` also delete or rename the working-tree files they
name. Nothing here writes file content into the working tree. Every refusal has
five parts: the command, the reason, one exact next command, whether an
override exists and who grants it, and what was written: "Nothing was changed."
(`.git` is byte-identical) for a refusal that comes before any write.

```sh
kaish> git add src/main.rs
kaish> git add -A                    # new, changed and deleted files
kaish> git add -u                    # tracked files only
kaish> git add -f build/out.o        # past an ignore rule
kaish> git rm old.txt                # index and working tree
kaish> git rm --cached notes.txt     # index only
kaish> git rm -r old-dir
kaish> git mv old.txt new.txt
kaish> git mv a.txt b.txt dir        # several sources into a directory
kaish> git restore --staged src/main.rs
kaish> git reset                     # also: git reset HEAD, git reset -- <path>
kaish> git reset HEAD~1 -- notes.txt # that index entry from another revision; nothing moves
```

`reset --soft <rev>` and `reset --mixed <rev>` (also `reset <rev>`) move the
branch and are described under "Rewriting history" below. A plain `reset`
(also `reset HEAD` and `reset --mixed`, with no path) is a `--mixed` reset to
`HEAD`: it writes `ORIG_HEAD` and the reflog line `reset: moving to HEAD`, as
git does for these spellings (`reset @` differs: docs/issues.md W30), so it needs a committer identity and exits 2 without one. On a branch
with no commit it only empties the index, as git does, and needs no identity.
`reset -- <path>` and `reset HEAD -- <path>` write neither.

Each returns its summary in `--json` (`added`, `modified`, `removed`,
`refreshed`; `removed`, `cached`; `moved`; `unstaged`). The text surface is
git's: `add`, `mv`, `restore` and `reset` print nothing on success; `rm`
prints `rm 'path'` per path; `git add` with nothing to add prints `Nothing
specified, nothing added.` and exits 0.

What refuses, with the exit code:

| Spelling | Exit | Why |
|---|---|---|
| `add -p`, `add -i`, `restore -p`, `reset -p` | 2 | no terminal |
| `add <ignored file or directory>`, named exactly | 1 | git's hint: `git add -f <path>` |
| `add` of a nested repository the index does not track | 4 | names it |
| `add <file real git would convert>` | 4 | names the attribute or setting |
| any pathspec that matches nothing, a glob that matches only ignored files included | 1 | |
| `index` file of zero bytes | 1 | git refuses it too ("index file smaller than expected") |
| `rm` of a path with staged or local changes | 1 | `--cached` or `-f` |
| `rm <dir>` without `-r` | 1 | |
| `mv` untracked or missing source, existing destination without `-f`, a directory into itself, a missing destination parent | 1 | |
| `restore` without `--staged`, or with `--worktree` | 2 | it would write file content |
| `reset --hard`, `--merge`, `--keep` | 2 | they write working-tree files; never |
| `reset --soft` with a path; two of `--soft`, `--mixed`, `--hard`, `--merge`, `--keep` | 2 | usage |
| any of them in a merge, rebase, ... or with a split index, ... | 4 | design section 4 |
| `add`, `rm`, `mv`, `restore --staged` or `reset` without `--soft` in a repository with an executable `post-index-change` hook, with or without a hook provider | 4 | git runs the hook after it writes the index, and this shell cannot (W50) |

`reset HEAD` and `reset HEAD -- <path>` are the unstage forms. Any other first
word that names a revision, or contains `~`, `^`, `@` or `{`, is a revision:
alone it moves the branch (see "Rewriting history"), and before `-- <path>` it
copies those index entries from that revision and moves nothing.

### Where it differs from git, on purpose

- `add` refuses the whole command when a named path is ignored; git stages the
  other paths and exits 1.
- `reset` and `restore --staged` do not print "Unstaged changes after reset"
  and do not refresh the stat of entries they restore.
- `restore --staged` and `reset` give a restored entry an all-zero stat, as git
  does; an entry that already matches HEAD is left alone.
- `add` reads whole files: a file over 128 MiB refuses (git streams).
- A path that is not UTF-8 is kept as the bytes it is, in the index, in a tree
  and on disk, by every verb here. A message that names such a path shows git's
  C-quoted form (`"caf\351.txt"`), which is not the path itself; a path on the
  command line must be UTF-8, so a glob such as `*af?.txt` is how to name one.
- A partial `rm` failure (a file that cannot be deleted) stops at the first
  failure, stages the deletions that happened, and says which paths are left
  and the command to run. Git continues past the failure and warns.
- An index in version 4 is read and written back as version 2
  (docs/issues.md W24).
- `rm` and `mv` do not support `-n`, `-q`, `-v` or `-k`.

## `git commit` and `git var` — what the built verbs do

These exist only in a tool built with the `commit` feature and given a
`WritableGitConfig`. `commit` writes a commit from the index and moves the
current branch; `var` prints the identity the next commit will use. Every
refusal has five parts: the command, the reason, one exact next command,
whether an override exists and who grants it, and what was written. A refusal
that comes before any write says "Nothing was changed." A refusal that comes
after the commit's objects were written (a hook provider that rejects the
tree, or a branch that moved before the ref update) says that no ref or index
changed, that the objects remain, and that `git gc` prunes them.

```sh
kaish> git commit -m 'Fix the parser'
kaish> git commit -m 'Fix the parser' -m 'The tokenizer dropped a closing quote.'
kaish> git commit -am 'Update the docs'            # tracked changes, then commit
kaish> git commit --only src/main.rs -m 'Fix main'  # also: git commit src/main.rs -m ...
kaish> git commit -F msg.txt                         # a path in the kernel's filesystem
kaish> git commit -m 'Fix it' --trailer 'Fixes: 12' --trailer 'Co-authored-by: Ada <ada@example.com>'
kaish> git commit --allow-empty -m 'Mark the release'
kaish> git commit -m 'Fix it' --author 'Ada <ada@example.com>'
kaish> git var GIT_AUTHOR_IDENT                     # also GIT_COMMITTER_IDENT
```

The first line is git's: `[main abc1234] subject`, `[main (root-commit) abc1234] subject`
for the first commit, `[detached HEAD abc1234] subject` on a detached HEAD. An
` Author: Name <email>` line follows when the author differs from the
committer, then ` N files changed, X insertions(+), Y deletions(-)` and one
`create mode`, `delete mode` or `mode change` line per file. `-q` prints
nothing. `--json` returns `commit`, `tree`, `parent`, `branch`, `root`,
`files_changed`, `insertions`, `deletions`, `author` and `committer`.

**Message.** Each `-m` is a paragraph. The message is cleaned in git's
`whitespace` mode, which is what `git commit -m` and `-F` use when no editor
opens: trailing whitespace comes off every line, runs of blank lines collapse
to one, leading and trailing blank lines go, the text ends with one `\n`, and
lines that start with `#` stay. An empty message exits 1 ("Aborting commit due
to an empty commit message."). With neither `-m` nor `-F`: exit 2, "There is no
editor in this shell; pass -m '<message>'." A trailer is placed as
`git interpret-trailers` places it, in the last paragraph when that paragraph
is a trailer block (so `Co-authored-by` given as the last `-m` is extended, not
repeated), and in a new paragraph otherwise; a trailer equal to the one before
it is skipped. The reflog line is `commit: <first line>`, or
`commit (initial): <first line>` on an unborn branch, in both the branch's log
and `logs/HEAD`.

**`-a` and `--only`.** `-a` stages tracked modifications and deletions under
the same `index.lock` the commit holds, as `git add -u` would. `--only <paths>`,
`<paths>` and `-- <paths>` mean git's `--only`: the commit is HEAD's tree with
the named paths' working-tree content (a deleted file is removed), the other
staged changes are left staged, and the real index ends up holding the
working-tree content of exactly those paths. A path must be known to the index
or to HEAD, else exit 1. Tests compare the commit oid and the resulting index
with git's, with other staged changes, a staged-only path and a staged
deletion in play.

**Identity.** When the embedder pinned an identity, that is the author and the
committer, and no variable is read (`--author` still sets the author). Otherwise
`GIT_AUTHOR_NAME` and `GIT_AUTHOR_EMAIL` must be set in the shell; the committer
is `GIT_COMMITTER_NAME` and `GIT_COMMITTER_EMAIL`, falling back to the author's
variables field by field. `user.name` and `user.email` in a config file are
never read, and `git config user.name` or `user.email` is refused with exit 2
naming `git var GIT_AUTHOR_IDENT`. With nothing to use, `commit` and `var` exit
2 naming both variables and saying the embedder can pin an identity. Dates:
`GIT_AUTHOR_DATE` and `GIT_COMMITTER_DATE` read ISO 8601 / RFC 3339
(`2026-08-01T10:00:00+02:00`, `2026-08-01 10:00:00 +0200`, `...Z`, fractional
seconds) and git's internal form (`1785578400 +0200`, `@1785578400`). Unset, a
date is the clock. A date with no offset, and the clock, are UTC (`+0000`): this
crate reads no zone database, where git uses the local zone. The committer date
does not borrow the author's.

**What refuses.**

| Spelling | Exit | Why |
|---|---|---|
| `--amend` with neither `-m`/`-F` nor `--no-edit` | 2 | no editor |
| `--amend` the rewrite policy forbids, or a shallow repository | 2, 4 | "Rewriting history" below |
| `--amend` on a branch with no commit | 1 | nothing to amend |
| `--amend` that leaves the commit's tree equal to its parent's, without `--allow-empty` | 1 | as git |
| `--amend` of a commit with an `encoding` header or a message that is not UTF-8 | 4 | cannot carry it over |
| `--no-edit` or `--reset-author` without `--amend`; `--reset-author` with `--author` | 2 | usage |
| `--fixup`, `--squash` | 2 | no autosquash |
| `-S`, `--gpg-sign` | 2 | this shell never signs |
| `commit.gpgSign` or `tag.gpgSign` true in the repository's config | 4 | same |
| `-p`, `--interactive` | 2 | no terminal |
| `-e`, `-v`, no `-m` and no `-F` | 2 | no editor |
| `-i`, `-m` with `-F`, `-F -`, `-a` with paths, `--only` with no paths | 2 | usage |
| a `--trailer` with no key, separator or value; an `--author` that is not `Name <email>` | 2 | usage |
| an empty message | 1 | as git |
| nothing to commit (the tree equals the parent's) without `--allow-empty` | 1 | as git |
| a named path git does not know | 1 | as git |
| the branch moved while the commit was made | 1 | names the ref and both oids |
| a hook present and no hook provider; `--no-verify` with a hook present and no provider | 4 | names the hook |
| `i18n.commitEncoding`, `commit.cleanup` other than `default`/`whitespace`, `core.logAllRefUpdates=false` | 4 | the commit would differ from git's |
| a merge, rebase, ... in progress; a split or sparse index; the other states the index verbs refuse | 4 | design section 4 |
| a symbolic link or hard link in the ref's write set (ref, `.lock`, reflog, `logs/HEAD`, `packed-refs`) | 4 | design section 6.1 |
| `index.lock` or `<ref>.lock` already present | 4 | not removed |

### Where it differs from git, on purpose

- The summary shows a rename as one delete and one create; git's summary
  detects renames. The commit object is not affected.
- Line counts in the summary come from `gix-imara-diff`'s Myers, and a side
  over 16 MiB is left out of them with a note on stderr.
- A bare `--trailer 'Key'`, or one with no value, is refused; git writes `Key:`.
- Dates with no offset and the clock are UTC; git's are local.
- `commit.template`, `prepare-commit-msg` content, `trailer.*` configuration
  and `commit.status` are not applied: there is no editor and no `git
  interpret-trailers` configuration here.
- `git commit` writes no `ORIG_HEAD`, runs no `gc --auto`, and does not
  refresh the index's stat information or write its cache-tree extension (git
  rebuilds it); `-a` and `--only` rewrite the index without it.
- A pre-commit hook rejection after `-a` or `--only` leaves unreachable
  objects behind; the index and the branch do not change.

## Rewriting history — `commit --amend` and `reset <rev>`

Both replace the branch's tip, so both pass the embedder's `RewritePolicy`
(`docs/embedding-git.md`, "The rewrite policy") before anything is written.

```sh
kaish> git commit --amend -m 'Fix the parser'      # new message, the index's tree
kaish> git commit --amend --no-edit                # keep the message; stage first
kaish> git commit --amend -a --no-edit             # also stage tracked changes
kaish> git commit --amend --no-edit --reset-author # author = the shell's identity, now
kaish> git reset --soft HEAD~1                     # move the branch; index and files stay
kaish> git reset HEAD~2                            # move it and reset the index (--mixed)
```

**`--amend`.** The new commit has the old commit's parents (all of them, for a
merge), the old author (name, email and date) unless `--author` replaces name
and email or `--reset-author` makes the author the shell's identity and clock,
and a new committer. The tree is the index, or `-a`'s or `--only`'s. The
message is `-m`/`-F` (cleaned as for a commit) or, with `--no-edit`, the old
message cleaned the same way; `--trailer` applies to either. Other commit
headers are copied and a signature is dropped, as git does. The reflog line is
`commit (amend): <first line>`; no `ORIG_HEAD` is written. Hooks are as for a
commit, plus `post-rewrite`: a repository with one refuses the amend without a
hook provider (exit 4), before anything is written. The output is git's, with
` Author:` when it differs and ` Date:` (the author's) except under
`--reset-author`; `--json` adds `amended`, the old commit.

**`reset --soft <rev>` and `reset [--mixed] <rev>`.** `--soft` moves the branch
(or a detached HEAD) and nothing else. `--mixed` also puts the whole index back
to `<rev>`'s tree. The working tree is never written. Both write `ORIG_HEAD`
(the old tip) and the reflog line `reset: moving to <rev as typed>` in the
branch's log and `logs/HEAD`, as git does for a revision that is not `@`
(git logs `HEAD` for `@`, and this shell logs `@`; W30), and need an identity
for the line.
`<rev>` is what `git log` accepts: `HEAD~N`, `^`, a branch, a tag, an oid.
`--json` returns `head`, `previous`, `mode`, `branch` and `unstaged`.

**The policy.** `Refuse` forbids a move that drops commits; `Unpublished`, the
default, forbids it when a dropped commit is reachable from a `refs/remotes/*`
ref; `Allow` permits it. A move that drops nothing always passes. The refusal
is exit 2 and reads, for an amend:

> git commit --amend -m x: refused. HEAD a1b2c3d is on origin/main, and this
> shell does not rewrite published commits. Remote-tracking refs show the last
> fetch, not the remote itself. Make a new commit instead: git commit -m
> '<message>'. There is no flag that overrides this; the policy is set by the
> program embedding this shell. Nothing was changed.

For a reset it says "This reset would drop commit a1b2c3d, which is on
origin/main". Remote-tracking refs are the state at the last fetch: a commit
another clone pushed since looks unpublished. A shallow repository refuses
under `Unpublished` (exit 4). In a repository with more than 100,000 commits
to read and no commit-graph file the check gives up (exit 1) rather than
guess; `git commit-graph write --reachable` makes it short. `commit-tree`,
`update-ref`, `hash-object` and `write-tree` are refused by name, and say what
the policy is unless it is `Allow`.

### Where it differs from git, on purpose

- A `reset` from a branch with no commit to a revision deletes a stale
  `ORIG_HEAD`, as git does.
- No "Unstaged changes after reset" list; entries a `--mixed` reset restores
  get an all-zero stat.
- Amend refuses a commit with an `encoding` header or a non-UTF-8 message.
- Costs, trust and smaller differences: `docs/issues.md` W29-W32.

## Creating refs — `branch <name>`, `tag`, `switch -c`, `checkout -b`

Only a tool built with the commit profile has these (`docs/embedding-git.md`,
"The commit profile"). All of them create a ref that does not exist and
nothing else.

```sh
kaish> git branch feature-x                  # at HEAD
kaish> git branch hotfix v0.1.0              # at a start point
kaish> git tag v0.2.0                        # lightweight
kaish> git tag -a v0.2.0 -m 'Release 0.2.0'  # annotated
kaish> git tag -m 'Release 0.2.0' v0.2.0     # annotated too, as in git
kaish> git tag -a v0.2.0 -F notes.txt HEAD~1 # message from a file
kaish> git switch -c feature-x               # make it and move HEAD onto it
kaish> git checkout -b feature-x             # the same
```

**Names** follow git: not `HEAD`, no leading `-`, and git's ref-format rules
(`a..b`, `a.lock`, `a@{b`, a trailing `/`, a space and `~ ^ : ? * [ \` all
refuse, exit 2). A branch or tag that exists refuses (exit 1) with the name,
and so does a name that would sit inside another (`a` beside `a/b`).

**A branch** is made at HEAD or at `<start>`, a revision `git log` accepts. The
reflog line is `branch: Created from <start as typed>`; with no start it names
the current branch, or `HEAD` when detached, as git does. A branch needs an
identity for that line (the variables or the pinned one). An unborn HEAD has
nothing to start from (exit 1). A start that is a remote-tracking branch makes
git write tracking configuration, which this shell never does: it refuses
(exit 4) unless `--no-track` is given.

**A tag** at `<rev>` (HEAD by default) can name a commit, a tag, a tree or a
blob, given as a branch, a tag, a ref, an object id or a `~N`/`^` suffix on one
of those. `HEAD^{tree}` and `HEAD:<path>` are not parsed (exit 2, W43). `-a` makes an
annotated tag, and so do `-m` and `-F`; there is no editor, so `-a` alone
refuses. The tag object is git's, byte for byte: `object`, `type`, `tag`,
`tagger` (the committer identity, with its date), a blank line and the
message. The message is cleaned as git cleans a tag message: trailing spaces
go, lines starting with `#` go, runs of blank lines collapse, the blank lines
at both ends go. An empty message is allowed. Tags have no reflog.

**`switch -c` and `checkout -b`** make the branch at HEAD and point `HEAD` at
it. The index and every file stay as they are, local edits included. The
reflogs are git's: `branch: Created from HEAD` on the branch and
`checkout: moving from <old> to <new>` on `HEAD` (`<old>` is the full oid when
HEAD was detached). A start point must name the commit HEAD is on; for any
other, `git branch <name> <start>` makes the branch without switching to it.
`HEAD` is a start point like any other: on an unborn HEAD it refuses (exit 1).
With no start point, an unborn HEAD changes only `HEAD`. `--json` returns `branch`, `ref`,
`commit`, `previous` and `detached`.

**Refused, with the next step in the text:** `branch -d -D -m -M -c -C -f` and
`-u`/`--set-upstream-to`/`--unset-upstream`/`-t`/`--track`; `tag -d -f -s -u`
and `tag -a` without a message; `switch <branch>`, `checkout <branch>` and
`checkout <path>` (they write files); `switch -C`, `checkout -B`, `--detach`,
`--orphan`, `-f` and `-m` on either. The differences from git, and the cost of
the name check, are `docs/issues.md` W35-W47.

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
