# Deferred work

In-tree issue tracker: known divergences, limitations, and parked work that is
deliberately out of scope for the PR that discovered it. Entries move out when
fixed (delete the entry in the fixing PR) or when they graduate to a GitHub
issue because someone outside the repo needs the link.

## git — the D.3 textconv/filter fixture described in architecture.md does not exist

Found while writing `docs/embedding-git.md` (PR 8, 2026-08-22), checking each
claim in the doc against an actual test before writing it down.
architecture.md §D.3 describes a behavioral fixture as already running "on
every build": a hostile repository declaring `diff.pwn.textconv` (pointed at
a script that plants a sentinel file) and `.gitattributes` mapping `* diff=pwn`,
run against every diff/show verb, asserting the sentinel never appears and the
output is the internal diff — plus the same shape for `filter.*.clean`/`smudge`
and `core.hooksPath`.

No such fixture exists. `tests/hostile_repo.rs` (grepped for `textconv`,
`hooksPath`, `filter.*.clean`, `sentinel`, `pwn` — nothing beyond the module
doc's own reference to the D.3 premise) covers only the containment-escape
surface: `commondir`, a `.git` file's `gitdir:` line, symlinked leaves,
`objects/info/alternates`. Real: the *dependency-absence* tripwire
(`.github/workflows/ci.yml`'s `git-tool-dependency-tripwires` job, `cargo
tree -i` for `gix-command`/`gix-transport`/`gix-filter`) is genuinely
enforced in CI. Not real: the *behavioral* proof that a repository
attempting to use textconv/filter/hooks is inert against this build, which
the design doc's D.3 prose reads as already covered.

The dependency tripwire is strong evidence on its own — nothing that could
act on `textconv` is even linked — but it is not the same claim as "a
hostile repository was actually run against every verb and provably did
nothing," and `docs/embedding-git.md` should not repeat the stronger claim
until that fixture exists. Corrected in `docs/embedding-git.md` (PR 8) to
state the tripwire accurately and name this gap rather than overclaim.
Writing the missing fixture is out of scope for PR 8 (an embedder-boundary
and config-plumbing PR, not a D.3 test-coverage PR) — worth its own small PR,
modeled on the containment-escape fixtures already in `tests/hostile_repo.rs`.

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

- **L7 — a commit's message body is an unbounded allocation on the `show`
  path, always; on the `log` path, only under `--body`.** Found by the
  2026-08-21 cross-model review. `verbs::show.rs`'s `build_commit_info`
  always calls `log::split_message` on the full decoded message and sets
  `body: Some(body)` — reasoned (in its own doc comment) as free because
  `show` is one commit, not a bulk listing, but nothing bounds how large one
  commit's message can be: git enforces no limit, so a hostile commit with a
  gigabyte-sized message costs a gigabyte-sized `String` copy (on top of
  `find_commit`'s own full-object read) to serve a single `git show`.
  `verbs::log.rs`'s per-commit body is gated behind `--body` (`if opts.body {
  split_message(...) }`), which avoids the *per-listing* multiplication but
  not this same per-commit cost once that flag is set. `show`'s blob form
  already has the right shape to copy: `read_capped_blob(repo, op, oid,
  opts.max_blob_bytes)` returns `(bytes, size, truncated)` and reports
  truncation honestly rather than reading unbounded content. Applying the
  same cap to a commit message would mean adding a truncation signal to
  `CommitInfo` (`model.rs`), which is shared by both `log` and `show` and is
  part of the published tool output schema — a real schema change, not a
  same-PR fix alongside an unrelated index-guard bug. Needs its own PR:
  bound the message read (probably behind the embedder's existing
  `max_blob_bytes` or a sibling constant), add a `body_truncated: bool` (or
  equivalent) field to `CommitInfo`, and update both call sites and their
  tests together so `log --body` and `show` cannot silently disagree on the
  cap.

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

## git log — `ctx.patient(budget)` is not wired up yet

Found alongside the D.3 fixture gap above while writing `docs/embedding-git.md`
(PR 8, 2026-08-22). architecture.md §E.3 names `ctx.patient(budget)` as the
answer to "Cancellation is the one honest weakness" — used, per that section,
"for `blame` and full-history `log` so the script watchdog does not kill a
legitimately slow read." `blame` is not implemented (no `Verb::Blame` exists
yet), and `grep -rn patient src/` finds no call site in `verbs/log.rs` or
anywhere else in this crate — `log` calls no `patient()` today. Today's only
bounds on an unbounded `log` walk are `--limit` and the kernel's own output
cap; a legitimately slow full-history read on a very large repository is
exposed to the script watchdog exactly as an unbounded one would be. Corrected
in `docs/embedding-git.md` (PR 8) to state this plainly rather than repeat
§E.3's aspirational framing as current behavior. Wiring it up is a small,
self-contained PR against `verbs/log.rs` once there is a concrete watchdog
timeout to test against.

## Upstream

- **R4 — unbounded recursion decoding the index cache-tree (gix-index),
  guarded locally; upstream-only from here.**
  `gix_index::extension::tree::decode::one_recursive` (gix-index 0.54.0,
  `src/extension/tree/decode.rs:36`) still recurses once per subtree of the
  `TREE` extension with no depth bound of its own — a `.git/index` carrying a
  deep enough cache-tree would abort the process with a stack overflow inside
  that call, before any kaish-git code runs. Confirmed by probe: a 1200-level
  chain, which `git write-tree` produces on request, overflows a 2 MiB thread
  in a debug build (a release build's smaller frames do not overflow at that
  depth — the guard below does not rely on that, but it is why the regression
  test is debug-build-only). Backtrace is ~1200 frames of `one_recursive`.

  `gix_index::decode::Options` still carries no way to skip or bound the
  extension (`thread_limit`, `min_extension_block_in_bytes_for_threading`,
  `expected_checksum`, `alloc_limit_bytes`), and `alloc_limit_bytes` does not
  help: it bounds the per-node subtree allocation, while the hostile shape is
  one subtree per level. That part of the analysis stands — this crate still
  cannot make gitoxide itself bound the recursion.

  **Closed for this crate** (not just raised): `ReadRepo::open_index`
  (`crates/kaish-tools-git/src/repo.rs`) now calls
  `index_depth_guard::refuse_if_cache_tree_too_deep` on the raw index bytes
  *before* `gix_index::State::from_bytes` ever sees them. The guard
  (`crates/kaish-tools-git/src/index_depth_guard.rs`) re-derives just enough
  of the on-disk index format — the fixed header, the variable-length entry
  records, and the `TREE` extension's own node encoding — to walk the
  cache-tree's nesting with an explicit heap stack (a `Vec`) instead of
  recursion, and refuses past `verbs::status::MAX_STATUS_TREE_DEPTH` (256,
  the same bound `flatten_subtree` uses) with `GitError::IndexTreeTooDeep`
  (exit 1) before any recursive decode runs. Because the walk is iterative,
  no input can make it recurse — this is a real bound, not a bigger stack
  raising the threshold.

  **Fails closed, not open.** The first version of this guard treated an
  index shape it could not parse (an unrecognized version, a truncated
  record) as "nothing to refuse" and waved it through to
  `gix_index::State::from_bytes` unchecked, reasoning that gix would reject a
  malformed index on its own. That reasoning had a hole a review caught
  before merge: "gix will reject a malformed index" is not the same claim as
  "gix will refuse to recurse" — this guard and gix's real decode are two
  independently written readings of the same bytes, so a parse failure on
  this guard's side is not proof gix's decode would also stop, and a
  mid-chain bail was reporting "safe" having only counted the levels it
  managed to read. Fixed to fail closed: every parse failure that could
  plausibly hide a deeper structure — a truncated cache-tree node, an entry
  this guard's own (re-derived) skip logic cannot account for, a dangling
  extension header — refuses with the new `GitError::IndexTreeUnreadable`
  (exit 1), not "not too deep". The only passthroughs left are the three
  checks that read, against `gix_index` 0.54.0's own source, as identical to
  its header gate (bad signature, unrecognized version, too few bytes even
  for the header) — an assumption this repo cannot itself test (`gix_index`
  is a crates.io dependency, not vendored) and that rides on the exact
  `=0.54.0` pin staying in effect, documented as exactly that at
  `index_depth_guard::ExtensionsRegion::NothingToCheck`, not overclaimed as
  proven.

  `a_tree_deeper_than_the_cap_is_refused` (`tests/status.rs`) still deletes
  the index to keep its assertion pointed at our own `flatten_subtree`
  recursion rather than the index path. The index path has its own coverage
  now: `a_hostile_cache_tree_in_the_index_is_refused_not_crashed` builds a
  1200-level cache-tree *without* deleting the index, and — because a stack
  overflow aborts the whole test process, not just one test — runs the
  guarded decode in a child process (this same test binary, re-invoked to run
  only that one test on a thread with the 2 MiB stack the probe measured) and
  asserts on the child's exit status: termination by signal is treated as R4
  regressing, a clean exit 1 naming the depth limit is the guard working.
  `a_truncated_cache_tree_node_is_refused_not_silently_passed` covers the
  fail-closed fix itself: a 50-level (well under the cap) cache-tree whose
  payload is truncated mid-node must refuse as unreadable, not pass as clean
  — confirmed to fail against the pre-fix code (it returned exit 0 with an
  ordinary, if incomplete, status report) before the fix landed.

  **A second, narrower fail-open closed later (2026-08-21):** `skip_one_entry`'s
  two name-skip branches disagreed on what `consumed` meant. The
  length-prefixed branch (`data.get(path_len..)?`) excluded the terminating
  NUL git always writes after a name; the NUL-terminated branch (name `>=
  4095` bytes, `&data[nul_at + 1..]`) included it. Both then reused the same
  `(consumed + 8) & !7` padding formula, which is only correct for the
  excludes-the-NUL meaning — on the includes-the-NUL branch it overshot the
  entry's true end by a full 8 bytes whenever `consumed` (with the NUL)
  landed exactly on an 8-byte boundary. That overshoot could walk the guard
  to a wrong offset in the entries region and still return `Some`, missing a
  `TREE` extension entirely, or (as the fixture below hits) come up short of
  the trailing checksum and wrongly refuse a legitimate index — either way
  contradicting `error.rs`'s "a real index written by real git always parses
  to completion" doc. Found by a cross-model review and confirmed against
  real git (`git update-index --add --cacheinfo` with a 4097-byte name: `>=
  4095` selects the NUL-terminated encoding, and 62 (fixed header) + 4097
  (name) + 1 (NUL) = 4160, a multiple of 8 — exactly the overshooting case).
  Fixed by making both branches consume through the NUL (the length-prefixed
  branch now does `data.get(path_len + 1..)?`) so `consumed` means the same
  thing everywhere, then rounding up with `(consumed + 7) & !7`. Pinned by
  `index_depth_guard.rs`'s
  `a_legitimate_index_with_a_nul_terminated_name_on_an_eight_byte_boundary_is_not_refused`
  (fails against the pre-fix formula) and
  `a_legitimate_index_with_a_length_prefixed_name_on_an_eight_byte_boundary_is_not_refused`
  (the branch that was already correct, now re-proven against the rewritten
  formula), both built from real git rather than hand-assembled bytes. The
  same review also claimed version-4 entries are padded like v2/v3 and that
  the `// No padding in this version.` comment on that branch is wrong — a
  probe against real git (`git update-index --index-version 4` on three
  one-byte files) refuted it: entries of 71, 70, and 69 bytes, none a
  multiple of 8, with the 20-byte checksum starting immediately after the
  last one. Left alone, and now pinned against regressing on that false
  reading by
  `a_real_git_version_four_index_has_no_padding_between_entries`.

  What is left is upstream-only: gitoxide's own `one_recursive` still has no
  depth bound, so anything that calls `gix_index::State::from_bytes` directly
  — outside this crate's guard — remains exposed. A gitoxide issue (a depth
  limit in `one_recursive`, or a way to skip the `TREE` extension via
  `decode::Options`) is still worth filing upstream; this repo's contribution
  guidance reserves filing to repos we don't own for Amy personally, so that
  step is hers, not an agent's.

## git — two tree-depth bounds, not one (not planned to converge)

`verbs::log::MAX_STAT_TREE_DEPTH` (64) and `verbs::status::MAX_STATUS_TREE_DEPTH`
(256, also reused by `index_depth_guard` for the index's cache-tree) used to
share the bare name `MAX_TREE_DEPTH` — harmless while each was private to its
own module, noticed and renamed when the R4 fix made `status`'s constant
`pub(crate)` and imported by name from a third module, which turned a
same-name collision cross-module.

Not unified into one constant, and not expected to be: `status`'s
`flatten_subtree` is genuinely self-recursive (calls itself once per subtree
on the real call stack), which is what makes 256 a hard stack-safety bound,
empirically anchored to the depth a debug build measurably overflows at
(700–800 levels on a 2 MiB thread). `log`'s `flatten_tree` walks with an
explicit `Vec`-based stack instead — no call-stack recursion at all — so its
64 is a generous sanity cap on a mechanism that does not carry the same
overflow risk in the first place, not a value measured against the same
failure mode. Two different mechanisms with two different appropriate
values; changing either is a behavior change, which is out of scope for a
naming cleanup.

## git tree walks — bounded depth but not width (found by the 2026-08-21 cross-model review, out of scope for the padding-guard PR)

Depth is bounded everywhere in this crate (`MAX_LISTING_TREE_DEPTH`,
`MAX_STAT_TREE_DEPTH`, `MAX_STATUS_TREE_DEPTH`, all above). A single tree
object's own *width* — its entry count — is bounded only by its raw byte
size, and a review flagged that `treewalk.rs::list_tree_at_depth` read every
entry of a visited directory into an owned `Vec` before its `--limit` check
in the output loop ever ran, so a hostile tree with millions of entries paid
that materialization cost regardless of the cap. Fixed in this PR: the
function now steps `find_tree_iter` directly and checks `*collector.truncated`
per entry, so a small `--limit` bounds the read itself, not just what gets
reported.

The same review flagged the identical shape in two more places, left
untouched here (out of scope for a padding-guard PR, and neither has a
`--limit` concept to bound against — see below):

- `verbs::status.rs`'s `flatten_subtree` (the recursive step
  `flatten_head_tree` calls) collects a tree's entries into two owned `Vec`s
  (`children`, `subtrees`) before doing anything with them — the exact
  materialize-then-process shape `list_tree_at_depth` had. `status` computes
  a complete HEAD-vs-index-vs-worktree comparison, though, so there is no
  `--limit` to bound reads against the way `ls`/`show` have one: a partial
  status would be a worse bug than the memory cost. Fixable the same way
  `list_tree_at_depth` was (stream entries instead of collecting), but that is
  a real change to a load-bearing function with its own extensive test
  coverage — worth its own PR, not a drive-by alongside an unrelated index
  fix.
- `verbs::log.rs`'s `flatten_tree` does *not* share the pre-collect-`Vec` step
  — it already processes each `find_tree_iter` entry directly in its `for`
  loop, pushing to its explicit stack or its output map as it goes, with no
  intermediate buffer to remove. Its width is still unbounded in total (no
  `--limit` concept, same reasoning as `status` above), but there is no
  redundant-materialization bug to fix here, just the inherent cost of a full
  tree-vs-tree diff.

## kaish-tools-git — publishing

- **The crates.io version timeline.** `kaish-tools-git` is at workspace
  version `0.1.0` and has never been published in this form, but the name is
  not free: crates.io already holds five libgit2-era versions (`0.8.0`
  through `0.8.4`, none yanked), owned by the same account, from the
  pre-rewrite `tobert/kaish` monorepo — a different codebase under the same
  name. Publishing `0.1.0` under a `max_version` of `0.8.4` is legal but
  confusing (cargo's resolver goes by version number, not publish date, so a
  fresh `cargo add` would keep landing on the old libgit2 code). Options and a
  recommendation (publish forward as `0.9.0` rather than yanking the old
  line or resetting the number) are laid out in full in
  [`docs/design/publishing.md`](design/publishing.md) — Amy's call, not made
  here.

## git — the embedder boundary

- **G6 — "mount the common dir" may be a wider grant than the git verbs need.**
  Raised by kaibo on 2026-08-22 from a measured case, not a hypothetical. A
  linked worktree whose main repository is outside the allowed set is refused
  with exit 4 at `screen_gitdir_file` (`repo.rs:138`), and the fix we tell the
  embedder is to mount the common git dir too. For an embedder whose allowed
  set doubles as its **kaish mount table** — kaibo's does — that single grant
  also makes `.git` readable to the model-facing shell: `cat`, `grep`,
  packfiles, reflogs, every branch's history. The git verbs need read access to
  the object store; the shell does not need it as a side effect.

  kaibo is explicit that the conflation is theirs and not ours to design
  around, and this is **not** scheduled. It is recorded because the shape
  recurs: if a second embedder reports it, the answer is a narrower grant than
  a mount — a path the git tool may read that is not a shell mount root — and
  that is a `GitConfig` question, possibly a kaish `ToolCtx` one. Do not build
  it on one report; the shape of the second ask should decide it, the way
  CU46b is being held for curl.

  Cost disclosure belongs in `docs/embedding-git.md` regardless, and is in
  scope for PR 8.

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

  **Asked and answered, 2026-08-22 (Amy, via the kaijutsu session).** The
  shape is now known even though the build is not scheduled:

  > *"today, kaijutsu does not sit behind a proxy, but over time we will want
  > to support that (I want to use it at work soon, but our safety features
  > have to be A/B-grade first, I think we're still at C+ until the current
  > workstreams tie up). kaish curl should allow an embedder to poke in those
  > kind of configs without getting too opinionated, and make eg proxy and
  > cert store options configurable."*

  Three things that ruling settles:

  - **Nothing to unblock today.** No proxy and no inspection CA on any of the
    three machines kaijutsu runs on. No deadline.
  - **The requirement is "configurable", not "supported".** Expose the proxy
    and the cert store as things an embedder states, and decide no policy on
    their behalf. This is the allowlist lesson again: kaijutsu named its own
    egress and survived our default being wrong (see `docs/curl.md`, "Name the
    allowlist; do not inherit it"). An API the embedder can state the answer
    into has that same property; one that infers a default does not.
  - **Size it against the real call site.** kaijutsu's entire curl
    configuration is four lines in one function. Proxy and roots must not
    change that shape — if the API needs more than a line each there, it is
    the wrong API.

  The gate is kaijutsu's own safety posture (their gate/ledger workstreams),
  not this API. Trigger to revisit: ping kaijutsu when the git crates' surface
  settles, by which point they will know whether the work laptop is in play.

## Cross-embedder obligations and a pattern worth stealing

Recorded 2026-08-22 out of the kaijutsu and kaibo exchanges. These are things
we owe other people, or things they do better than us.

- **X3 — a kaish dependency behavior canary.** kaijutsu carries
  `command_substitution_survives_a_non_last_pipeline_stage`, a test whose only
  job is to fail loudly if the kaish dependency regresses on a behavior they
  depend on. We have `dependency tripwires` over `cargo tree` for the gix
  plumbing (asserting `gix-command`/`gix-transport`/`gix-filter` never enter
  the graph) and an e2e stage asserting the MOTD version matches
  `kaish-version` — but nothing that pins a kaish *behavior* we rely on.
  A silent behavior regression across a 0.15.x patch would reach us as a
  mystery, not a failure. Pick the behaviors the tool crates actually depend
  on (argv binding order under `with_raw_argv`, `GlobalFlags` handling,
  `ToolCtx::resolve_path` refusal shapes) and pin one test each. Cheap, and it
  is exactly the shape that would have caught the 0.15.0 undeclared breaking
  changes early.

- **X4 — tell kaijutsu when kaish #385/#386 lands in a published release.**
  An `if` condition's stderr reaching the enclosing statement. kaijutsu's gate
  guard bodies are `if`-shaped and their deny reasons arrive empty without it.
  **Published release, not merged to main** — a merged sha is not something
  they can consume. We track the 0.16 bump for our own reasons and talk to
  kaish-lead directly, so this costs us one message at a moment we are already
  paying attention.

- **X5 — ping kaijutsu when the git crates' surface settles**, as the trigger
  to look at CU46b (proxy and cert store) together. Agreed with them in place
  of an open-ended wait, because it is an event both sides can observe. By then
  they will know whether the work laptop — the only place a corporate
  TLS-inspecting CA would appear — is actually in play.

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
