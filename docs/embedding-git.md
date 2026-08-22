# Embedding `kaish-tools-git`

The guide a maintainer reads before registering this crate's `git` tool in
their own kaish kernel — kaibo or kaijutsu, or anyone else building a
kernel-embedding agent surface. It covers what to type, what each knob costs,
exactly what "read-only" does and does not guarantee, and what to know before
committing to it.

Everything here has a test behind it, cited by name. Where a claim does not,
that is said explicitly rather than left implicit — see "Guarantees without a
test" at the end.

## Register it

```rust
use kaish_tools_git::GitConfig;

let git = kaish_tools_git::tool(GitConfig::read_only())?;
// tools.register(git);  // inside Kernel::with_backend's configure_tools closure
```

This is `src/lib.rs`'s own module-doc example, compiled by
`cargo test --doc -p kaish-tools-git` on every run — it cannot silently rot.
`GitConfig::read_only()` is the *only* constructor: the read profile, all nine
implemented verbs (`info`, `status`, `log`, `ls`, `show`, `diff`, `branch`,
`tag`, `worktree list`), tool name `git`
(deliberately shadowing external `git` — kaish resolves builtins before
`PATH`), and the default [`Limits`](#limits). `tool()` fails at registration,
not at first use, if the config could never produce a working tool — an empty
verb set, or a tool name that is empty or contains whitespace
(`tool_rejects_a_config_with_no_verbs`, `tool_rejects_an_unusable_name`,
`src/tool.rs`).

A narrower embedder — a different command word, one verb dropped, lower
output caps — chains from there, and only ever narrows:

```rust
use kaish_tools_git::{GitConfig, Limits, Verb};

let git = kaish_tools_git::tool(
    GitConfig::read_only()
        .without_verb(Verb::Show) // no blob/tree reads from this build
        .with_tool_name("kgit")   // register beside real `git`, not over it
        .with_limits(Limits { max_rows: 200, ..Limits::default() }),
)?;
```

Also a compiled doctest (`src/lib.rs`, "Narrowing the profile"). There is no
`with_verb` — `without_verb` is the only way to change the verb set, so no
chain of calls, however long, can widen a build's surface past what
`read_only()` granted (`no_public_method_can_widen_the_verb_set`,
`src/config.rs`).

## `GitConfig`, knob by knob

| Method | Default | What it costs to change |
|---|---|---|
| `GitConfig::read_only()` | — | The only constructor. Read profile, every verb in `Verb::ALL`, name `git`, default `Limits`. |
| `.without_verb(Verb)` | none subtracted | Removes a verb from the schema, not just from dispatch — see [Verb visibility](#verb-visibility-a-disabled-verb-is-genuinely-gone). Subtracting an already-absent verb is a no-op. Subtracting every verb makes `tool()` fail at registration (`NoVerbsEnabled`). |
| `.with_tool_name(name)` | `"git"` | Registering as anything else means external `git` on `PATH` is not shadowed — a caller who types `git` gets whichever one kaish's own PATH-vs-builtin resolution picks, not necessarily this tool. An empty or whitespace-containing name is rejected at registration (`UnusableToolName`). |
| `.with_limits(Limits)` | see below | Every field is a **hard cap** — a verb's own `--limit` flag may only lower it, never raise it. Raising a limit raises real cost: `max_blob_bytes` and `max_diff_files` bound single-allocation and single-invocation work respectively (see [L3](#known-limitations) for where "per-blob bounded, not per-commit bounded" still bites). |

`Verb` is `#[non_exhaustive]` and today carries exactly the six implemented
verbs — `info`, `status`, `log`, `ls`, `show`, `diff`
(`read_only_enables_every_implemented_verb`, `src/config.rs`, and
`the_embedding_guide_names_every_verb` pins this list against `Verb::ALL` so
the two cannot drift again). A verb is added to the enum in the same PR that
implements it, never ahead of
its implementation, so there is never a schema entry for something that
cannot actually run.

### `Limits`

| Field | Default | Bounds |
|---|---|---|
| `max_rows` | 1000 | Rows any listing verb (`status`, `log`, `ls`, `show`'s tree form, `branch`, `tag`, `worktree list`) returns. Not `diff` — a diff's rows are files, and they have their own cap below. |
| `max_diff_files` | 500 | Files compared in one invocation: `git diff`'s whole result, and `log --stat`'s per-commit file list. A verb's `--limit` may lower it, never raise it. |
| `max_blob_bytes` | 8 MiB (`8 * 1024 * 1024`) | Bytes of blob content `show` will read, and of a single working-tree file `status` will hash. Over the cap: the read is declined and reported (`git show: blob '<oid>' is <size> bytes, over this build's <cap>-byte cap`), never silently truncated. |
| `max_hunk_bytes_per_file` | 256 KiB | Bytes of hunk text `git diff --patch` will produce for one file, under the `textdiff` feature. Measured before a hunk's lines are built and applied at whole-hunk granularity — a half-hunk is not a patch — so a file it cuts is marked `hunks_capped` with its counts still exact —
distinct from `lines_capped`, which means the file was not read at all. Not consulted by a build without `textdiff`, which has no hunks. |
| `submodule_depth` | 1 | Reserved; `git info`'s `submodules` count reads `.gitmodules` in the working tree only, no recursive descent exists yet to bound. |

Pinned by `limits_defaults_match_the_design` (`src/config.rs`) — a silent
drift in a default is a silent change in how much an agent gets back, so the
numbers above are asserted, not just documented.

### Verb visibility: a disabled verb is genuinely gone

`GitConfig::without_verb` does not add a runtime check that rejects the verb
when asked for by name — it removes the verb from the `ToolSchema` the tool
builds. That schema is the single source three surfaces read from, and
disabling a verb removes it from all three:

- **`tools --json` / `help git` / completion.** All three are built from
  `Kernel::tool_schemas()`, captured once at kernel construction from each
  registered tool's `.schema()`. A disabled verb is absent from the
  subcommand list (`schema_carries_only_the_enabled_verbs`, `src/tool.rs`)
  and from `help git`'s Examples section, which is filtered by the same
  config (`examples_are_filtered_by_the_config_and_every_verb_has_one`,
  `src/tool.rs`) — the negative control there confirms every *other*
  verb's example survives in the same schema, so this is not an
  accidental blanket removal.
- **Dispatch.** `route()` (the argv router) and the kernel's own subcommand
  selector are two independently written readings of the same schema-walking
  rule; `tests/router_kernel_drift.rs` builds a real `kaish_kernel::Kernel`,
  registers this tool, and drives it through `Kernel::execute` to prove they
  agree — for every disabled verb, dispatch is refused *before* it reaches
  this crate's own belt-and-braces `VerbNotEnabled` guard (exit 5), and the
  refusal does not name the verb as existing-but-disabled (an internal detail
  that would tell a probing agent more than a plain "no such command" does).
  Every other verb this same config still enables is asserted to keep working
  through the identical kernel and schema, in the same test — a check that
  only ever proves absence can pass vacuously.
- **`git info`'s own capability report.** `capabilities.verbs` in `git info`'s
  JSON output is what lets an agent ask "what can I do here" instead of
  discovering a disabled verb by being refused (architecture.md §B.1). It is
  pinned against the same schema for both the full config and every
  one-verb-narrowed config (`info_capabilities_are_pinned_to_the_schema`,
  `tests/router_kernel_drift.rs`) — it cannot report a verb the schema does
  not also offer, or omit one the schema does.

`worktree list` is the one verb spelled with two words, because `worktree` is
a schema **node** rather than a leaf — the write profiles add `add`, `remove`,
`lock` and `prune` beside `list` under the same word (architecture.md §B.11).
`Verb::WorktreeList` is what `without_verb` takes, and subtracting it removes
the whole `worktree` node: a build that cannot list working trees does not
advertise a `worktree` group with nothing in it
(`subtracting_the_verb_removes_the_worktree_node`, `tests/worktree.rs`). A
bare `git worktree` is a usage error naming what the group holds, not a
listing.

## The read-only story: five layers, stated precisely

The claim is not "a flag is checked." It is that **in a default build, the
code that could write does not exist** — and the two facts below are what
keep that from being a false sense of safety.

1. **The code does not exist.** `ReadRepo` (`src/repo.rs`) assembles its
   object store, ref store, and parsed config itself and never lends them
   out; there is no facade `Repository` handle a caller could reach a ref
   transaction or index writer through. Every write-shaped gitoxide
   identifier (`gix-ref`'s `transaction`/`prepare`/`commit`, `gix-index`'s
   `write_to`/`write`, `gix_object::Write`) is asserted absent outside a
   `verbs/write/` module that does not exist in this build
   (`write_shaped_plumbing_appears_only_under_verbs_write`,
   `tests/write_shaped_identifiers.rs`).
2. **Isolation by construction.** Nothing is loaded that this crate did not
   choose to load — no system config, no user config, no `GIT_*`
   environment reads, no credential helpers. Repo-local `.git/config` is
   read with `gix_config::File::from_bytes_no_includes`, so `include.path`
   is resolved by this crate or refused (exit 4), never followed by library
   code on our behalf.
3. **No spawn machinery, anywhere.** `gix-command`, `gix-transport`, and
   `gix-filter` are absent from the dependency tree — verified empirically
   in CI's `git-tool-dependency-tripwires` job (`.github/workflows/ci.yml`),
   which runs `cargo tree -i` for each of the three and fails the build if
   any resolves, not assumed from a feature flag. Both feature
   configurations are asserted, `textdiff` included, because a tripwire that
   only ran on the default set would say nothing about the feature. That
   means a repository's `diff.*.textconv` or `filter.*.clean` declaration is
   inert text with nothing to read it. **And that is proved as behavior, not
   only as a dependency graph.** `PwnRepo` in `tests/hostile_repo.rs` is a
   repository declaring `diff.pwn.textconv`, `diff.pwn.command`,
   `filter.pwn.clean` / `smudge` / `process` and `core.hooksPath`, with
   `.gitattributes` mapping `* diff=pwn filter=pwn` and a script per
   mechanism that plants its own sentinel file. Every verb that touches blob
   content runs against it (`no_verb_runs_textconv_a_filter_or_a_hook`) and
   none of them plants anything. The control that makes that mean something
   is `the_hostile_fixture_pwns_real_git`: the same repository, handed to
   real git, answers `git diff` with the script's output, and fires textconv,
   the external diff driver, the clean and smudge filters, and the
   pre-commit hook out of `core.hooksPath`. The tripwire says the code is not
   linked; the fixture says what a caller gets, which is the claim that
   survives a pin bump quietly adding an edge.
4. **The proof: a `.git` fingerprint.** `tests/readonly_fingerprint.rs`
   builds a repository with packed objects, multiple branches, and a dirty,
   multi-state working tree; fingerprints every path, size, mtime, and
   content hash under `.git`; runs every read verb across a representative
   flag matrix; fingerprints again; asserts byte-identical. This is the
   test that would fail if any of the above stopped being true — and
   `real_git_status_writes_to_dot_git` in the same file keeps the contrast
   honest: real command-line `git status` *does* touch `.git` (the index
   stat-cache refresh), which is exactly the class of write this crate does
   not have.
5. **The VFS boundary — and the trap.** Two facts, and they are easy to
   confuse in exactly the direction that creates false confidence:

   - **This crate does not read through `kaish-vfs`.** It operates on real
     host paths, reached through `ctx.backend().resolve_real_path()`, behind
     whatever the embedder's backend exposes there — the `localfs` axis, in
     kaish's own terms. On a backend where `resolve_real_path` returns
     `None` (a `MemoryFs` mount, a `NoLocal` kernel, any backend with no
     disk-backed notion of the path), every verb refuses with
     `GitError::NotRealPath`, **exit 4**, naming the VFS path and stating
     plainly that this crate reads through the host filesystem and cannot
     see a mount with no real path.
   - **Mounting a repository `LocalFs::read_only` does NOT make this crate
     read-only.** It makes kaish's *own* file verbs (`cat`, `write`, `mv`,
     …) read-only on that path — worth doing, and orthogonal to this. Git
     goes around the VFS entirely on the native path this crate takes, so a
     `LocalFs::read_only` mount contributes nothing to the read-only
     property claimed above. **Layers 1–4 are what make this crate
     read-only**, and layer 4's fingerprint test is what can falsify that
     claim if it stops being true. Believing the opposite — that a
     read-only mount is what protects `.git` here — is the dangerous
     misreading this section exists to head off.

## Containment: what stops discovery from walking outside your mount

`ctx.resolve_path()` still supplies the containment ceiling for repository
*discovery* — this is the one place kaish-vfs still matters even though
object reads bypass it. `git status` run from a subdirectory needs to search
upward for `.git`, and the ceiling is what stops that search from walking
past the mount that contains the path an agent named: the containing mount's
real root, from `ctx.backend().mounts()` plus `resolve_real_path()` on the
mount path, is the boundary `ReadRepo::discover` is given
(`an_outer_repo_is_invisible_from_the_mount_root`, `tests/discovery_ceiling.rs`).
Every directory a repository names for itself after that — its `git_dir`
(a `.git` *file*'s `gitdir:` line can point anywhere), its `common_dir`
(`.git/commondir`, itself a path), symlinked leaves under an already-checked
parent, `objects/info/alternates` entries — is separately ceiling-checked
against that same root, because all of them are attacker-controlled the
moment a repository was not created by the caller. `tests/hostile_repo.rs`
is the fixture suite that proves each one; see architecture.md §D.5's
"containment invariant" write-up for the full primitive-by-primitive
breakdown.

### Several allowed trees, and linked worktrees as the sharp case

An embedder's allowed set is rarely one mount. kaibo's own read scope, for
example, is `--root` plus repeatable `--allow-path` entries, computed per
call — and it also follows git worktrees off a common git dir it can reach,
resolving the "containing tree" dynamically rather than as one static path.
**A linked worktree mounted without its main repository is the case this
produces**, and it is not an edge case: `git worktree add` puts the worktree
and its main repository in *sibling* directories by default (kaibo's own PR
workflow: `~/src/wt/<repo>-<topic>` beside `~/src/<repo>`), so a kernel
launched with only the worktree in its allowed set hits this on its very
first git call.

**What happens, precisely.** A linked worktree's `.git` is not a directory —
it is a *file* whose `gitdir:` line names the worktree's private git
directory under `<main-repo>/.git/worktrees/<name>`. If the main repository
is outside the mount, that line escapes it, and this crate refuses with
`GitError::EscapesMount`, **exit 4**, naming the linked worktree (a
directory the caller already knows about) and the mount root, and stating
that nothing was read
(`a_legitimate_worktree_whose_common_dir_is_unmounted_is_refused_helpfully`,
`tests/hostile_repo.rs`). A second, less common route reaches the same
error variant: an ordinary repository whose own `.git/commondir` file (not
its `gitdir:` line) points outside the mount fails the same containment
check on the *content* of that file. Grep for either route's `what:` text
if you are debugging a refusal — `"git directory (the \`gitdir:\` line in
its .git file)"` for the first, `"common directory (.git/commondir)"` for
the second; both raise the identical `EscapesMount` variant with the
identical recovery text.

**The refusal is correct, not a bug to route around.** Under the sandbox
model, the main repository genuinely is not readable from inside this
mount, and the refusal cannot tell the honest case (a worktree PR workflow)
from the hostile one (a repository trying to point kaish-git somewhere it
was never given) — so it does not try, and it does not echo the escaping
path either: that path is repository content from a source the tool has not
decided to trust, and repeating it back would turn the refusal into a
one-bit oracle for probing the host filesystem
(`the_escape_refusal_does_not_echo_the_escaping_path`, `src/error.rs`).

**What the message hands you instead is a command, run in a place you
trust:**

```
git rev-parse --git-common-dir --path-format=absolute
```

Run **inside the worktree that was refused** — the command answers relative
to `cwd`, and running it anywhere else gives a plausible-looking answer for
the wrong repository. One command answers both routes above: a linked
worktree's `gitdir:` target always nests under
`<common-dir>/worktrees/<name>`, so the directory the command reports is
what needs mounting either way. Verified on a real worktree:

```
gitdir:  ~/src/<repo>/.git/worktrees/<name>
common:  ~/src/<repo>/.git
```

Mount what it reports, and retry.

**This is a mount-table requirement your embedder must satisfy — this crate
does not, and cannot, arrange it for you.** It has no way to add a mount to
your backend's allowed set on your behalf; it can only tell you, honestly
and without leaking anything, that one is missing.

**The cost is not free, and is worth stating alongside the fix.** If your
allowed-set entries double as your kaish mount table — as they do for a
kernel where the same paths gate both this tool's discovery ceiling and
`cat`/`grep`/every other file-reading builtin — then "mount the common dir"
does not only grant the git verbs access to `.git`. It makes the whole
`.git` directory readable to the model-facing shell: every branch's
history, packfiles, reflogs, everything `cat`/`grep`/`ls` can see once a
path is mounted. That is a materially wider read scope than the git verbs
themselves need, granted as a side effect of satisfying this precondition.
Decide that deliberately — a dedicated mount scoped to exactly the common
dir, if your backend supports one, is narrower than widening an existing
allowed-tree entry to cover it.

## What this tool does not protect you from

The threat model this crate addresses is stated above: read-only by
construction (layers 1–4), containment against a repository naming its own
escape, no spawn surface, and a falsifiable fingerprint test rather than an
argument. It is real, and it holds against a repository that names its own
escape — the axis those layers were built for.

It does not hold on the axis this section is about, and on that one a
short-lived `git(1)` invocation is *better* off than you are. Read on before
deciding how much the layers above buy you.

What it does not address: **a pure-Rust, no-exec git tool still puts a large
parser over attacker-controlled bytes — `.git/index`, packfiles, refs,
config — inside your process.** For a short-lived CLI, a crash in that
parser costs one invocation. For a long-lived server — kaibo, kaijutsu, any
kernel that stays up across many callers — a panic or a stack overflow in
that parsing path takes down the whole process and every in-flight job, not
just the one repository that triggered it. That is the risk shape a
long-lived embedder should weigh, and it is a different question from
"is this tool read-only."

**What is guarded explicitly, today:** `.git/index`'s cache-tree extension
(the `TREE` extension) is decoded by `gix-index` with unbounded recursion
and no depth bound of its own — confirmed by probe, a crafted 1200-level
chain overflows a 2 MiB thread before any of this crate's code runs. This
crate's own `index_depth_guard` re-derives enough of the index format to
walk that structure iteratively (a heap `Vec`, not the call stack) *before*
handing the bytes to `gix_index::State::from_bytes`, and refuses past 256
levels, exit 1.

That threshold sits far above any real repository rather than close to one.
The bound is on cache-tree nesting, which tracks directory nesting; measured
across five checkouts on hand, the deepest tracked path was 10 directories
(in a 2,640-file workspace), so 256 is roughly twenty-five times the deepest
shape we have seen. It is `MAX_STATUS_TREE_DEPTH` in `verbs/status.rs` if you
want the reasoning.

The guard fails **closed**: an index shape it cannot fully
account for — a truncated record, an unrecognized structure — is refused as
unreadable, not waved through on the assumption that gitoxide's own decode
would also stop. This is `docs/issues.md`'s **R4** entry, in full.

**What is guarded by construction:** `include.path` resolution is done by
this crate (`from_bytes_no_includes`), not by `gix-config`'s own include
machinery, so there is no library code path that could follow an escaping
include even if a parsing bug existed elsewhere in that crate. `gix-diff`
is taken without its `blob` feature specifically because that feature pulls
`gix-command`, closing an entire spawn-capable code path rather than
patching around it.

**What is not fuzzed at all, and this is stated plainly rather than
implied otherwise:** there is no fuzz corpus, no `cargo-fuzz` target, and no
fuzzing infrastructure anywhere in this repository today. R4 was found by
manual analysis and a hand-crafted probe, not a fuzzer — the same is true of
every other hostile-input fixture in `tests/hostile_repo.rs`. The gitoxide
plumbing crates this build pins (`gix-index`, `gix-pack`, `gix-object`,
`gix-config`, …) are dependencies, not vendored code, and this crate does
not independently fuzz-test their parsers. If your threat model includes a
repository from an untrusted source reaching this tool, weigh that
honestly: the guards that exist are real and tested, but they are not a
substitute for exhaustive input coverage that does not exist yet.

**R4's own history is worth two sentences as illustration, not reassurance.**
The depth guard was added, refused the crafted attack, and shipped. A later
review then found the guard *itself* had a bug: its NUL-terminated
entry-name branch under-consumed a name's terminator by one byte, and reusing
the same padding formula from the length-prefixed branch overshot an entry's
true end by a full 8 bytes whenever the miscount happened to land on an
8-byte boundary — walking to a wrong offset while still returning `Some`,
which could miss a `TREE` extension entirely (the false-safe direction) or
wrongly refuse a legitimate index (the false-refuse direction). Real git,
run to produce the exact boundary case, was the oracle that caught it; the
fix is in and pinned by two tests built from real git rather than
hand-assembled bytes. The point is the class, not the specific bug: a guard
written over hostile bytes needs an oracle it did not have the first time,
and having one guard pass review once is not the same claim as having
proven the class of bug is gone.

**If there is something you can do about the blast radius, it is at your
layer, not this crate's.** This crate offers no async cancellation for a
long verb today — `kaish-tool-api`'s `ctx.patient(budget)` exists and
architecture.md §E.3 names it as the intended fix so a legitimate slow
`log`/`blame` read is not killed by the script watchdog, but nothing in
`src/` calls it yet (`blame` is unimplemented, and `log` does not call it
either), so an unbounded read today has only `--limit` and the kernel's own
output cap between it and running to completion — and no panic-catching
boundary of its own either way. An embedder that runs tool invocations
behind a supervised task, a process boundary, or a catch_unwind at the
dispatch seam gets some protection against a crash taking down more than
the one call; one that calls this tool inline in its
main request loop does not, and should decide that with the risk above in
view rather than after finding out the hard way.

### Cost that scales with the repository, not with the result

Found by cross-model review on 2026-08-22 and stated here because a
long-lived server is where it bites. **`--limit` bounds rows and blob reads.
It does not bound the work that finds the changed paths.** Flatten-and-compare
has to look at every path before it can know which ones changed, so
per-invocation cost tracks repository size, not answer size.

Concretely, for a repository a caller chose rather than one you control:

- **`git status` and `git diff` hash every tracked file** against its worktree
  entry. `max_blob_bytes` caps each read; nothing caps how many. A million
  tracked files is a million reads per call, and `--limit 1` does not change
  that (`docs/issues.md` **G7**).
- **A tree flatten allocates one map entry per leaf.** Depth is capped at 64,
  width is not (**G8**).
- **A single diff has no deadline.** Myers is O(ND) in edit distance over
  blobs capped at 8 MiB each, so one pathological pair can stall a call, and
  there is no cancellation to interrupt it (**G9**, and see the no-cancellation
  entry below).
- **`git status`'s tree walk recurses on the call stack**, bounded at 256
  levels. That bound was measured against a **2 MiB thread**, where overflow
  began at 700–800 levels. If you run this tool on a smaller stack, your real
  margin is smaller than the constant suggests — size the thread, or prefer
  the verbs that walk iteratively (**G10**).

- **`git branch` and `git tag`'s ancestry flags cost commits, not rows**
  (**B4**). A plain listing reads refs and decodes no commit at all
  (`commits_examined` is 0). `--contains` and `--merged` are *filters*, so
  they judge every candidate ref before `--limit` truncates, and their cost
  does not fall when you lower the limit. `--ahead-behind` is *decoration*, so
  it runs only on the rows that survive truncation — which is why it is
  opt-in, and why lowering `--limit` does lower its cost
  (`the_limit_bounds_the_ahead_behind_cost_because_it_runs_first`,
  `tests/branch.rs`). Each `--ahead-behind` branch reads both its own history
  and its upstream's, to the roots; the counts are exact and independent of
  commit ordering, and that is what the full read buys (**B1**). Every one of
  these walks is metered against a shared per-invocation budget of **100,000
  commits**, and passing it is a **refusal** (exit 1) naming the limit, not a
  partial listing — a filter that gave up looking would report a branch as
  not matching. `commits_examined` in `--json` is the number to watch.

None of these is a memory-safety problem and none is reachable past the
containment boundary. They are resource cost, and the mitigation available
today is the same one in every case: **pass `--path` to bound the flatten**,
size your timeouts against repository scale rather than result size, and do
not point a shared server at an arbitrary repository without a budget around
the call.

## Checklist: containing this tool

- [ ] Register with `GitConfig::read_only()` and narrow from there —
      `.without_verb` for anything you do not want offered, `.with_limits`
      for output caps appropriate to your context budget.
- [ ] Confirm your `KernelBackend::resolve_real_path` returns `Some` only
      for paths you intend this tool to read — it is the entire native-path
      boundary (layer 5 above).
- [ ] Do **not** rely on a `LocalFs::read_only` mount for git-level
      read-only. It does nothing for this crate; layers 1–4 are the whole
      story.
- [ ] If your allowed set includes a linked worktree, mount its common git
      dir too (`git rev-parse --git-common-dir --path-format=absolute`,
      run inside the worktree) — and decide deliberately whether that widens
      shell-wide read access more than the git verbs alone need.
- [ ] If your kernel is long-lived, weigh the parsing-surface risk above
      against your process-isolation story; this crate does not provide one.
- [ ] Read `docs/issues.md` before depending on exact numeric output —
      several `git status` divergences (C2, C3, C7, C8) and `git log --stat`
      line-count precision (L6) are characterized, not silent, but they are
      real if your caller diffs output against real git byte-for-byte.

## Known limitations

Selected from `docs/issues.md` — the ones that change an adoption decision,
not the whole backlog. Read that file for the rest, including the two
tree-depth bounds and the entries not listed here.

- **`git status` divergences from real git (C2, C3, C7, C8):** each is
  characterized and pinned by a test built against a live git oracle, so a
  regression is caught, but the *current* behavior still differs from real
  git in specific shapes — an unstaged file→directory typechange, an ignored
  directory holding tracked files, an empty untracked directory, a directory
  wholly ignored by its own nested `.gitignore`. See `docs/issues.md`'s "git
  status — divergences from git" for exact behavior on each.
- **`git log --stat` line counts (L1, L3, L6):** no rename detection (a
  rename counts as a full delete + a full add); `max_blob_bytes` bounds each
  file's read but not the aggregate per commit; and line counts can differ
  from `git show --numstat` by a handful on real-world diffs where more than
  one minimal edit script exists — confirmed against three real repositories,
  in both directions (over and under).
- **`git branch`/`git tag` divergences from real git (B2, B3):**
  `refs/remotes/<remote>/HEAD` is reported as `origin/HEAD`, where git's own
  `%(refname:short)` shortens that one ref to the bare remote name (`origin`);
  and a lightweight tag's `message_summary` is `null`, where git's
  `%(contents:subject)` falls back to the *target commit's* subject. Both are
  pinned against a live git oracle in `tests/branch.rs` and `tests/tag.rs`.
- **`git worktree list` names a working tree it cannot reach, and does not
  probe it (B5).** A worktree registered outside every mount gets
  `path_vfs: null` **and** `prunable: null`: deciding prunability would mean
  stat-ing a host path the repository chose, which is a one-bit existence
  oracle for anything outside the sandbox. Its registration metadata — name,
  branch, HEAD, lock — still comes through, because all of that lives under
  the common dir. `git branch` also does not mark a branch as checked out in
  another working tree the way `git branch`'s `+` does; `git worktree list`'s
  `branch` column is where that lives.
- **Message bodies are unbounded on the `show` path (L7):** `git show`
  always reads a commit's full message body with no cap, even though `show`
  is a single-commit read; `git log`'s per-commit body is gated behind
  `--body` but pays the same uncapped cost once that flag is set. A hostile
  commit with a very large message costs a correspondingly large allocation.
- **The blob cap declines the whole blob, never a truncated prefix.** A blob
  over `max_blob_bytes` is not read at all — `git show` reports its oid and
  real size and nothing else. Raise the cap if you need to read it; there is
  no partial-read mode.
- **Two independent tree-depth bounds, not unified.** `log`'s `--stat` walk
  (64 levels, no call-stack recursion) and `status`'s worktree walk (256
  levels, genuinely self-recursive, empirically anchored to a measured
  overflow point) are different mechanisms with different appropriate
  values — not an oversight, and not planned to converge.
- **A fail-closed guard can refuse a legitimate repository.** The
  `.git/index` depth guard walks the entries itself before handing them to
  gitoxide, and an index shape it cannot fully account for is refused as
  unreadable (exit 4) rather than passed through. That is the safe direction
  and it is deliberate — the alternative is a stack overflow that takes your
  whole process — but the consequence a user meets is a **false refusal**: a
  real repository this build declines to read. It has happened. The 2026-08-21
  follow-up bug had exactly this direction: the guard mis-skipped
  NUL-terminated entry names by 8 bytes, walking to a wrong offset. If you see
  exit 4 on a repository real `git` reads without complaint, that is a bug in
  this crate, not a policy decision — report it with the index, and read
  "What this tool does not protect you from" for why the guard exists at all.
- **No cancellation.** A slow read cannot be interrupted: `ctx.patient(budget)`
  is named by the design doc and called nowhere in this crate, and `blame` (its
  other named consumer) is unimplemented. `--limit` and the kernel's output cap
  are the only bounds on a full-history `log`. If you run this inline on a
  request path, size the timeout around that.
- **`--patch` is a build feature, and only `git diff` has it.** Unified patch
  text needs the `textdiff` cargo feature, which is off by default; `git info`
  lists it under `capabilities.features` so an agent can tell. Without it,
  `git diff --patch` and `--context` refuse with exit 4 naming the feature.
  With it, `git diff --patch` renders the patch — byte-identical to
  `git diff --patch` for the two object-backed endpoint pairs, and accepted
  by `git apply --check` — with four stated non-fidelities: no `index` line
  for a working-tree side, no binary patch encoding, lossy rendering of
  content that is not valid UTF-8, and git's default section-heading rule
  rather than a `.gitattributes` `xfuncname` pattern. `git log --patch`
  refuses in both builds: bounding a patch per commit needs a cap `Limits`
  does not have. See `docs/git.md`, "`--patch`".

## Version and pin guidance

This crate publishes as **`0.9.0`** and does **not** inherit
`[workspace.package].version` (`0.1.0` in this repo) — see
`docs/design/publishing.md` for why: crates.io already holds a libgit2-era
`kaish-tools-git` at `0.8.x` under the same owner, a different codebase, and
`^0.8` does not resolve to `0.9.0`, so publishing forward rather than
resetting the number does not move any existing consumer.

Depend on it the way the root `Cargo.toml` here depends on the four kaish
crates: a caret range on a **published** version (`"0.9"`, not a git rev), so
a patch release resolves automatically and a minor bump — pre-1.0, a minor
here can carry a breaking change — is a deliberate, reviewed step rather
than something that rides in silently. `git info`'s `capabilities.limits`
and `gix_pins` report the exact gitoxide plumbing crate versions this build
links (`GIX_PINS` in `src/lib.rs`, cross-checked against `Cargo.toml`'s exact
pins by `pins_match_the_manifest`) — read those at runtime if you need to
correlate a running build against a gitoxide security advisory rather than
trusting this document to stay current between releases.

## Guarantees without a test

Most of this document is backed by a named test. These are the claims that
are not — enumerated rather than counted, because a count in a section like
this is one more thing that can quietly go stale:

- **Process-isolation advice** in "What this tool does not protect you
  from" (supervised tasks, `catch_unwind` at the dispatch seam) is guidance,
  not a property this crate enforces or tests — there is nothing in this
  codebase that could assert an embedder followed it.
- **The absence of a fuzz corpus** is a fact about this repository's current
  state (verified by search, not by a passing test), not a claim this crate
  makes about itself in code. It can only go stale in the direction of
  becoming false — if fuzzing infrastructure is added later, this section
  should be the first thing updated.
- **The absence of cancellation** — `ctx.patient(budget)` is named by
  architecture.md §E.3 and called nowhere in this crate. Same category as the
  fuzz corpus: an absence verified by search (`grep -rn 'patient(' crates/`
  returns no call site), not a property any test asserts. Load-bearing if you
  are deciding whether to run this inline on a request path, so it is stated
  in "Known limitations" too rather than only here.

## See also

- [`docs/design/architecture.md`](design/architecture.md) — the full design:
  §B the verb surface, §C the profile config (this document's source of
  truth for `GitConfig`), §D read-only enforcement in full, §E kaish
  integration (schema/routing, the `resolve_real_path` bridge, the error
  taxonomy), §H phasing.
- [`docs/git.md`](git.md) — history and design intent; the old libgit2-era
  embedding guide lives there as Appendix A, for provenance only — it is not
  current API.
- [`docs/issues.md`](issues.md) — the full deferred-work backlog.
