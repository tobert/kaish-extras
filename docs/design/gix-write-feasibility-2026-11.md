# gix write feasibility for the `commit` profile — source-read verification (2026-11-06)

> Method copied from [gix-plumbing-feasibility-2026-08.md](gix-plumbing-feasibility-2026-08.md):
> every claim is cited to the pinned sources on disk under
> `~/.cargo/registry/src/index.crates.io-*/`, with `crate-version/path:line`.
> Version-specific facts drift; re-verify on any pin bump. Two claims were
> also checked against real git 2.56.0 in a throwaway repository (marked
> **[probed]**); everything not marked is read from source and **not** run.
> No crate code was changed. `cargo tree -i` was run with
> `flock ~/.cache/zorak-heavy.lock`.
>
> Pins read: `gix-odb 0.83.0`, `gix-object 0.63.0`, `gix-index 0.54.0`,
> `gix-ref 0.66.0`, `gix-lock 24.0.0`, `gix-tempfile 24.0.0`, `gix-zlib 0.1.0`,
> `gix-actor 0.41.2`, `gix-validate 0.11.3`. Our side: `crates/kaish-tools-git`
> at `372c7f5`.

## Verdict summary

| # | Question | Verdict | One line |
|---|---|---|---|
| 1 | Write blob/tree/commit/tag | **Feasible, no new dependency** | `gix_object::Write` on the `gix_odb::Handle` we already hold; zlib-rs level 1, mode 0444, tempfile + rename, **no fsync**. |
| 2 | Tree from index | **Hand-write (~80 lines)** | Nothing in `gix-index` writes a tree. `gix_object::Tree` has git's sort order but only `debug_assert`s it. Drop the cache-tree extension. |
| 3 | Update the index | **Feasible with care; one hand-written racy-git step** | `gix-index` writes entries, `tree`, `sparse` only. Lock **before** the read. Refuse `link` (split index). |
| 4 | Ref updates | **Feasible; two hand-written pieces** | CAS works. `switch -c` HEAD reflog is not written by gix. The reflog message is not checked for newlines. |
| 5 | "Unpublished" reachability | **Feasible, with a cost caveat** | `reach.rs::Reaches` answers it, bounded at 100,000 commits; a "no" costs the full history. |
| 6 | Refuse list | **Mostly feasible; one blocker for hermeticity** | Everything is checkable from repo-local bytes. User-level config is invisible by design. |
| 7 | D.1 / D.4 | **Viable** | `WriteRepo` wraps `ReadRepo`. The D.1 scan has real holes; widen it first. |
| 8 | wasm | **Confirmed native-only** | Matches A.3; A.3's prose has drifted from `lib.rs`. |

## Surprises that should change the design

1. **A symlinked directory under `objects/` or `refs/` becomes a write
   primitive.** `docs/issues.md` P13 records the read-side carve-out. With the
   write profile the same shape writes: `objects/ab` symlinked out of the mount
   receives the renamed object (`gix-odb-0.83.0/src/store_impls/loose/write.rs:204-211`,
   `create_dir` tolerates `AlreadyExists`, `persist` follows the directory). A
   symlinked `refs/heads/sub` receives `sub/x.lock` and the renamed ref
   (`gix-ref-0.66.0/src/store/file/transaction/prepare.rs:132`). A symlinked
   `logs/HEAD` receives an `append(true)` write
   (`gix-ref-0.66.0/src/store/file/loose/reflog.rs:116`). The write profile
   needs its own containment step before each write (section 7).
2. **The reflog message is not validated.** `Error::MessageWithNewlines` is
   declared (`reflog.rs:222`) and used nowhere in `gix-ref 0.66.0`.
   `writeln!(file, "\t{message}")` (`reflog.rs:154-164`) writes the message
   raw, so a newline forges a second reflog line. We must reject `\n` and `\r`
   ourselves.
3. **`gix_odb` loose writes have no fsync, and neither does anything else in
   the write path.** `grep -rn 'sync_all\|sync_data\|fsync'` over `gix-odb`,
   `gix-lock`, `gix-tempfile`, `gix-ref`, `gix-index` and `gix-fs` finds
   nothing. Section 1 says what that costs.
4. **`File::write` (the index) takes its lock *after* you read.**
   `gix-index-0.54.0/src/file/write.rs:67-71`. A caller that reads, edits, then
   calls `File::write` has a lost-update window git does not have (git locks,
   then reads). Take `index.lock` first and use `File::write_to` into the lock.
5. **`File::at` silently dissolves a split index**
   (`gix-index-0.54.0/src/file/init.rs:99-101`) and `State::write_to` never
   writes `link`, `UNTR`, `FSMN` or `REUC`
   (`gix-index-0.54.0/src/write.rs:108-135`). `open_index` today uses
   `State::from_bytes`, which keeps `state.link()` visible; the write path must
   keep using it and refuse on `link`.
6. **`Stat::from_fs` takes `gix_index::fs::Metadata`, not
   `std::fs::Metadata`** (`gix-index-0.54.0/src/entry/stat.rs:86`). The
   wrapper uses `rustix::fs::lstat` and reports `st_ctime` as the "created"
   time (`src/fs.rs:84-101`); `std::fs::Metadata::created()` would be the
   birth time, which is not what git stores.
7. **The ledger gate does not exist in the published API.** `architecture.md`
   C.3 and D.1 rest on `ctx.request_approval(...)`. `grep -rin
   'approval\|latch'` over `kaish-tool-api 0.18.1` and `kaish-types 0.18.1`
   finds nothing. A write verb cannot fail closed through the ledger until a
   kaish PR lands; until then the only gate is that the embedder opted into
   the profile (C.3's type gate, itself not yet written: `config.rs:31-39` has
   `Profile::Read` only).
8. **B.11's "Never" list names `commit --amend`, `switch`, `checkout`.**
   `architecture.md:703-707`. The requested profile (`switch -c`, amend under
   an "Unpublished" policy) reverses that list. Edit B.11 in the same PR, on
   purpose.
9. **Hermetic identity cuts both ways.** We read only `<common_dir>/config`
   (`repo.rs:1213-1238`). A human's `~/.gitconfig` with `commit.gpgsign`,
   `core.hooksPath` or `core.autocrlf` is invisible to us, so a refusal that
   keys on repo-local config cannot see them. The write profile must state, in
   its tool description, that it never signs and never runs hooks whatever the
   host's user config says (section 6).

---

## 1. Writing objects

**Verdict: feasible, no new dependency, no `gix-command` / `gix-filter`.**

### The path

- `gix_odb::at(&objects_dir)` returns `Handle = Cache<store::Handle<Arc<Store>>>`
  (`gix-odb-0.83.0/src/lib.rs:182-199`). `repo.rs:384` already builds exactly
  that and `ReadRepo::objects()` lends it (`repo.rs:666`).
- `Cache<S>` implements `gix_object::Write` (`gix-odb-0.83.0/src/cache.rs:144`),
  delegating to `store::Handle<S>` (`src/store_impls/dynamic/write.rs:27`),
  which writes to `snapshot.loose_dbs.first()` (`write.rs:33`): the repository's
  own `objects/`, never an alternate.
- The loose `Store` does the I/O (`src/store_impls/loose/write.rs:29-139`):
  1. `tempfile::Builder::tempfile_in(<objects dir>)`, mode `0o444` on unix
     (`:168-177`). The temp name is the `tempfile` crate's `.tmpXXXXXX`; git's
     is `tmp_obj_*`, so `git gc` will not prune a leftover.
  2. Header (`loose_header`), then payload, through a
     `gix_zlib::stream::deflate::Write` at `BEST_SPEED` (level 1,
     `gix-zlib-0.1.0/src/lib.rs:19`), the same default git uses for
     `core.looseCompression`. The deflater is `zlib-rs`
     (`gix-zlib-0.1.0/src/stream/deflate.rs:50`); `flush()` is `Finish`
     (`:222-224`).
  3. SHA-1 computed in the same pass (`gix_hash::io::Write`, `write.rs:159-161`).
  4. `fs::create_dir(objects/xx)` with `AlreadyExists` ignored (`:204-210`), then
     `file.persist(object_path)`, a rename (`:211`). A rename over an existing
     identical object is harmless; there is no "already exists, skip" check.
- Compression level is `store::init::Options::loose_compression`
  (`dynamic/init.rs:30,41`), settable through `gix_odb::at_opts`. The read path
  uses the default; honoring `core.looseCompression` means opening a second
  handle with `at_opts`, or accepting level 1 (git's default).

### What each object kind needs from us

- **Blob**: `write_buf(Kind::Blob, bytes)` on raw bytes. Anything git would do
  to the bytes first (autocrlf, `text`, `ident`, filters) is section 6.
- **Tree**: `gix_object::Tree { entries }` is `WriteTo`
  (`gix-object-0.63.0/src/tree/write.rs:26`). `Entry`'s `Ord` is git's tree
  order (dirs compare as `name/`, `tree/mod.rs:367-377`). **Write checks
  sorting only with `debug_assert_eq!` (`write.rs:29`), so a release build
  writes an unsorted tree without complaint.** It rejects only a NUL in a name
  (`:43-48`); it does not reject `/`, `.git`, or empty names. Sort and
  validate ourselves (`gix_validate::path::component`, section 2).
- **Commit**: `gix_object::Commit` (`lib.rs:149-168`). `write_to`
  (`commit/write.rs:8-27`) writes headers then `\n` then the message verbatim:
  it adds no trailing newline and does no cleanup. Signature fields go through
  `validated_token`, which rejects `<`, `>` and `\n` in name/email
  (`gix-actor-0.41.2/src/signature/mod.rs:126-131`, `:137-144`), so identity
  injection fails loud. Git also strips leading/trailing "crud" characters
  from names; we do not, and the test must say so.
- **Tag**: `gix_object::Tag` (`lib.rs:204`); `write_to` validates the name
  (`tag/write.rs:28`) and the tagger signature. Message handling: it prepends
  one `\n` unless the message is all newlines (`:35-37`).

### Crash behavior

No fsync anywhere (surprise 3). Rename-into-place is atomic with respect to a
reader, but after a power loss a renamed object can be zero-length while the
ref that names it survives. Git's default `core.fsync` is not all-on either
(not probed; verify before claiming parity). Cheap mitigation if we want it:
after the four object writes and before the ref transaction, open each written
object path (`loose::Store::object_path`, `write.rs:152-154`; the `Handle` does
not expose the loose store, so recompute `objects/xx/yyyy`) and `sync_all`, then
`sync_all` the fan-out directories. That is hand-written, optional, and belongs
behind a documented decision, not a silent default.

### Dependencies

All already linked: `gix-zlib 0.1.0`, `zlib-rs 0.6.6`, `tempfile 3.27.0`
(`gix-odb` non-dev dependency, `gix-odb-0.83.0/Cargo.toml:114`), `rustix`.
`cargo tree -p kaish-tools-git -i gix-command|gix-transport|gix-filter|signal-hook`
each report "did not match any packages" today. Nothing the write path adds
changes that. New *direct* pins needed: `gix-lock = "=24.0.0"` (already in
`Cargo.lock`) and `gix-validate = "=0.11.3"` (already in `Cargo.lock`).

```rust
// verbs/write/objects.rs  (compiled only under feature = "commit")
use gix_object::{Write as _, Kind};

fn write_blob(repo: &WriteRepo, bytes: &[u8]) -> Result<ObjectId, GitError> {
    // `compute_hash` first: skip the write when the object is already present
    // (loose or packed). The loose store does not check.
    let id = gix_object::compute_hash(repo.hash_kind(), Kind::Blob, bytes)?;
    if repo.objects().exists(&id) { return Ok(id); }
    repo.objects().write_buf(Kind::Blob, bytes).map_err(|e| GitError::repository(..))
}
```

---

## 2. Building a tree from an index

**Verdict: hand-write.**

`gix-index` has `State::from_tree` (tree to index, `src/init.rs:63`) and no
inverse: `grep -rn 'write_tree\|fn.*tree' gix-index-0.54.0/src` finds the
`tree` (cache-tree) extension and `from_tree` only. Nothing else pinned offers
it. `gix-traverse`/`gix-diff` read trees.

### Algorithm

Entries in the index are sorted bytewise on the full path, then stage. Because
`/` is a real byte in index order and tree order appends `/` to directory names
when comparing, a single pass groups consecutive entries by first path
component and the groups come out in tree order. Sort each directory's
`Vec<tree::Entry>` anyway (`Entry: Ord`), because the writer will not.

Rules, each of which needs a test against real git:

| Case | Rule |
|---|---|
| Stage != 0 anywhere | Refuse (merge in progress). Check `entry.flags.stage()`. |
| `INTENT_TO_ADD` (`entry/flags.rs:53`) | Skip: git's `write-tree` leaves it out. Not probed. |
| `SKIP_WORKTREE` | Include; its id is the index id. |
| Exec bit | `Mode::FILE_EXECUTABLE` to `EntryKind::BlobExecutable` (`0o100755`). Mapping: `Mode::to_tree_entry_mode` (`entry/mode.rs:16`). |
| Symlink | `Mode::SYMLINK` to `0o120000`; id is the link-target blob. |
| Gitlink | `Mode::COMMIT` to `0o160000`; **do not** require the object to exist. |
| Blob/link ids | Require `objects.exists(id)`; git's `write-tree` errors on a missing object. |
| Path components | `gix_validate::path::component(c, mode, Options::default())` on every component: rejects `..`, `.`, empty, `.git` (including NTFS/HFS spellings). `Options::default()` turns all protections on (`gix-validate-0.11.3/src/path.rs:63-71`). |
| Sparse index (`state.is_sparse()`) | Refuse: `Mode::DIR` entries stand for whole trees (`entry/mode.rs:39-41`). |
| Empty directories | Cannot occur; trees come only from entries. |

```rust
fn write_tree(repo: &WriteRepo, state: &State) -> Result<ObjectId, GitError> {
    fn build(repo: &WriteRepo, state: &State, entries: &[Entry], prefix_len: usize)
        -> Result<ObjectId, GitError>
    {
        let mut out: Vec<tree::Entry> = Vec::new();
        let mut i = 0;
        while i < entries.len() {
            let path = entries[i].path(state);            // full path
            let rest = &path[prefix_len..];
            match rest.find_byte(b'/') {
                None => { /* leaf: map mode/id; push tree::Entry */ i += 1; }
                Some(slash) => {
                    let name = &rest[..slash];
                    let j = i + entries[i..].iter()
                        .take_while(|e| e.path(state)[prefix_len..].starts_with_str(name)
                                     && e.path(state)[prefix_len + name.len()..].first() == Some(&b'/'))
                        .count();
                    let id = build(repo, state, &entries[i..j], prefix_len + name.len() + 1)?;
                    out.push(tree::Entry { mode: EntryKind::Tree.into(), filename: name.into(), oid: id });
                    i = j;
                }
            }
        }
        out.sort();
        repo.objects().write(&Tree { entries: out }).map_err(..)   // WriteTo; hashes while writing
    }
    build(repo, state, state.entries(), 0)
}
```

Recursion depth: the existing `index_depth_guard.rs` bounds the cache-tree
decode (docs/issues.md R4). A hand-written recursive builder needs its own
bound (use an explicit stack, or cap path depth at the same limit and refuse).

### Cache-tree extension: invalidate, do not write

`State::write_to` writes the `tree` extension as stored, "not recomputed or
invalidated" (`gix-index-0.54.0/src/file/write.rs:55-66`, `write.rs:119-121`).
A stale-but-valid cache-tree makes a later `git commit` capture old subtree
content. So: **call `state.remove_tree()` (`access/mod.rs:634`) before every
write.** The cost is that the next real `git commit` rehashes every directory;
the result is still correct. `extension::Tree` has public fields
(`extension/mod.rs:22-35`) so a later optimization could populate it from our
own write-tree result, but that is an optimization with a staleness risk and
not part of the first cut.

---

## 3. Updating the index

**Verdict: feasible; the lock order and racy-git smudge are ours to write.**

### What `gix-index` writes and drops

`State::write_to` (`src/write.rs:57-101`) writes: header (V2, or V3 if any entry
has `EXTENDED`: `detect_required_version`, `:145-150`; **V4 is never written**),
entries (skipping `REMOVE`-flagged ones, `:84-90`), the `tree` extension if
present and enabled (`:112-118`), `sparse` if `is_sparse()` (`:122-127`), and
EOIE if any extension was written. **Not written:** `link`, `UNTR`
(untracked cache), `FSMN` (fsmonitor), `REUC` (resolve-undo), `IEOT`.

| Index feature | Behavior if we read-modify-write | Policy |
|---|---|---|
| Version 4 (`index.version=4`) | Rewritten as 2/3. Valid; git reads it. | Accept, document. |
| `tree` (cache-tree) | Written stale unless removed. | `remove_tree()` always. |
| `UNTR`, `FSMN` | Dropped; git rebuilds. | Accept. |
| `REUC` (resolve-undo) | Dropped; loses `checkout --conflict`/`reset` data. | Accept (no merges in profile). |
| `link` (split index) | `File::at` dissolves it into one full index (`init.rs:99-101`); `State::from_bytes` keeps `state.link()` and `write_to` then writes a **partial** index. | **Refuse** when `state.link().is_some()`. Also refuse `core.splitIndex=true`. |
| Sparse index | `sparse` ext is written, `Mode::DIR` entries kept. | **Refuse** (`state.is_sparse()`). |
| `SKIP_WORKTREE` entries | Preserved (`FlagsExtended`, `entry/flags.rs:136-151`). | Refuse to *stage* such a path; leave others. |
| Unknown mandatory extension | `from_bytes` errors (`extension/decode.rs:20,63,67`, `MandatoryUnimplemented`). | Already a loud failure. |

### Order of operations (the lost-update window)

`File::write` locks inside itself after the caller has already read
(`file/write.rs:67-71`). Git locks first. So, with `gix-lock` pinned directly:

```rust
let mut lock = gix_lock::File::acquire_to_update_resource(
    git_dir.join("index"), gix_lock::acquire::Fail::Immediately, None)?; // index.lock, O_EXCL
let bytes = std::fs::read(index_path)?;                                  // read UNDER the lock
let mtime = filetime::FileTime::from_last_modification_time(&std::fs::metadata(index_path)?);
let (mut state, _) = gix_index::State::from_bytes(&bytes, mtime, Kind::Sha1, Default::default())?;
refuse_if_unsupported(&state)?;                // link, sparse, stage != 0
// ... mutate entries ...
state.remove_tree();
smudge_racy_untouched_entries(&mut state, mtime, work_dir)?;
let file = gix_index::File::from_state(state, index_path);   // file/init.rs:109
let mut w = std::io::BufWriter::with_capacity(64 * 1024, lock);
file.write_to(&mut w, gix_index::write::Options { extensions: Extensions::All, skip_hash: false })?; // appends the trailing hash
w.into_inner()?.commit()?;                     // rename index.lock -> index
```

`State::write_to` alone does not append the trailer checksum; `File::write_to`
does (`file/write.rs:19-35`). Use the file-level call.

Note `open_index` today passes `FileTime::zero()` because we never write back
(`repo.rs:1105`). The write path must pass the real index mtime; the racy
step below needs it.

### Entry stat from the filesystem

```rust
let file = std::fs::File::open(path)?;                           // open once...
let meta = gix_index::fs::Metadata::from_file(&file)?;           // ...fstat the fd (fs.rs:30)
let stat = gix_index::entry::Stat::from_fs(&meta)?;              // entry/stat.rs:86
let mut bytes = Vec::new(); file.take(MAX).read_to_end(&mut bytes)?;  // ...then read the same fd
```

Stat first, then read, from one descriptor: a concurrent write then yields a
stat that does not match the content, which git treats as modified. `Stat::from_fs`
truncates size, device and inode to 32 bits as git does. Mode: `S_IXUSR`
(owner exec bit) selects `FILE_EXECUTABLE`, as git does; `core.filemode=false`
means new entries are `100644` and existing ones keep their index mode. Not probed.

### Racy-git on write

`gix-index` has no write-side handling. `grep -rin 'racy\|smudg' src` finds
only `Stat::is_racy` (`entry/stat.rs:11-30`) and a warning in
`access/mod.rs:31`. Git's rule (smudge when writing): an entry whose mtime is
not older than the **old** index file's mtime was never verified; once we write
a newer index, it would look clean while the file may have changed in the same
timestamp tick. The conservative fix:

```rust
for e in untouched_entries(&mut state) {
    if e.stat.is_racy(old_index_mtime, stat::Options { check_stat: true, use_nsec: true, ..Default::default() }) {
        e.stat.size = 0;          // git's smudge value: forces a content compare next time
    }
}
```

Git smudges only when content differs while stat matches; zeroing every racy
untouched entry is a superset, safe and cheap. **Not probed against git's
`read-cache.c`; prove with a test that runs `git status` and `git diff-files`
after our write on a fixture whose files share the index's mtime.**

### Hand-written parts that need git-comparison tests

- `git add` replaces file-with-directory conflicts (`a` vs `a/b`) in both
  directions. `State::dangerously_push_entry` (`access/mod.rs:553`) performs no
  such check; call `sort_entries()` afterwards.
- Deleting a tracked path from the worktree stages a removal on `add`; use
  `remove_entries` (`:605`) or set `Flags::REMOVE` (`:23` in `flags.rs`).
- Ignored paths: `git add` refuses an explicitly named ignored path. Reuse the
  `gix-worktree` ignore `Stack` the `status` verb builds.
- Gitlink / nested repo: a directory containing `.git` makes git add a
  gitlink (with a warning). We refuse instead.

---

## 4. Ref updates

**Verdict: feasible; two things to hand-write (HEAD reflog on `switch -c`, newline check).**

### The transaction

`file::Store::transaction()` (`gix-ref-0.66.0/src/store/file/transaction/mod.rs:70`),
`.prepare(edits, ref_lock_fail, packed_lock_fail)` (`prepare.rs:232`), `.commit(committer)`
(`commit.rs:28`). `prepare` takes every lock; failure rolls back; `commit` is
**not** atomic across refs (`commit.rs:10-27`).

- **CAS.** `Change::Update { expected: PreviousValue::MustExistAndMatch(Target::Object(old)), .. }`
  compares under the held lock (`prepare.rs:168-180`) and returns
  `Error::ReferenceOutOfDate { expected, actual }`. For a new ref:
  `PreviousValue::MustNotExist` (`:157-166`). **Weakness:** `MustNotExist` passes
  when the existing ref already has the same target (`if existing.target != *new`,
  `:158`), so "create branch X at oid" succeeds silently on an existing branch
  at that oid. Check existence ourselves first (branch create must say
  "already exists", as git does). Also `read_existing_ref` maps a **corrupt**
  loose ref to "absent" (`:37`), so `MustNotExist` can overwrite a ref file
  that fails to parse; detect with `Store::try_find` first.
- **Lock files.** `gix_lock::File::acquire_to_update_resource(<ref>.lock)`, created
  with `tempfile::Builder::rand_bytes(0)` (`gix-tempfile-24.0.0/src/handle.rs:49`),
  an exclusive create. A pre-existing `.lock` returns
  `acquire::Error::PermanentlyLocked { resource_path, attempts }` for
  `Fail::Immediately` too (`gix-lock-24.0.0/src/acquire.rs:200-208`), and the
  message says "might need manual deletion" (`:53-57`). **Never delete a stale
  lock automatically**: staleness cannot be told from a concurrent git, and
  git does not either. Surface it as exit 1 with the lock path named.
- **`signals`.** Not needed. `gix-tempfile`'s `signals` feature is off in our
  graph (`cargo tree -e features -i gix-tempfile` shows only `default`/`hp-hashmap`;
  `signal-hook` is absent). Drop-based cleanup covers errors and unwinding;
  SIGKILL/SIGINT leave `index.lock` or `<ref>.lock` behind (documented
  limitation, `gix-tempfile-24.0.0/src/lib.rs:20-26`). Do not enable it: it
  installs process-global signal handlers, which an embedded library in
  kaijutsu/kaibo must not do.
- **Committer.** `commit(committer)` needs `Some(SignatureRef)` whenever the
  reflog is written, else it fails **late** with `MissingCommitter`
  (`reflog.rs:153`), possibly after earlier edits landed. Always pass `Some`.
  Time source: E.6 (`GIT_COMMITTER_DATE`, else system clock, offset `+0000`).
- **Order inside `commit`.** reflog append first, then ref rename
  (`commit.rs:43-77`, comment "reflog first, then reference"). A failure
  between them leaves a reflog line for a ref that did not move.
- **Reflog policy.** `Store.write_reflog` is a public field (`store/file/mod.rs:24`).
  `ReadRepo` builds its store with `WriteReflog::Disable` (`repo.rs:467`). `WriteRepo`
  needs its own `Store` built with `Normal`. `Normal` autocreates logs for
  `refs/heads/`, `refs/remotes/`, `refs/notes/`, `refs/worktree/` and `HEAD`
  (`reflog.rs:174-180`) regardless of bare-ness; git's `core.logAllRefUpdates`
  default is "true unless bare". Read the key; map `true` to `Normal`,
  `always` to `Always`, and treat `false` (or unset in a bare repo) as
  unsupported, because `Disable` would also stop appending to logs that exist.

### Deref and the double reflog

An edit on `HEAD` with `deref: true` is split by `extend_with_splits_of_symbolic_refs`
(`transaction/ext.rs:56-132`): the HEAD edit becomes `RefLog::Only` with
`expected: Any`, and a second edit on the referent branch carries the original
`expected` and `RefLog::AndReference`. The result is one lock per ref and one
reflog line in each of `logs/HEAD` and `logs/refs/heads/<b>`, previous oid
carried to HEAD by `leaf_referent_previous_oid` (`prepare.rs:417-428`).

```rust
let committer = signature_ref(&ident);                     // SignatureRef; never None
let msg = reflog_message("commit (initial)", subject)?;    // rejects \n and \r
store.transaction()
    .prepare(
        [RefEdit {
            name: FullName::try_from("HEAD")?,
            deref: true,
            change: Change::Update {
                log: LogChange { mode: RefLog::AndReference, force_create_reflog: false, message: msg },
                expected: match old_tip {
                    Some(oid) => PreviousValue::MustExistAndMatch(Target::Object(oid)),
                    None      => PreviousValue::MustNotExist,        // unborn branch
                },
                new: Target::Object(new_commit),
            },
        }],
        Fail::Immediately, Fail::Immediately,
    )?
    .commit(Some(committer))?;
```

Unborn branch: `HEAD` resolves to `refs/heads/main`, which does not exist;
the leaf edit sees `existing = None` and `MustNotExist` passes. Verify with a
test that the reflog's first line has the null old oid, as `repo.rs:545-575`
treats that state as `Head { oid: None }`.

### Reflog messages git writes **[probed, git 2.56.0]**

| Operation | `logs/HEAD` | `logs/refs/heads/<b>` |
|---|---|---|
| first commit | `commit (initial): <subject>` | same |
| commit | `commit: <subject>` (first line only) | same |
| `commit --amend` | `commit (amend): <subject>` | same |
| `branch x HEAD` | none | `branch: Created from HEAD` |
| `branch x main` | none | `branch: Created from main` |
| `branch x` (implicit) | none | `branch: Created from <current branch name>` |
| `switch -c x` | `checkout: moving from <old> to x` | `branch: Created from HEAD` |

Note `branch x` with no start point logs the current branch's name, not `HEAD`.
(Observed: `branch: Created from topic`.) Match the form the user typed.

### `switch -c`

`switch -c x` from `HEAD` is a ref creation plus a HEAD retarget; no worktree or
index change (same commit). Two edits: create `refs/heads/x`
(`MustNotExist`, message `branch: Created from HEAD`), then
`HEAD -> refs/heads/x` as `Target::Symbolic` with
`expected: MustExistAndMatch(Target::Symbolic(old_branch))`. **gix writes no
reflog line for a symbolic-ref update** ("Special HACK", `commit.rs:53-66`),
so `checkout: moving from <old> to x` in `logs/HEAD` must be appended by hand.
It matters: `@{-1}` and `git switch -` read exactly those lines. Hand-appending
means reproducing the line format (`<old> <new> <name> <<email>> <secs> <tz>\t<msg>\n`,
`reflog.rs:154-164`) under the `HEAD.lock` we hold.

### packed-refs

Default `PackedRefs::DeletionsOnly` (`transaction/mod.rs:17`): updates and
creations write loose files only, as git does. If `packed-refs` exists the
transaction loads it for the CAS lookup and takes `packed-refs.lock`
transiently (`prepare.rs:279-350`); with no packed edits, `commit` is a no-op
(`store/packed/transaction.rs:159`), so `packed-refs` is not rewritten. A
concurrent `git pack-refs` in that window gets a lock error, not corruption.

---

## 5. "Unpublished" reachability for amend

**Verdict: feasible with `reach.rs`; fail closed on cost.**

Question: is `HEAD` an ancestor of (or equal to) the tip of any ref under
`refs/remotes/`?

`reach::Reaches::to(head).from(repo, &mut budget, tip)` (`reach.rs:87`, `:115`)
answers "does `tip` reach `head`", memoized across tips, iterative, and
metered by `Budget` at `MAX_ANCESTRY_COMMITS = 100_000` (`:38`). Exhaustion is
`GitError::AncestryBudgetExhausted`, a refusal, not a truncated "no" (`:21-26`).
For an amend policy that is the right direction: **cannot prove unpublished
means refuse.** `refuse_shallow` (`:262`) already refuses on a shallow repo.

Costs and caveats:

- A "no" (unpublished) answer walks the **entire** history behind every remote
  tip: `Reaches::from` does not stop on the first hit and does not use
  generation numbers. A large repository exhausts the budget on the answer
  we want most. If amend is used on big repositories, an early-exit variant
  and a `gix-commitgraph` generation cutoff (`Option<&Graph>` is already
  first-class, A.4) are the follow-ups.
- Enumerating tips: `repo.refs().iter()` / `branch.rs` already lists
  `refs/remotes/` (`verbs/branch.rs:41`). Skip symbolic `refs/remotes/*/HEAD`
  (it duplicates a tip).
- "Not published" means "not in any remote-tracking ref **this clone has
  fetched or pushed**". A push from another clone is invisible. Name the
  policy accordingly in the tool description ("not known to be published").
- Type coupling: `Budget::parents` and `Reaches::from` take `&ReadRepo`
  (`reach.rs:61`, `:115`). A `WriteRepo` that wraps a `ReadRepo` passes
  `write.read()` and needs no change to `reach.rs`.

---

## 6. Detecting what we must refuse

**Verdict: all checkable from bytes we already open, except user-level config.**

Everything below keys on repo-local evidence. `config_values` reads only
`<common_dir>/config` (`repo.rs:1213-1238`); includes are refused. The refusal
set below is therefore a **floor**, not a proof that real git would behave the
same (surprise 9).

| Hazard | How to detect | Where to look |
|---|---|---|
| Hooks | any non-`.sample` file named `pre-commit`, `prepare-commit-msg`, `commit-msg`, `post-commit`, `post-rewrite` (amend), `post-checkout` (`switch`), `reference-transaction` (git runs it for every ref update); or `core.hooksPath` set (any value) | `<common_dir>/hooks/` (shared by linked worktrees), config. The existing hostile-repo fixture proves real git fires a `core.hooksPath` pre-commit. |
| Signing | `commit.gpgsign=true`, `tag.gpgSign=true`, `tag.forceSignAnnotated=true`; `gpg.format` set | config; refuse, since `gix-command` is forbidden |
| Clean filter | `filter.<name>.clean` / `.process` in config *and* a `.gitattributes` mapping a path to that driver | config + attributes |
| EOL / text conversion | `core.autocrlf` not `false`/unset; `core.eol`; attributes `text`, `eol`, `crlf`, `ident`, `working-tree-encoding` | config + attributes |
| Merge / rebase / cherry-pick / revert in progress | any of `MERGE_HEAD`, `CHERRY_PICK_HEAD`, `REVERT_HEAD`, `REBASE_HEAD`, `rebase-merge/`, `rebase-apply/`, `sequencer/`, `BISECT_LOG`, `SQUASH_MSG`, `MERGE_MSG` in the **per-worktree** `git_dir`; or any index entry with stage != 0 | `ReadRepo::git_dir()` |
| Detached HEAD | `repo.head()?.detached` (`repo.rs:545-598`) | refuse `commit`; allow `switch -c` |
| Unborn branch | `Head.oid == None` | supported: initial commit, no parent |
| Linked worktree | `git_dir != common_dir` (`repo.rs:471-481`) | supported; HEAD, `index`, `logs/HEAD` live in `git_dir`, refs/objects in `common_dir` |
| Shallow | `repo.is_shallow()` (`repo.rs:521`) | commit allowed; amend policy refused (section 5) |
| SHA-256 | `check_object_format` (`repo.rs:1549`) already refuses | no change |
| Sparse / split index, `core.sparseCheckout` | `state.is_sparse()`, `state.link()`, config | refuse |
| `core.symlinks=false`, `core.ignorecase=true`, `core.fsmonitor`, `core.untrackedCache` | config | refuse the first two; the others only add extensions we drop |
| `i18n.commitEncoding` set | config | refuse (we would owe an `encoding` header and a transcode) |
| Commit message cleanup | `commit.cleanup`, `commit.template` | implement git's `whitespace` cleanup for `-m` (strip leading/trailing blank lines and trailing spaces, collapse blank-line runs, end with `\n`); refuse a non-default `commit.cleanup` |

### Attributes: a no-op proof beats a blanket refusal

`* text=auto` is the GitHub default; refusing every repo that has it would
make the profile useless. The conversions git applies on `add` are identities
on bytes that contain no CR and no `$Id`: CRLF to LF can only change content
that has `\r`; `ident` only content with `$Id`. **[probed]**: with
`.gitattributes` `* text=auto`, an `x\r\ny\r\n` file and an `x\ny\n` file get the
same blob id (`b77b4eb`, both through `git add` and `git hash-object`), so
raw-byte staging of the CRLF file would have produced a **different, wrong**
blob. So the rule to implement is: for each staged path, if it has any
applicable `text`/`eol`/`crlf`/`ident`/`working-tree-encoding`/`filter` attribute
(or `core.autocrlf` is on) then stage only if the bytes contain no `\r` and
no `$Id` and no `filter` driver applies; otherwise refuse that path with the
reason. This needs a per-path attribute lookup. `gix-attributes` is **not in
`Cargo.lock`** (`gix-worktree` is pinned `default-features = false`, `Cargo.toml`),
so using it adds a new crate; whether it links `gix-command` is **not verified**
(not on disk). The alternative that adds nothing: scan `.gitattributes` files
from the root down to the path's directory plus `info/attributes` for the
tokens above with a small matcher built on `gix-glob`, which is already
pinned. Either way, user-level `core.attributesFile` and
`~/.config/git/attributes` are invisible.

---

## 7. D.1 grep invariant, D.4 fingerprint test, and `WriteRepo`

**Verdict: viable, after widening the scan.**

### `WriteRepo` on top of today's `repo.rs`

`ReadRepo`'s fields are private, but every piece a writer needs has a
`pub(crate)` accessor already: `objects()` (`:666`), `common_dir()` (`:671`),
`work_dir()` (`:680`), `ceiling()` (`:687`), `git_dir()` (`:495`), `open_index()`
(`:1090`), `contained_leaf()` (`:1194`). So:

```rust
// verbs/write/repo.rs
pub(crate) struct WriteRepo {
    read: ReadRepo,                 // discover() is unchanged; reach.rs, head(), resolve_* reused
    refs: gix_ref::file::Store,     // same constructor as repo.rs:471-481, write_reflog = Normal
    _token: WriteToken,             // C.3's acknowledgement; ledger token when the API has one
}
impl WriteRepo {
    pub(crate) fn open(read: ReadRepo, token: WriteToken) -> Self { /* build refs store */ }
    pub(crate) fn read(&self) -> &ReadRepo { &self.read }
}
```

`ReadRepo` keeps `WriteReflog::Disable` and its `Store`, so a read verb that
somehow called `.transaction()` would write no reflog but would still take
locks and move refs. The scan below catches the call, not the capability.
Stronger and cheap: make `ReadRepo::refs()` and `objects()` return thin
borrowed newtypes (`RefsRead<'_>`, `OdbRead<'_>`) that expose `find`,
`try_find`, `iter`, `exists` and no `transaction`/`Write`. Then "read code
cannot write" is a type fact and the scan is a second line.

**Containment before each write (surprise 1).** `discover` containment covers
what *this crate* opens; gix opens loose paths by name and follows symlinks
(`repo.rs:386-453`, comment). For writes, before each object write, `lstat`
`objects/<xx>` (symlink means refuse); before the ref transaction, `lstat`
every existing directory component of `refs/<...>` and the `logs/<...>` leaf.
Ideally `openat2(RESOLVE_BENEATH)` (kaish #276); until then this is the same
TOCTOU-at-parity position the read path states (`architecture.md` E.2).

### D.1 scan: holes

`tests/write_shaped_identifiers.rs:21-46` matches literal substrings on
comment-stripped source. What it misses today, each demonstrable by writing the
call in `repo.rs` and watching the test stay green:

| Missed spelling | Why |
|---|---|
| `use gix_object::{FindExt, Write};` | needle is the literal `gix_object::Write` |
| `handle.write_buf(..)`, `.write_stream(` | method calls through an already-imported trait |
| `gix_lock::`, `acquire_to_update_resource` | not listed |
| `.persist(`, `.commit(` on a lock | not listed (`.commit(` appears in tests' own prose, so gate on `gix_lock` instead) |
| `std::fs::write`, `fs::rename`, `fs::remove_file`, `OpenOptions`, `create_dir` | the primitives a hand-written reflog append or fsync would use |
| `Store::at(` with a non-`Disable` option | a second ref store |
| `//` inside a string literal (`"http://..."`) | `strip_line_comments` truncates the line and can hide code after it |

Add those needles. Raise `files.len() >= 21` (`:96`) with the new modules.
Keep `WRITE_ALLOWED = ["verbs/write"]` and the test that pins it (`:161-170`).
Add **two controls**: one planted source per new needle (as
`the_scan_detects_a_write_shaped_identifier` does, `:133`), and an assertion
that `src/verbs/write/` *does* contain at least one needle when the feature
exists, so a scan that stopped seeing that directory fails loud.

### D.4 fingerprint: two tests, not one

1. **Read profile stays read-only with `--features commit` compiled in.**
   Run the existing `readonly_fingerprint` under `--features commit` (CI
   matrix, as the tripwires already run with `textdiff` on and off). Add an
   assertion that `GitConfig::read_only()` publishes exactly `Verb::ALL`'s
   read verbs, so the feature adding verbs to the schema is a failure. The
   fingerprint covers the scratch root, which is where a stray `*.lock` or
   leftover `.tmp*` object would show.
2. **A write-effect test with an exact allowlist.** After a commit, the set of
   changed paths under the scratch root must equal exactly: new
   `objects/xx/*` files, `index`, `refs/heads/<b>`, `logs/HEAD`,
   `logs/refs/heads/<b>`, and nothing else; **no** `*.lock`, no `.tmp*` in
   `objects/`, no worktree file touched. Run it for success, and for *each
   refusal* (hook present, gpgsign, merge in progress, stale `index.lock`,
   CAS loss) with the expectation "fingerprint unchanged"; a refusal that
   half-wrote is the failure this exists to catch.
   Add the cross-checks the read tests cannot do: `git fsck --strict`
   exits 0, `git cat-file -p HEAD` equals the commit we meant (including author
   and committer lines), `git status --porcelain` is empty after
   `add && commit`, and `git log --format=%B` shows the message byte-for-byte.
   Per the reviewer-claims rule, run real git in a throwaway repo for every
   "git does X" assertion rather than reasoning from this note.

---

## 8. wasm

**Verdict: native-only; matches A.3.**

The write path is `std::fs` end to end: `tempfile::NamedTempFile` (loose
objects, `write.rs:168-177`), `gix-tempfile`/`gix-lock` (`tempfile::Builder`,
`gix-tempfile-24.0.0/src/handle.rs:49`), `rustix::fs::lstat` (unix `fs.rs`),
`std::fs::OpenOptions::append` (reflog). The plumbing set compiles for
`wasm32-unknown-unknown` (`gix-plumbing-feasibility-2026-08.md` section 6.2:
`gix-lock`, `gix-tempfile` build and "degrade at runtime"), but runtime
behavior on wasm is an error per write, which is the silent-degradation shape
A.3 rejects. `lib.rs:89-94` makes the whole crate a `compile_error!` on any
wasm target, so `commit` inherits that and needs nothing.

Two documentation drifts found while checking:

- `architecture.md` A.3 quotes a guard with an
  `unsupported-wasm-loose-objects-only` escape feature. `lib.rs:89` is an
  unconditional `compile_error!`, and `Cargo.toml` declares only `default` and
  `textdiff`. A.3's open note about renaming that feature is moot.
- `architecture.md` A.2's feature table lists a default `read` feature; the
  manifest has none.

Recommendation: when the guard narrows for a VFS build, keep the write path
native-only with a second, specific message
(`#[cfg(all(feature = "commit", target_family = "wasm"))] compile_error!`), and
record that a VFS-backed write needs a reimplementation of `gix-lock`
atomicity (feasibility section 7 counterweight 2).

---

## Risk register (what to test first)

| Risk | Likelihood | Blast radius | First test |
|---|---|---|---|
| Symlinked `objects/xx`, `refs/heads/d`, `logs/HEAD` writes outside the mount | medium (attacker repo) | arbitrary file create/append | planted-symlink fixture, assert nothing written outside; **must exist before the first write verb** |
| Raw-byte staging diverges from git's blob (CRLF, `$Id`, filters) | high on real repos | silently wrong content | blob-id comparison with `git hash-object --path` over a fixture matrix |
| Index lost update (read before lock) | medium | lost staged changes | two writers racing on `index.lock` |
| Stale racy entry after our write | low-medium | `git status` misses an edit | file edited in the same tick as the old index mtime, then `git status` |
| Split/sparse/`link` index overwritten | low | corrupt index | fixture with `git config core.splitIndex true && git update-index --split-index` |
| Reflog newline forgery | low | forged history | message with `\n` is refused (exit 1/2) |
| Unsorted tree written (release build) | low | invalid tree, `fsck` fails | `git fsck --strict` over a path set with `a`, `a.b`, `a/x`, `a-b` |
| Amend "unpublished" check exhausts the budget on big repos | high on big repos | amend unusable (fails closed) | `KAISH_GIT_BIG_REPO` run |
| Crash between reflog append and ref rename | low | log line without move | document; not testable without fault injection |
| Hooks / signing / filters from user-level config | high | policy bypass the repo did not choose | state in the tool description; cannot be detected hermetically |

## What to decide before building

1. Edit B.11 and C.3 on purpose: `switch -c` and amend leave the "Never" list.
2. Open the kaish PR for an approval API on `ToolCtx`, or accept an
   embedder-acknowledgement-only gate for the first `commit` release.
3. Choose the attribute strategy (new `gix-attributes` pin vs hand matcher on
   `gix-glob`) and verify `gix-attributes` links no spawn crate before pinning.
4. Decide fsync: none (matches the pinned crates) or post-write `sync_all`.
5. Widen the D.1 scan and add the containment step before the first write
   verb lands, not after.
