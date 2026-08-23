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
  within our own surface, which is the part worth closing. PR 5 did factor the
  pairing out — it is `diffcore::pair_exact_renames`, shared by `status` and
  `diff` — so what remains is deciding whether `log --stat` should run it per
  commit at all, which costs an add/delete pass over every commit walked and
  diverges from git's own default (`git log --stat` without `-M` does not
  detect renames either).

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
  same committer instant to the second. Git's walk orders them one way; ours
  orders them another.

  **The cause recorded here on 2026-08-21 was wrong, and is corrected
  2026-08-22 (PR 7).** It said the `BinaryHeap` is "keyed on committer time
  alone" and "has no secondary tiebreaker". It has one: `Pending::cmp` in
  `log.rs` breaks a tie on the oid, and has since 2026-08-13 (`4495cb0`), a
  week before the finding was written. So the order is *deterministic* — the
  same repository answers the same way twice, which the original text says it
  would not — it is just **not git's** order. Git resolves the tie by its own
  discovery order along the walk; we resolve it by hash. Narrow (needs two
  unrelated same-second commits) but real: fast scripted merges,
  sub-second-granularity clocks, or a CI-driven series can all produce one.
  The fix is still a tiebreaker matching git's, added to `Pending`'s ordering.

  **It does not reach `git branch` or `git tag` (checked in PR 7).**
  `--contains` and `--merged` are boolean ancestry predicates, which no
  ordering can move, and `--ahead-behind` counts rather than orders — and its
  walk was deliberately made order-independent for a different reason (**B1**).
  Nothing in those verbs sorts by commit time at all: rows come back in
  full-refname order.

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

- **L8 — `--stat` misses a mode-only change that git counts.** The tree
  comparison behind `--stat` compares blob oids, and a `chmod +x` changes the
  tree entry's mode without changing the blob — so a commit that only flips
  the executable bit reports `files: 0`. Git counts it as one file changed
  with zero lines on both sides (`0\t0\trun.sh` in `--numstat`). Found while
  PR 5 factored the comparison core out; pinned by
  `log.rs::stat_misses_a_mode_only_change_that_git_counts`, which asserts our
  behavior and git's separately. `git diff` matches git here — its comparison
  carries the class alongside the oid — so the fix is to have `--stat` do the
  same, which means `changes_against` reporting a class-only difference. Not
  done in PR 5: `--stat`'s counts are covered by their own oracle tests and
  changing what it reports is a behavior change outside a diff PR.

- **L9 — `--stat` counts no lines for a submodule move that git counts as
  one each side.** A gitlink has no blob to read, so `--stat` reports the file
  with zero additions and deletions. Git renders the patch as `-Subproject
  commit <old>` / `+Subproject commit <new>` and counts `1\t1`. Pinned by
  `log.rs::stat_counts_no_lines_for_a_submodule_move_that_git_counts_as_one`.
  `git diff` matches git (one line per present side, no blob read to know it),
  so this is a one-line change in `commit_stat` whenever `--stat`'s counts are
  next touched. **The related crash is fixed**, not deferred: `read_blob` used
  to decide gitlink-ness by asking the object store for the oid's header, and
  a gitlink's oid is a commit in *another* repository — so `git log --stat`
  failed outright, exit 1, on any commit that moved a submodule pointer. The
  class the caller already has names it now (`diffcore::Side::Gitlink`).

## git diff — deferred (architecture.md B.4, shipped in PR 5)

- **D1 — a modified-and-moved file is a delete plus an add.** Rename detection
  is exact-match only: a blob oid reappearing at a new path. A file that was
  edited *and* moved has a different oid and never pairs, where git scores the
  pair (`R087`) and folds it. **Permanent under this dependency set** —
  `gix-diff`'s rename tracker is behind its `blob` feature and `blob` pulls
  `gix-command` — so this is reported rather than fixed: `similarity` is only
  ever `100` or `null`, and copy detection is absent entirely. Pinned by
  `diff.rs::a_modified_and_moved_file_is_a_delete_plus_an_add`, which asserts
  both behaviors. Revisiting means a line-similarity scorer of our own over
  `gix-imara-diff`, which is a real design question (what threshold, and how
  many candidate pairs may it consider) and not a gap to close quietly.

- **D2 — an unmerged path is omitted rather than reported.** A conflicted path
  has no stage 0, so it is dropped from *both* sides of the comparison,
  counted in `unmerged`, and named on stderr. Dropping it from one side only
  would report a conflicted file as `deleted`, which is a wrong answer wearing
  a normal status. Git reports a `U` row instead. Giving one here means a
  model change: B.4's `DiffFile` has no `conflicted` field where B.2's
  `StatusEntry` does, and `EntryStatus` has no unmerged word by an explicit
  B.2 decision. Pinned by
  `diff.rs::unmerged_paths_are_declared_not_silently_dropped`.

- **D3 — `--path` filters the candidate set before renames are paired.** The
  filter is applied early on purpose: it is what keeps `--path src` from
  hashing the whole working tree. The cost is that a rename whose *source* is
  outside the filter reports as an addition rather than as a rename. Closing
  it means pairing on the unfiltered sets and filtering the rows afterwards,
  which gives back the bound. Not decided; nobody has hit it.

- **D5 — the worktree endpoints hash every tracked file the filter kept.**
  `--limit` bounds the *reported* files and, deliberately, the blob reads
  behind their line counts — truncation happens before `finish` reads
  anything, so a small `--limit` bounds the reading and not only the output.
  What it cannot bound is the pass that decides *which* files changed:
  index→worktree and rev→worktree hash every tracked file, because content
  hashing is the only honest way to tell — git uses the index's stat cache
  for this and we do not, since refreshing it is a `.git` write and the
  fingerprint test (D.4) exists to catch those. `--path` is applied to the
  candidate set before that pass, which is the lever a caller has. Same cost
  `git status` already pays, and accepted for the same reason.

## git diff --patch — deferred (architecture.md B.4, F.1, shipped in PR 6)

- **T1 — hunk text for non-UTF-8 content is lossy, so its patch does not
  apply.** Content with no NUL byte is text to git and to this build, but
  hunk lines are `String`, so a latin-1 byte becomes U+FFFD on the way in and
  the rendered patch is refused by `git apply` where git's own is accepted.
  It is one conversion point — `diffcore.rs`'s `load_both` — and the counting
  path has always had it, so this is not new with hunks; what is new is that
  the lossy bytes now reach the caller. Closing it means carrying hunk text
  as bytes and giving `ExecResult` a bytes payload for `--patch`, which is a
  kaish-side question (`success_text_or_bytes` exists for blobs but does not
  compose with `with_output_and_text`). Pinned both ways by
  `textdiff.rs::latin1_content_renders_lossily_and_git_does_not`, which
  asserts git keeps the byte, we do not, and the consequence.

- **T2 — `git log --patch` refuses even with `textdiff` on.** Not an
  oversight: `git diff --patch` is bounded at `max_diff_files` x
  `max_hunk_bytes_per_file` (500 x 256 KiB), and a per-commit patch
  multiplies that by `--limit` — 20 commits of defaults is 2.5 GiB of patch
  text with no third cap to stop it. `Limits` has no whole-report patch
  budget, and adding one is a `GitConfig` change (C.1), which is a model
  change and not a same-PR fix. The refusal names
  `git diff --patch --from <commit>~1 --to <commit>`, which is bounded and
  answers the same question for one commit. `git show --patch` is the same
  shape and the same deferral: a single commit is bounded, so it is the
  cheaper of the two to add, and B.5 already specifies `--name-only`,
  `--stat` and `--patch` for `show` as one group. Pinned by
  `textdiff.rs::log_patch_points_at_diff_patch_rather_than_at_the_feature`.

## git branch / tag / worktree list — deferred (architecture.md B.7, B.9, shipped in PR 7)

- **B1 — `--ahead-behind` reads both histories to their roots, rather than
  stopping at the common part.** The cheap version of this walk pops commits
  newest-first and stops as soon as everything queued is reachable from both
  sides, which makes its cost proportional to the *divergence* instead of to
  the history. It is only sound if a commit is popped after every commit that
  reaches it — that is, if committer time increases from parent to child —
  and it does not have to. Written that way first and caught by two fixtures:
  `RefsRepo` pins one committer instant across its whole history, and the
  early-stopping walk reported `behind 2` where `git rev-list --left-right
  --count` reports 1; a fixture whose merge base is stamped seven months after
  both tips did the same. Git makes the same assumption and softens it with a
  slop counter (`paint_down_to_common`'s `SLOP = 5`); we do not, so the walk
  we ship is order-independent and exact and reads
  `|ancestors(local) ∪ ancestors(upstream)|` commits per reported branch.
  Both properties are asserted rather than described
  (`branch.rs::a_backwards_committer_clock_does_not_move_the_counts`,
  `branch.rs::a_history_with_one_committer_instant_still_counts_correctly`).
  Making it cheap again wants either commit-graph generation numbers or git's
  slop, and neither is worth doing before a real repository makes the cost
  hurt. Same family as **L5**, and the reason L5 does not reach this verb: L5
  is about a *reported order*, this is about a *count*, and nothing here
  depends on order any more.

- **B2 — `refs/remotes/<remote>/HEAD` is named `origin/HEAD`, where git's
  `%(refname:short)` says `origin`.** Git shortens that one ref to the bare
  remote name, which reads like a branch called `origin` sitting beside
  `origin/main`. We strip the namespace prefix uniformly instead. Deliberate,
  and pinned in both directions by
  `branch.rs::a_symbolic_remote_head_is_named_by_its_ref_not_by_its_remote`,
  which asserts git's answer as well as ours. Revisit only if an embedder
  reports a caller comparing our names against `git branch -r` output.

- **B3 — a lightweight tag's `message_summary` is `null`, where git's
  `%(contents:subject)` reports the target commit's subject.** A lightweight
  tag has no tag object, so it has no message; git's fallback hands back a
  line nobody wrote about the tag, which for an agent reading a tag listing
  is worse than an absent field. Deliberate, and pinned with git's own answer
  asserted beside ours in
  `tag.rs::annotation_is_reported_only_where_there_is_one`.

- **B4 — `--contains` and `--merged` cost history that `--limit` does not
  bound.** Same family as **G7-G10**, and the same cause: a filter has to
  judge every candidate before truncation, or the truncation cuts rows the
  filter never looked at. `--contains` memoizes reachability per commit across
  every ref asked, so N branches cost the union of their histories rather than
  the sum; `--merged` builds one ancestor set for the named revision, so it is
  one full-history walk however many branches are listed. Neither falls when
  `--limit` falls, and the test that pins `--ahead-behind`'s cost *does* fall
  asserts this half stays flat
  (`branch.rs::the_limit_bounds_the_ahead_behind_cost_because_it_runs_first`).
  What is different from G7-G10, and worth stealing: the cost is **reported**.
  `commits_examined` is on every `branch`/`tag` result, it is 0 for a listing
  that walks nothing, and a shared per-invocation budget of 100,000 commit
  reads (`reach.rs`'s `MAX_ANCESTRY_COMMITS`) turns the unbounded case into a
  refusal (exit 1) instead of a stall. The same treatment would suit `status`
  and `diff`'s worktree hashing, which today have neither a counter nor a cap.

- **B5 — `git branch` does not mark a branch checked out in another working
  tree.** Git prints `+` beside it (and refuses to delete it). We have the
  information — `git worktree list` reads every registration's private HEAD —
  but the branch row has no field for it, and adding one means `branch`
  reading the worktree registrations on every plain listing, which is exactly
  the kind of cost B4 is about. `git worktree list`'s `branch` column answers
  the same question for the price of a second call. Not scheduled.

- **B6 — `worktree list` skips a registration it cannot interpret, rather than
  reporting it.** A `gitdir` file whose contents are not an absolute path
  ending in `.git` names no working tree we can report without inventing one,
  so the row is left out. Three shapes reach it: a relative path (git's
  `relativeworktrees` extension, which `repo.rs` refuses to open a repository
  under at all), a path not ending in `.git`, and an empty file. Refusing the
  whole listing would let one bad registration hide every good one; a row with
  a guessed path would be worse. What is missing is a *count* of what was
  skipped, so a caller can tell "no such worktree" from "we could not read
  it". Pinned as unit cases in `verbs/worktree.rs`; add the count if an
  embedder ever asks.

- **B7 — a bare repository is not a row in `git worktree list`.** Git lists
  the bare git directory as a worktree with a `bare` marker; we list only the
  linked worktrees it owns. A bare repository has no working tree, so leaving
  it out of a listing *of working trees* is defensible — but it is a
  divergence, and B.9's row shape has no `bare` field to carry it if we ever
  want parity. Pinned with git's own answer asserted beside ours in
  `worktree.rs::a_bare_repository_lists_its_linked_worktrees_and_not_itself`.
  The neighbouring case is worse and is not a divergence anybody has hit: a
  repository whose git directory is named something other than `.git` *does*
  have a working tree, and it is missing from the listing. That one needs the
  `core.worktree` handling this build does not have.

## git blame — deferred, and a good contribution-sized piece

**BL1 — `git blame` is designed and unbuilt.** It was scoped into PR 7 with
the three listing verbs and dropped before any of it was written. The reason
is plain and worth writing down rather than dressing up: this maintainer
almost never uses blame, so it is low value for the first embedders, and it is
a well-shaped piece for someone who does want it. Nothing about it is blocked.

**The design is already written** —
[architecture.md B.8](design/architecture.md#b8-git-blame) — and it should be
followed rather than redesigned:

- `git blame <PATH> [--rev <REV>] [--lines <A:B>] [--limit <N>]`. `<PATH>` is
  a required positional; `--rev` defaults to `HEAD`; `--lines` is 1-based
  inclusive and is the **whole** range grammar — `-L`'s regex and offset forms
  do not exist and should be a parse-time refusal naming the spelling that
  does; `--limit` defaults to 2000 lines.
- Rows are `{line, oid, short_oid, author, time, orig_line, text}`.
- Three honesty requirements, in the payload rather than in prose:
  `blamed_rev` naming the committed revision actually annotated,
  `worktree_differs: true|false` with a stderr note when true (blame is
  committed-content-only, and neither refusing nor quietly annotating stale
  content is honest), and `follows_renames: false`.

**It is hand-composed here.** `gix-blame` is not available to this crate:
architecture.md A.2 explains why this build uses gitoxide's plumbing crates
and skips the `gix` facade, and blame is one of the two verbs that costs. The
shape is a revwalk from `--rev`, a path-limited tree diff per commit against
its first parent, and line mapping across each commit that touched the path.
`verbs/log.rs`'s walk and `diffcore.rs`'s `flatten_tree` / `line_hunks` are
the pieces to build on; `reach.rs`'s `Budget` is the metering to reuse.

**Two things make it harder than the row shape suggests**, and a contributor
should know both before starting:

1. **Nothing can interrupt it.** architecture.md E.3 names `ctx.patient(budget)`
   as the answer to a slow read, and `grep -rn patient src/` finds no call
   site anywhere in this crate (see the `git log` entry above). A slow blame
   runs to completion.
2. **`--limit` bounds lines; the cost is commits.** A 40-line file with two
   thousand commits behind it is expensive, and `--limit 10` does not make it
   cheaper — the revwalk is what costs, and it is driven by the path's
   history, not by how many rows are wanted. Say that plainly in the flag's
   own published description rather than letting "max lines" imply a bound it
   does not provide. `reach.rs`'s pattern is the one to copy: meter the walk,
   report what it spent (`commits_examined`), and refuse rather than truncate
   when the budget runs out. This is the **G7-G10 / B4** family; add to it
   rather than restating it.

**A sub-gap, not a blocker for a first version:** rename-following across
paths (kaish-extras#11). B.8 is explicit that this is a *different* gap from
B.4's exact-match renames — that one is permanent under this dependency set,
this one waits on a rename-aware primitive existing at the plumbing level at
all. A first blame that reports `follows_renames: false` and stops at the
rename is the right first version.

**What lands with it:** an entry in `Verb`/`Verb::ALL`, a `VERB_MATRIX` entry
in `tests/readonly_fingerprint.rs`, a mention in `docs/embedding-git.md` (the
`the_embedding_guide_names_every_verb` guard enforces it), an `EXAMPLES`
entry, and a differential test against `git blame --line-porcelain`.

## git — what the tool schema actually publishes (read from the schema, 2026-08-22)

Read out of `ToolSchema` rather than inferred from the source, per AGENTS.md
("do not infer the published text by grepping"). Two things every verb
publishes that no agent should be reading, both pre-existing and both
cross-verb, so neither is a PR 5 fix:

- **`--operands` is in the schema — FIXED in PR 5, and the first diagnosis was
  wrong.** Every verb carries an `#[arg(hide = true)] operands: Vec<String>`
  sink so clap accepts the `--`-terminated tail `ToolArgs::to_argv()` emits
  (E.1). Those sinks reached agents carrying a description written for *us*
  ("do not read this field, it cannot distinguish them either", naming
  `ToolArgs::to_argv` and `tool.rs`) on all six verbs.

  The first reading was that `schema_from_clap` fails to honor clap's `hide`
  and the fix is a kaish PR. **Checked at the source, and kaish is right.**
  `kaish-tool-api` 0.16's `clap_schema.rs:100-125` skips hidden *flags* and
  deliberately keeps hidden *positionals*, documenting why: for most tools
  the hidden positional IS the public surface (`cat paths…`). That is true
  here too — `git show HEAD:src/lib.rs` and `git ls HEAD src` are positional,
  and an agent needs them documented. Asking kaish to drop them would have
  deleted the only schema entry describing the flagship spelling of two verbs.

  So the defect was entirely ours, and it is the AGENTS.md rule "Published
  text is published" broken six times in one place: behavior belongs in the
  `///`, mechanism in a `//`. Each verb's operand doc now states what an agent
  types (`git status -- src tests`, `git log HEAD~5 -- src/lib.rs`,
  `git ls HEAD src`, `git show HEAD:src/lib.rs`, `git diff -- src`, and
  `info` taking none), and the mechanism moved to `//`.
  `no_published_description_is_a_note_to_ourselves` reads the built schema —
  not the source, per the same rule — and fails on internal vocabulary, with a
  negative control asserting the `operands` param is present and shows a real
  spelling. Mutation-tested: it goes red on a reverted description.

- **`--limit` publishes `type=string` and no default.** It is `usize` in
  every verb's parser with a real `default_value_t` (1000 for `ls`/`show`,
  20 for `log`, 500 for `diff`), and the schema carries neither the type nor
  the number. `diff`'s and `log`'s argument docs state the default in prose,
  which is the "provide specific values" rule doing the work the schema
  field is not; `ls`'s and `show`'s do not, and should. Whether the type hint
  can be fixed at all is a `schema_from_clap` question.

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

`diffcore::MAX_FLAT_TREE_DEPTH` (64, named `verbs::log::MAX_STAT_TREE_DEPTH`
until PR 5 moved the walk it bounds into the shared comparison core) and
`verbs::status::MAX_STATUS_TREE_DEPTH`
(256, also reused by `index_depth_guard` for the index's cache-tree) used to
share the bare name `MAX_TREE_DEPTH` — harmless while each was private to its
own module, noticed and renamed when the R4 fix made `status`'s constant
`pub(crate)` and imported by name from a third module, which turned a
same-name collision cross-module.

Not unified into one constant, and not expected to be: `status`'s
`flatten_subtree` is genuinely self-recursive (calls itself once per subtree
on the real call stack), which is what makes 256 a hard stack-safety bound,
empirically anchored to the depth a debug build measurably overflows at
(700–800 levels on a 2 MiB thread). `diffcore`'s `flatten_tree` — the walk
`log --stat` and `diff` share — uses an explicit `Vec`-based stack instead — no call-stack recursion at all — so its
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
- `verbs::log.rs`'s `flatten_tree` moved to `diffcore.rs` in PR 5 and is now
  shared with `diff`. It does *not* share the pre-collect-`Vec` step
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

## git — from the pre-publish review round (2026-08-22)

Four reviews before 0.9.0, framed as *"what tests are missing"* rather than
*"find bugs"*: `crusoe-ds4` and `qwen38` for breadth on different families,
`gemini-pro` and `gpt-5.6-sol` deliberating over one shared dossier. Two live
bugs came out and are **fixed** (`info`'s worktree-count oracle, `log --limit
0`); the stale module docs are fixed. What is left, ranked:

- **P13 — two guards now check the same invariant, and one of them is the weak
  one P9 was about.** Closing P9 added `tests/config_needle_guard.rs`, whose
  `the_real_crate_src_does_not_read_core_worktree` scans for both the literal
  `"core.worktree"` and the two-argument `config_values("core", "worktree")`
  form. `tests/hostile_repo.rs`'s `work_dir_is_bounded_by_discovery` still
  carries the single-needle version — **superseded, and superseded by the very
  defect it was cited for**. AGENTS.md forbids parallel old/new paths, so the
  old one goes; what it uniquely carries (the comment explaining why the
  work_dir ceiling check is merely defensive) moves with it rather than being
  deleted. It survived the P9 fix only because `hostile_repo.rs` was owned by a
  concurrent branch at the time — a scheduling artifact, not a decision.

- **P2 — a non-canonical index mode is silently dropped, and the comment
  misattributes the skip.** `Class::from_index` returns `None` for anything
  outside the four canonical modes, and both call sites skip it with a comment
  saying "a sparse-directory entry (cone mode)". A sparse entry is `040000`; a
  `100664` entry — which `git update-index --cacheinfo 100664,...` writes
  happily — is dropped too. The tree side *does* classify it, so `status` and
  `diff --staged` report the file **deleted** where git reports a mode change.
  The comment would send the next reader at the wrong fix. Separately,
  `treewalk` normalizes raw modes through `mode.kind().as_octal_str()`, so
  `ls` prints `100644` where `git ls-tree` prints `100664`.

- **P4 — the gix open-by-name carve-out reaches further for refs than the doc
  admits.** `repo.rs` documents that gix opens objects, packs, `HEAD` and ref
  files by name after discovery, and characterizes the consequence as a read
  that "almost always fails to parse as a git object". True for objects, false
  for refs: a loose ref is 40 hex chars and `packed-refs` is `<40hex> <name>`
  lines — shapes real host files have. A symlinked `refs/heads/pwn` pointing at
  a host file whose first line is 40 hex characters would surface those 160
  bits as a branch oid in `branch --json`; non-hex content fails the parse,
  which is still a one-bit content probe. `HEAD` and `packed-refs` are fixed
  names and *are* interceptable the way `open_leaf` intercepts other fixed
  leaves. Decide: intercept, or state the leak louder than "almost always".

- **P5 — a `.gitignore` symlinked out of the mount is a content oracle.**
  `gix-worktree` reads per-directory `.gitignore` itself during the untracked
  walk, and its content decides which paths are reported ignored. Plant an
  untracked file `N`, symlink `sub/.gitignore` at a host file: `status
  --ignored` reports `!! N` exactly when that file contains a pattern matching
  `N`. One bit per invocation against any host file. `walk_untracked_and_ignored`
  already visits every directory itself, so a `symlink_metadata` pre-screen is
  available without touching gix.

- **P6 — a symlinked start path walks discovery outside the ceiling.**
  `screen_gitdir_file` returns `Ok(())` when the start path canonicalizes
  outside the ceiling, on the comment's reasoning that "discovery will report
  the missing path". It does not — it walks from there, and the ceiling's
  lexical prefix match drops. The *result* is still refused with no content
  read, but gix's ownership check fires on outside candidates, so
  `NoTrustedGitRepository` (exit 4) versus nothing-there (exit 1) is one bit:
  "is there a repo I do not own above my symlink's target".

- **P7 — a repo-relative `include.path` hides the format and extension
  gates.** `check_include_paths` refuses only *escaping* includes, and includes
  are never followed (`from_bytes_no_includes`) — so a repository can put
  `core.repositoryformatversion = 2` or `extensions.objectFormat = sha256` in
  an included file and `check_format_version`/`check_extensions` never see it.
  The bare config reads as format 0, the gates are skipped. Blast radius is
  bounded today (nothing honors `core.worktree`, and a `reftable/` directory is
  caught by an on-disk probe regardless of config), but this is the refusal
  layer's own bypass.

- **P10 — untested semantic inputs, each a characterization test.** Shallow
  clones (the `refuse_shallow` gate exists, nothing exercises it); replace refs
  and grafts (git honors them by default, this walk never consults them);
  octopus merges (every fixture merge has two parents, while `--merges`,
  `--first-parent`, `--stat` and `^3` all have 3+-parent paths); `status` on an
  unborn HEAD; deep tag chains against the depth-8 `show` and depth-32
  `peel_tag_chain` bounds; non-UTF-8 tree entry names (rendered lossily where
  `git ls-tree` C-quotes, and silently skipped in the untracked walk);
  `--path ""` (silently matches everything, git errors); and `core.autocrlf`,
  whose divergence C5 records in prose while every fixture sets it false.

- **P11 — narrow the "byte-identical to `git diff --patch`" claim, or test it
  to the claim.** Both deep reviewers flagged it. Verified since: our
  `quote_c_style` is correct, including that a space is *not* quoted (git
  disambiguates it with a trailing tab instead) — so the reviewers' specific
  charge was wrong. But the quoting is unit-tested against our belief about
  git rather than against git itself, in a crate whose whole discipline is
  real-git-as-oracle. Untested inputs that would settle it: paths containing a
  quote, a tab, or non-ASCII bytes; combined mode-and-content change; empty-file
  transitions; one-sided missing-newline; `.gitattributes` function context;
  and oid-abbreviation growth on collision. Note non-UTF-8 *paths* cannot reach
  the renderer at all — it takes `&str` — which is a boundary to document
  rather than test.

- **P12 — `status` fails the whole call on one over-cap tracked file, and the
  guide implies otherwise.** `read_worktree_blob` returns `BlobTooLarge` and
  `status` propagates it, so a repository holding any tracked file over
  `max_blob_bytes` (8 MiB default) gets exit 1 and no report at all. That is
  deliberate and tested (`a_tracked_file_over_the_blob_cap_is_refused`) — a
  loud refusal beats an unbounded read — but `embedding-git.md`'s Limits table
  says "the read is declined and reported", which reads as *that file* being
  declined while the rest of the report arrives. For a code-review server
  pointed at real repositories (vendored binaries, fixtures, models) this is
  common, and real `git status` handles it fine. **Decide** whether whole-call
  failure is right, then make the guide say what the code does.

  **Decided 2026-08-23 (Amy): per-file decline, the report continues.** The
  oversized file is marked as not compared and the rest of the report arrives.
  Three reasons it went that way rather than keeping the refusal:

  - **The vocabulary already exists.** `lines_capped` and `hunks_capped` both
    mean "we declined to read this, and nothing else". A third member of that
    family is the shape this crate already teaches an agent to read, and it
    keeps one term with one meaning.
  - **The refusal was the odd one out, not the rule.** `show` declines a single
    over-cap blob; `diff --patch` caps hunks per file and keeps going. Only
    `status` escalated a per-file limit to the whole call.
  - **It is what the message and the guide already said.** `BlobTooLarge` reads
    "it will not read this one. Raise the cap to include it", and the Limits
    table reads "the read is declined and reported". Both describe the behavior
    being adopted here, which is why the mismatch was reported as a doc bug
    rather than noticed as a behavior one.

  Not free, and the cost is the reason the original refusal was defensible: a
  partial report can be mistaken for a complete one. So the decline must be
  **visible in the report itself**, not only on stderr — an agent reading
  `--json` has to be able to tell that a path was skipped without parsing prose.
  `a_tracked_file_over_the_blob_cap_is_refused` inverts into a test that the
  report arrives *and* names the skipped file, and it needs a negative control
  proving an under-cap file in the same repository is still compared normally.

## git — cost shapes `--limit` does not bound

Found by the 2026-08-22 cross-model review (kaibo default cast, whole files,
no diff) over the merged PR 5 + PR 8 round. None is a memory-safety bug; each
is a cost an embedder running a long-lived server should be able to see coming.
Grouped because they share one cause: **flatten-and-compare has to look at
every path before it knows which ones changed**, so per-invocation cost tracks
repository size, not result size.

- **G7 — worktree hashing is unbounded per invocation.** `diff` and `status`
  both hash every tracked index entry against its worktree file
  (`verbs/diff.rs:422-450`, `verbs/status.rs:467-521`). `max_blob_bytes` caps
  each file; nothing caps the count, and `--limit` does not apply — it bounds
  rows and blob reads after the comparison. A repository with a million
  tracked files costs a million reads per call. For a long-lived server
  pointed at a repository chosen by a caller, that is a CPU/IO denial of
  service that no existing knob prevents. A `max_files_examined` limit would
  bound it; the argument against is that a partial `status` is a wrong
  `status`, so the honest form is a refusal, not a truncation. Not built —
  the shape of the first real report should decide it.

- **G8 — `flatten_tree` bounds depth, not width.** `diffcore.rs:157-198` caps
  depth at 64 but the output `PathMap` grows with the tree's total leaf count.
  Shared by `status`'s `flatten_head_tree`, `log`'s `changes_against`, and
  `diff`. Same family as the `treewalk.rs` width bug fixed on 2026-08-21, but
  not the same fix: that one materialized a listing it was about to truncate,
  where this one genuinely needs every path.

- **G9 — Myers has no time bound.** `diffcore.rs:316` runs
  `gix_imara_diff::Diff::compute` with `Algorithm::Myers` over two blobs each
  capped at `max_blob_bytes` (8 MiB). Memory is bounded; time is O(ND) in the
  edit distance, so a single pathological pair of large, wholly different
  blobs can stall one call. `max_diff_files` bounds how many such pairs one
  invocation attempts, so the exposure is `limit x Myers(2 x max_blob_bytes)`.
  There is no per-diff deadline, and `ctx.patient` is not wired up either
  (see the `git log` entry above), so nothing interrupts it. **PR 6's hunk
  path does not widen this**: `diffcore::line_hunks` makes the same one
  `Diff::compute` call per file that `line_delta` makes, on the same content,
  for the same set of files — `--patch` diffs nothing the counting path would
  have skipped. It adds `postprocess_lines`, one linear pass.
  `max_hunk_bytes_per_file` bounds the hunk *output*, not the Myers run that
  precedes it, so it does not bound time either; checking it before the diff
  would mean declining a patch for any file larger than 256 KiB, which is the
  wrong answer for a one-line change in a large file.

- **G10 — `status`'s `flatten_subtree` is the one tree walk that recurses on
  the call stack.** `verbs/status.rs:822-870`, bounded at
  `MAX_STATUS_TREE_DEPTH` (256). The bound was set empirically against a 2 MiB
  thread, where overflow was measured at 700-800 levels — a ~3x margin. Every
  other walk here avoids the stack: `diffcore::flatten_tree` uses an explicit
  `Vec`, `index_depth_guard` is iterative by design, `treewalk` recurses but
  at depth 64. The margin is against a 2 MiB stack; an embedder running the
  tool on a smaller thread has less than they would read from the constant.
  Either convert it to an explicit stack like its siblings, or state the
  thread-stack assumption where an embedder will see it. **The doc half is
  done** (`docs/embedding-git.md`); the conversion is not.

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
  A silent behavior regression across a 0.16.x patch would reach us as a
  mystery, not a failure. Pick the behaviors the tool crates actually depend
  on and pin one test each. Cheap, and it is exactly the shape that would have
  caught the 0.15.0 undeclared breaking changes early.

  **Started, on the git side.**
  `crates/kaish-tools-git/tests/kaish_behavior_canary.rs` pins what `$(git …)`
  binds. The 0.16 bump motivated it and is also the argument for the rest: it
  compiled clean with 25 test binaries green while its changelog carried a
  dozen behavior changes no compiler could see.

  **Still open**, and mostly curl's, since that is where the argv surface is:
  argv binding order under `with_raw_argv`, `GlobalFlags` handling, and
  `ToolCtx::resolve_path` refusal shapes. Curl has no harness for it yet — no
  `kaish-kernel` dev-dependency, which is what the git canary needs to build a
  real `Kernel`. Adding one is the first step and the same trade git already
  made: dev-only, out of the published graph.

- **X4 — done 2026-08-23: kaijutsu told that #385/#386 are in a published
  release.** Both halves are in the `v0.16.0` tag — #385 (`94605b43`, a
  condition's stderr) and #386 (`d8ccd35a`, its stdout) — confirmed with
  `git tag --contains` rather than read off the changelog, since the obligation
  was specifically *published, not merged*.

  **The fix is unexercised, and that is the status to carry.** kaijutsu's gate
  guards are rc scripts, and an rc shell differs from an interactive one there;
  verifying one means creating a context against a live kernel running the new
  build, which is their operator's call. They have been bitten twice by scripts
  that passed in a shell and failed at context create, silently both times. So
  nobody should read "0.16 shipped it" as "deny reasons arrive now" — they will
  report either way once it runs. Kept here rather than deleted because the
  obligation was discharged and the outcome is still open.

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
every kaish crate and the `Tool` trait does not match. Whoever picks this up
should check the minor first and discover it here rather than at link time.

**As of 2026-08-23 the arithmetic stopped being the blocker.** This workspace
is on 0.16 and kaibo was heading there from 0.14.1; if they have landed it,
the two unify and the remaining questions are the VFS ones above rather than
the version. Confirm kaibo's actual pin before relying on this — it is their
state, not ours, and this note is a report of what they said they were doing.

The kaibo session has an endpoint-by-endpoint input-shape table pulled from
the live OpenAPI spec; ask for it when this starts.
