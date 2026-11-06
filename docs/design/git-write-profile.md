# kaish-git — the commit profile (design, 2026-11)

Status: **draft**. Policy sections record Amy's rulings of 2026-11-06. The
plumbing sections rest on `gix-write-feasibility-2026-11.md` (source-read,
file:line into the pinned crates). Sections marked *(pending)* wait on
`git-write-survey-2026-11.md`.

This replaces architecture.md B.11's "Commit profile" sketch and C.3's
ledger-based gate. Both assumed kaish's approval ledger, which kaish removed
before 0.14 (kaish `docs/EMBEDDING.md`, "Why this, and not a gate").

## 0. Decisions at a glance

| # | Decision | Where |
|---|---|---|
| W1 | Writes compile only with the `commit` cargo feature, off by default | §2 |
| W2 | Writes run only on a `WritableGitConfig`, which an embedder must construct by name | §2 |
| W3 | Per-command approval is the embedder's, through kaish `plan` and the schema's `operations` ids. The tool holds no approval state | §2 |
| W4 | History rewrites follow `RewritePolicy::{Refuse, Unpublished, Allow}`; default `Unpublished` | §3 |
| W5 | First cut: `add`, `rm`, `restore --staged`, `commit` (`-m`, `-F`, `-a`, `--amend`), `branch <name>`, `tag <name>`, `switch -c` | §4 |
| W6 | Nothing writes the working tree. `switch <existing>` and `restore <file>` are refused in this cut | §4 |
| W7 | Hooks: refuse when the repository has one, unless the embedder installs a `GitHooks` provider | §5 |
| W8 | Signing: refuse when `commit.gpgSign`/`tag.gpgSign` is true, unless the embedder installs a `CommitSigner` | §5 |
| W9 | Content conversion (`filter=`, `text`/`eol`, `core.autocrlf`): refuse for the affected path. No provider in this cut | §5 |
| W10 | Every ref move is compare-and-swap against the oid we read, and writes a reflog entry in git's format | §6 |
| W11 | The read surface stays provably read-only with `commit` compiled in (D.1 invariant, D.4 fingerprint) | §7 |
| W12 | Oracle tests: a commit we write is byte-identical to the one real git writes from the same index, identity, and dates | §8 |
| W13 | Every directory a write touches under the git dir is `lstat`-checked first; a symlink refuses the write | §6.1 |
| W14 | Identity: the embedder may pin it on `WritableGitConfig`; otherwise kernel variables; neither refuses | §5.4 |
| W15 | Loose objects are fsynced before the ref transaction that makes them reachable | §6 |

## 1. Why these verbs

Amy, 2026-11-06: *"the git write side ... for the common path of commit
workflows. --amend will be interesting, I'd like it to be switchable, to
disallow it when a linear clean history is preferred. this way we can put off
force push longer since amends are the main reason they become necessary."*

The 2026-10 survey (tasks 18–24) found that every model, in every run, typed real
git for the commit path: `add`, `commit -m`, `commit --amend -m`,
`checkout -b`/`switch -c`, `restore --staged`, `restore FILE`, `stash`.
*(pending: the 2026-11 write survey's ranked spellings replace this paragraph.)*

## 2. The gate

Three layers. Each one is enough to stop an unintended write, and none of them
depends on the others.

1. **Compile time.** Write verbs live under `src/verbs/write/` behind
   `#[cfg(feature = "commit")]`. A build without the feature has no write code
   in it (architecture.md D.1 is unchanged for that build).
2. **Construction.** `GitConfig::read_only()` stays the only plain
   constructor. The write verbs exist only on the type that this returns:

   ```rust
   #[cfg(feature = "commit")]
   impl GitConfig {
       /// Enable the commit profile: `add`, `rm`, `restore --staged`,
       /// `commit`, `branch <name>`, `tag <name>`, `switch -c`.
       pub fn with_commit_profile(self) -> WritableGitConfig;
   }

   #[cfg(feature = "commit")]
   impl WritableGitConfig {
       pub fn with_rewrite_policy(self, p: RewritePolicy) -> Self;
       pub fn with_hooks(self, h: Arc<dyn GitHooks>) -> Self;
       pub fn with_signer(self, s: Arc<dyn CommitSigner>) -> Self;
       pub fn without_verb(self, v: Verb) -> Self; // still subtractive
   }

   pub fn tool(config: impl Into<ToolConfig>) -> Result<GitTool, ConfigError>;
   ```

   The name says what it enables. It carries no authorization, which is the
   part of C.3 that survives.
3. **Per command, the embedder's.** kaish's model since 0.14: the embedder
   `plan`s a statement, decides with its own policy, and runs it or does not.
   We help it decide without knowing tool names: each write leaf in the
   schema declares `operations` ids, and the root lists their union (the rule
   `ToolSchema::operations` states for a tool with several behaviors).

   | id | Declared by |
   |---|---|
   | `git.index` | `add`, `rm`, `restore --staged`, `commit -a` |
   | `git.commit` | `commit` |
   | `git.ref` | `commit`, `branch <name>`, `tag <name>`, `switch -c` |
   | `git.rewrite` | `commit --amend` (and any later rewrite verb) |

   The `git.` namespace is ours (kaish's operation module reserves only `fs.`
   and `trash.`). `git.rewrite` is on the `commit` leaf even when the policy is
   `Refuse`, because the schema is reflected before argv is known.

   **kaish ask (not filed):** `plan` reports a command's name and argv, not the
   subcommand path it resolves to. An embedder that wants "this statement
   runs `git commit`, which declares `git.rewrite`" has to walk the schema
   itself. A `plan` that reports the resolved leaf's `operations` would close
   that. Amy decides whether to file it.

## 3. History rewrites

```rust
#[non_exhaustive]
pub enum RewritePolicy {
    /// Never replace a commit that exists. `--amend` exits 2 (usage).
    Refuse,
    /// Replace a commit only when no `refs/remotes/*` ref reaches it.
    /// This is the case that can never need a force push. The default.
    Unpublished,
    /// Replace any commit on the current branch.
    Allow,
}
```

- **What counts as a rewrite:** anything that moves a branch to a commit that
  is not a descendant of where it was. In this cut that is only
  `commit --amend`. Later candidates (`reset --soft HEAD~N`, `commit --fixup`
  + autosquash) go through the same check, so the policy is about the ref
  move and not about a flag.
- **`Unpublished` reachability:** is `HEAD` an ancestor of (or equal to) any
  ref under `refs/remotes/`? A walk down from each remote tip that looks for
  HEAD costs the whole history behind the tip when the answer is "no", which
  is the answer we want (feasibility §5). Use git's merge-base shape instead
  (`paint_down_to_common`): walk from HEAD and every remote tip at once in
  committer-time order, mark each commit with the sides that reach it, and
  stop when every queued commit is reached by both. HEAD is published if a
  remote side reaches HEAD itself. For HEAD one commit ahead of `origin/main`
  this examines a few commits. Use commit-graph generation numbers when the
  file exists. The walk keeps `reach.rs`'s 100,000-commit budget; exhausting
  it refuses (a wrong "unpublished" is the dangerous error, a refusal is
  not). A shallow repository refuses `--amend` under `Unpublished`: the
  history that would answer is missing.
- **What the remote-tracking refs can and cannot tell us.** They are the last
  fetch, not the remote. A commit pushed by another clone since the last fetch
  looks unpublished. The refusal text and `docs/git.md` say this in those
  words. It is the same information real git's `--force-with-lease` uses.
- **Refusal text** *(pending: second-turn survey results pick the wording).*
  Shape: lead with the consequence, name the policy and the commit, give the
  one command that recovers:

  > `git commit --amend`: refused — HEAD a1b2c3d is on origin/main, and this
  > shell does not rewrite published commits. Make a new commit instead:
  > `git commit -m '<message>'`

## 4. Verb surface

*(pending: spellings and flags from the write survey.)* Minimum spellings,
each checked against real git:

| Verb | Accepts | Refuses, naming the next step |
|---|---|---|
| `add` | `<paths>`, `-A`/`--all`, `-u`/`--update`, `.` | `-p`/`-i` (interactive), `-f` past ignore rules (open: see §9) |
| `rm` | `<paths>`, `--cached`, `-r` | `-f` when the worktree file differs from the index (git refuses too) |
| `restore --staged` | `<paths>`, `.` | `restore <file>` without `--staged` (writes the worktree; W6) |
| `commit` | `-m` (repeatable, paragraphs as git), `-F <file>`, `-a`, `--amend` (policy), `--allow-empty`, `--author` (open: see §9) | `--no-verify` when hooks exist (§5), `-S`/`--gpg-sign` without a signer, `--fixup`/`--squash` (later) |
| `branch <name> [<start>]` | create only | `-d`/`-D`/`-m`/`-f` |
| `tag <name> [<rev>]`, `tag -a -m` | lightweight and annotated | `-d`, `-f` |
| `switch -c <name>` / `checkout -b <name>` | create at HEAD and move HEAD (no worktree change) | `switch <existing>`, `checkout <branch>` (W6), `-c` with a start point that is not HEAD (it would change the worktree) |

Repository states we refuse to write in (exit 4, naming the state and the
real-git command that ends it):

- merge, rebase, cherry-pick, revert, or bisect in progress — any of
  `MERGE_HEAD`, `CHERRY_PICK_HEAD`, `REVERT_HEAD`, `REBASE_HEAD`,
  `rebase-merge/`, `rebase-apply/`, `sequencer/`, `BISECT_LOG` in the
  per-worktree git dir; or any index entry with a stage other than 0;
- a split index (`link` extension) or sparse index — `gix-index` would
  silently dissolve or drop them (feasibility surprise 5);
- `core.symlinks=false`, `core.ignorecase=true`, `i18n.commitEncoding` set,
  a non-default `commit.cleanup`;
- sha256 object format (the read side already refuses it).

Allowed, as git allows them:

- **Unborn branch:** the first commit has no parent and a
  `commit (initial): <subject>` reflog message.
- **Detached HEAD:** commit moves `HEAD` itself; stderr names the new oid and
  says `git switch -c <name>` keeps it on a branch.
- **Linked worktree:** `HEAD`, `index` and `logs/HEAD` live in the
  per-worktree git dir; objects and refs in the common dir.
- **Shallow:** commit is allowed; `--amend` under `Unpublished` is not (§3).

**Message cleanup** for `-m`/`-F` follows git's default `whitespace` mode:
strip trailing spaces, drop leading and trailing blank lines, collapse runs of
blank lines, end with one `\n`. Each `-m` is a paragraph. An empty message
after cleanup refuses, as git does. `commit.template` does not apply (no
editor).

## 5. What a build that cannot spawn must not pretend to do

Amy, 2026-11-06: *"refuse, but we should make sure a provider can implement
hooks if it wants to."*

### 5.1 Hooks

```rust
pub trait GitHooks: Send + Sync {
    /// Called before the commit object is written. `Err` aborts the commit
    /// and its text is the refusal.
    fn pre_commit(&self, ctx: &HookContext<'_>) -> Result<HookOutput, HookError>;
    /// May rewrite the message, as git's commit-msg hook may.
    fn commit_msg(&self, ctx: &HookContext<'_>, message: &mut String) -> Result<HookOutput, HookError>;
    /// After the ref moved. Errors are reported, not fatal (git's rule).
    fn post_commit(&self, ctx: &HookContext<'_>, commit: ObjectId) -> HookOutput;
}
```

`HookContext` carries the repository's real paths (worktree, git dir), the
tree oid about to be committed, the hook directory we resolved, and the names
of the hook files present. A provider decides how to honor them: spawn the
real scripts, run its own checks, or decline. Without a provider:

- **A hook file present** → the write refuses, exit 4, naming the hook and
  saying to commit with real git or ask the embedder for a hook provider.
  "Present" means a non-`*.sample` file under `<common_dir>/hooks/` (or
  `core.hooksPath`, which also counts when set at all) named for a hook git
  would run on that write: `pre-commit`, `prepare-commit-msg`, `commit-msg`,
  `post-commit`, `post-rewrite` (amend), and `reference-transaction` (git
  runs it on every ref update, so it gates `branch` and `tag` too).
- **No hook file** → commit proceeds. Nothing is skipped, so nothing is said.
- **`--no-verify`** is refused whenever a hook is present and no provider is
  installed (Amy, 2026-11-06: refuse always). We cannot run the hook, so
  honoring the flag would look exactly like a silent bypass. With a provider
  installed, `--no-verify` is passed to it and the provider decides.
- **User-level config is invisible.** A human's `~/.gitconfig` can set
  `core.hooksPath`, `commit.gpgSign` or `core.autocrlf`, and we never read it
  (hermetic, D.2). The `commit` tool description says so in plain words: this
  git never runs hooks or signs unless the embedder provides that, whatever
  the host's git configuration says.

`core.hooksPath` is repo-local config we already read deliberately (D.2). A
`hooksPath` outside the mount is reported by name only, never resolved
(existence-oracle rule, as G4).

### 5.2 Signing

**Refused in this cut; the provider trait comes later** (Amy, 2026-11-06).
`commit.gpgSign`, `tag.gpgSign` or `tag.forceSignAnnotated` true (repo-local)
refuses `commit` / `tag -a` with exit 4. `-S`/`--gpg-sign` refuses with exit 2.
The later shape: `CommitSigner::sign(&self, payload: &[u8])`, called with the
object bytes, returning the signature we insert as `gpgsig`.

### 5.3 Content conversion

`add` stores worktree bytes as they are. A path whose attributes name a
`filter=` driver, or `text`/`eol` conversion, or a repo with `core.autocrlf`
set to `true`/`input`, would get different bytes from real git. That is the
"silently different commit" we will not write: `add` refuses that path
(exit 4) naming the attribute. LFS repositories are the common case this
catches. A conversion provider is a later question; nobody has asked for it.

**Most repositories pass.** `* text=auto` is GitHub's default, so refusing
every repository that sets it would make the profile useless. The
conversions git applies on `add` are identities on bytes with no `\r` and no
`$Id` (feasibility §6, probed: under `* text=auto`, `x\r\ny\r\n` and `x\ny\n`
hash to the same blob, so raw staging of the CRLF file is wrong). The rule:

1. A path matched by a `filter=` attribute whose driver has a `clean` or
   `process` entry in repo-local config → refuse.
2. A path matched by `working-tree-encoding` → refuse.
3. Otherwise, if the bytes contain no `\r` and no `$Id` → stage them raw.
4. Otherwise, if any conversion could apply (`core.autocrlf` true/input,
   `core.eol`, or a `text`/`eol`/`crlf`/`ident` attribute on the path) →
   refuse that path, naming the attribute.

The matcher is ours, built on the already-pinned `gix-glob`, reading
`.gitattributes` from the root down to the path's directory plus
`info/attributes`. No new crate. It over-approximates where that is safe (a
`[attr]` macro counts as a possible conversion), so it can refuse a path git
would stage raw, never the reverse. Reading attributes to *refuse* is a
different act from reading them to *run* a driver (D.3), and it is the only
use.

### 5.4 Identity

Amy, 2026-11-06: the embedder pins, kernel variables are the fallback.

- `WritableGitConfig::with_identity(author, committer)` fixes both. When set,
  kernel variables are not read. `--author` may still set the author (git
  allows it); nothing sets the committer.
- Otherwise E.6 applies: `GIT_AUTHOR_NAME`/`_EMAIL` required,
  `GIT_COMMITTER_*` default to the author, `GIT_AUTHOR_DATE`/
  `GIT_COMMITTER_DATE` optional.
- Neither → refuse, exit 2, naming both the variables and that the embedder
  can pin an identity.
- Repo-local `user.name`/`user.email` are not read. An agent that ran
  `git config user.name` elsewhere gets the refusal above, which names what to
  set instead.
- The reflog line uses the committer.

## 6. Refs, reflogs, and the index

- **Order of a commit:** lock `index.lock` (for `-a`), write blobs, build and
  write trees, write the commit, fsync the new loose objects, then the ref
  transaction. A crash before the transaction leaves unreachable objects,
  which is harmless; a crash after it leaves a ref whose objects are on disk.
  The pinned crates fsync nothing (feasibility surprise 3), so the fsync is
  ours.
- **Compare-and-swap.** Every ref update names the oid we read as its
  expected previous value (`MustExistAndMatch`, or `MustNotExist` for a
  create). If the ref moved in between, the transaction fails and we exit
  1 naming the ref, both oids, and "run `git status` and retry". No retry
  loop.
- **Create is our check, not gix's.** `MustNotExist` passes when the ref
  exists at the same oid, and a corrupt loose ref reads as absent
  (feasibility §4). `branch x` / `tag x` / `switch -c x` look the name up
  first and refuse with git's text (`a branch named 'x' already exists`).
- **Reflog** for every ref move, in git's message format (probed against git
  2.56: `commit: <subject>`, `commit (amend): <subject>`,
  `commit (initial): <subject>`, `branch: Created from HEAD`,
  `checkout: moving from <old> to <new>`). gix writes no `HEAD` line for a
  symbolic update, so `switch -c` appends its `checkout:` line itself. A
  message containing `\n` or `\r` is refused before the transaction:
  gix-ref 0.66 declares that check and never runs it, so a newline would
  forge a second reflog line. The reflog is what makes an amend
  recoverable, so `Unpublished` and `Allow` depend on it.
- **Index write:** take `index.lock` first, read the index from bytes
  (`State::from_bytes`, which keeps `link` visible so we can refuse it),
  edit, `write_to` the lock, commit the lock. `gix-index`'s `File::write`
  locks after the read, which is a lost-update window git does not have.
  Remove the cache-tree extension before every write (gix writes it stale).
  The untracked-cache, fsmonitor and resolve-undo extensions are dropped,
  as `gix-index` cannot write them; git rebuilds the first two. Entry stat
  comes from `gix_index::fs::Metadata` (lstat, git's `ctime`), not
  `std::fs::Metadata`. Racy entries are smudged on write, as git does.
- **A stale `.lock`** is not ours to remove. Exit 4 naming the lock path and
  saying another git process may be running.

### 6.1 Write containment

The read side's P13 carve-out (a symlinked loose ref can read outside the
mount, docs/issues.md) becomes a write primitive here: a symlinked
`objects/ab`, `refs/heads/sub` or `logs/HEAD` receives our write outside the
mount (feasibility surprise 1, with file:line). Before each write, every
directory component from the git dir down to the target — and the target
itself, for appends — is `lstat`ed; a symlink or a non-directory refuses the
write, exit 4, naming the path relative to the git dir only (existence-oracle
rule). The threat is a hostile repository at rest, not a concurrent attacker
racing us, and the doc says so. The planted-symlink fixture lands in PR 1,
before any verb.

## 7. Keeping the read surface provably read-only

*(pending: feasibility §7.)* Requirements:

- The D.4 fingerprint test runs every read verb with `commit` compiled in and
  a `WritableGitConfig` installed, and still asserts `.git` unchanged.
- D.1's source scan widens: write-shaped plumbing may appear only under
  `src/verbs/write/` and the `WriteRepo` type.
- A new fingerprint test runs every *refused* write spelling and asserts
  `.git` unchanged. A refusal that wrote half a transaction is the failure
  mode this catches.

## 8. Tests

Real git is the oracle, as on the read side:

- **Commit identity.** Same index, identity, dates, and message → our commit
  oid equals the oid `git commit` writes (`GIT_AUTHOR_DATE`/
  `GIT_COMMITTER_DATE` fixed). One assertion covers tree building, object
  encoding, header order, and message normalization.
- **Index equality.** After our `add`/`rm`/`restore --staged`,
  `git ls-files -s --debug` matches what git's own verb produced on a twin
  repository; `git status --porcelain=v2` agrees; `git fsck --strict` is
  clean.
- **Reflog equality** (minus timestamps) against git on a twin.
- **Policy negative controls.** Each `RewritePolicy` setting with a published
  and an unpublished HEAD: four refusals asserted by exact text, two
  successes asserted by the resulting reflog. A test that only asserts
  refusals passes over a build where amend never works.
- **Hostile repository.** `PwnRepo` gains hooks and a signing config;
  without providers every write refuses and no sentinel appears.

## 9. Settled 2026-11-06

- Identity: embedder pins, kernel variables fallback (§5.4).
- `--no-verify` with a hook and no provider: refuse always (§5.1).
- Signing: refuse now, trait later (§5.2).
- `add -f` past an ignore rule: accept, as git does. Plain `add` of an
  ignored path refuses with git's hint naming `-f`.

What this changes elsewhere, in the same PR: architecture.md B.11's "Never"
list loses `commit --amend` and `switch -c`/`checkout -b`; C.3's
`ctx.request_approval` paragraph is replaced by §2 here; A.2/A.3's drift
(a `read` feature and a wasm escape feature the manifest does not have) is
corrected.

## 10. Phasing *(pending)*

1. `WriteRepo`, the gate, write containment (§6.1) with its planted-symlink
   fixture, the widened D.1 scan (feasibility §7 lists the needles), both
   fingerprint tests, then objects + index, `add`/`rm`/`restore --staged`.
2. `commit` (`-m`, `-F`, `-a`), identity, reflog, hooks and signer
   boundaries.
3. `--amend` and `RewritePolicy`.
4. `branch <name>`, `tag <name>`, `switch -c`/`checkout -b`.
