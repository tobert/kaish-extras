# `kaish-tools-git` — the crates.io version timeline

Decision needed before the first publish from this repo. Not made here —
Amy's call — but the options and their consequences are laid out below so the
call is a quick read, not a re-derivation.

## The situation, verified against crates.io (2026-08-21)

`kaish-tools-git` already exists on crates.io, owned by `tobert` (Amy) — no
naming conflict, just a prior life:

| Version | Published  | Yanked |
|---------|------------|--------|
| 0.8.4   | 2026-06-15 | No     |
| 0.8.3   | 2026-06-14 | No     |
| 0.8.2   | 2026-06-12 | No     |
| 0.8.1   | 2026-06-11 | No     |
| 0.8.0   | 2026-06-08 | No     |

Its description: *"kaish git tool bundle: the GitVfs backend and the git
builtin (libgit2)"*, repository `github.com/tobert/kaish` — the old kaish
monorepo, pre-rewrite. That crate is libgit2-backed, has a `GitVfs` backend,
and a write-capable builtin. This repo's `kaish-tools-git` is a from-scratch
rewrite: gitoxide plumbing (not libgit2, not even the `gix` facade), read-only
by construction, no `GitVfs`. It shares a name and an owner with the old
crate; it does not share a lineage, an API, or a feature set. Workspace
version here is `0.1.0`, never published in this form.

Publishing `0.1.0` under a name whose `max_version` is `0.8.4` is legal — cargo
does not forbid publishing a lower version number after a higher one exists —
but it is confusing in a specific, mechanical way: cargo's resolver treats
version *numbers*, not publish dates, as the ordering. A fresh
`cargo add kaish-tools-git` (no version pinned) will keep resolving to `0.8.4`
— genuinely the higher version number — so the new gitoxide-based code would
never be what a new consumer gets by default. The crates.io page and docs.rs
would keep showing 0.8.4 as "newest" until something changes that.

## The options

### (a) Yank 0.8.x, publish 0.1.0

Yanking removes 0.8.0–0.8.4 from **fresh** dependency resolution — a
`cargo add` or a `cargo update` with no existing lock entry can no longer pick
them — while leaving them downloadable for anyone whose `Cargo.lock` already
pins one (yanking does not break an existing lock). `0.1.0` then becomes the
only resolvable version, so a fresh install gets the gitoxide code.

Cost: this is the most disruptive option to any real consumer of the *old*
crate. Someone who depends on `kaish-tools-git = "0.8"` for the libgit2
`GitVfs` backend, without a committed lockfile (a CI matrix job, a fresh
container build), loses the ability to resolve *any* version that has that
backend — `0.1.0` does not have it, does not have write support, and does not
have the same API at all. Yanking here does not just "hide an old version," it
tells cargo there is no longer a version of this crate that does what the old
one did, which is not really true — the old code still exists, it is just not
what this repo publishes.

### (b) Publish forward as 0.9.0, keep the timeline monotonic

Bump straight to the next 0.x minor rather than resetting to 0.1.0. No
yanking, no touching the existing 0.8.x releases at all.

Why this is the recommended option:

- **Existing consumers are unaffected.** `^0.8` (what `kaish-tools-git = "0.8"`
  means under Cargo's SemVer rules) is `>=0.8.0, <0.9.0` — `0.9.0` does not
  satisfy it, so nobody currently locked to the old libgit2 crate moves
  automatically. They keep building against 0.8.4 until they deliberately
  widen their own requirement.
- **The timeline stays honest.** crates.io and docs.rs show `0.9.0` as
  newest, which it genuinely is — no version-ordering confusion for anyone
  reading the page.
- **A 0.x minor bump is exactly where SemVer already says "no compatibility
  promise."** Cargo treats every 0.x minor boundary as a breaking-change
  boundary by convention; landing a total rewrite at 0.8→0.9 is not stretching
  that contract, it is precisely what it is for.
- **Nothing is deleted.** The old crate stays exactly as reachable as it is
  today for anyone who still wants it, deliberately (`kaish-tools-git = "=0.8.4"`
  or similar).

Residual friction, disclosed rather than hidden: the `description` and
`repository` fields swing hard between 0.8.4 and 0.9.0 (libgit2 → gitoxide,
`tobert/kaish` → `tobert/kaish-extras`) — a reader diffing the two versions on
crates.io will notice a rewrite, not a patch. That is the honest signal, not a
defect; a one-line note in this crate's README calling out the architecture
change would make it legible without digging into the diff (worth doing
regardless of which option is picked, since the README's "PR 1, one verb so
far" language was already stale before this pass — see the same commit that
adds this document for the fix).

### (c) Something else

- **A new crate name** (`kaish-tools-git2`, a scoped alternative) sidesteps
  the timeline question entirely, at the cost of abandoning the established
  name's discoverability and leaving the old crate's page frozen at 0.8.4
  forever with no forward pointer. Not recommended: Amy owns the name and the
  new crate fills the same conceptual role (a git tool for kaish embedders),
  so reusing it is not a namesquat — it is the normal case of a rewrite
  keeping its name.
- **Yank later, once 0.9.x has had time to prove itself**, effectively (b)
  now and revisit (a) as a follow-up once there is real signal about whether
  anyone still depends on the 0.8.x libgit2 line. Reasonable, and compatible
  with picking (b) today.

## Recommendation

**(b) — publish as `0.9.0`.** It is the only option that changes nothing about
what already exists on crates.io while still giving a fresh install the
gitoxide-based code by default. Revisit yanking 0.8.x only if there is a
concrete reason to (e.g., a security issue in the old libgit2 code that needs
loud retirement) — that is a decision to make with real information, not a
prerequisite for shipping this rewrite.
