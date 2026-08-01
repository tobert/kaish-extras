# gix plumbing feasibility for Path 2 — source-read verification (2026-08-01)

> Produced by a **Claude Opus source-reading agent** on **2026-08-01**, at the
> request of [architecture.md](architecture.md) "Co-architect note 2 — the
> gix-command finding and the facade-vs-plumbing fork". Every claim below is
> cited to the **actual pinned sources** on disk under
> `~/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/`, not to training
> data or docs.rs. Version-specific facts drift; re-verify on any pin bump.
>
> Crates read: `gix-object 0.63.0`, `gix-odb 0.83.0`, `gix-ref 0.66.0`,
> `gix-traverse 0.60.0`, `gix-revision 0.48.0`, `gix-revwalk 0.34.0`,
> `gix-commitgraph 0.38.0`, `gix-discover 0.54.0`, `gix-index 0.54.0`,
> `gix-pack 0.73.0`, `gix-diff 0.66.0`, plus `gix-config 0.59.0` (added — see
> §6.1) and the facade `gix 0.85.0` for comparison.
>
> Compile checks in §3.4 and §6.2 were run for real against
> `wasm32-unknown-unknown` on this machine.

---

## Verdict summary

| # | Question | Verdict | One-line reason |
|---|---|---|---|
| 1 | Object access abstractable behind `gix_object::Find`? | ✅ **Yes, cleanly** | `Find`/`Exists`/`FindHeader` are three tiny object-safe traits with an opaque boxed error; `gix-odb` is *one* implementor, not a requirement. |
| 2 | Are traverse / revision / revwalk generic over object source? | ✅ **Yes, fully** | Every entry point is bounded on `gix_object::Find`; **none of the three crates depend on `gix-odb` at all**. |
| 3 | Can gix-pack decode from caller-supplied bytes (no mmap)? | ✅ **Yes — already generic in 0.73.0** | `data::File<T>`, `index::File<T>`, `multi_index::File<T>` are generic over `T: Deref<Target=[u8]>` with public `from_data()`. **The prior research's "mmap blocker" is stale.** |
| 4a | gix-index coupling to `std::fs` | ✅ **Trivially decoupled, read *and* write** | `State::from_bytes()` / `State::write_to(impl io::Write)` are both public and byte-only. |
| 4b | gix-ref coupling to `std::fs` | ⚠️ **Read: parsers exposed, store is not. Write: hard-wired.** | `packed::Buffer::from_bytes()`, `loose::Reference::try_from_path(bytes)`, `log::iter::forward(bytes)` are public — but `file::Store` is `PathBuf`-typed, `ref_contents()` is `pub(crate)`, iteration is `walkdir`, transactions are `gix-lock`. |
| 4c | gix-discover coupling to `std::fs` | ❌ **Total — but the crate is small and droppable** | Pure `std::fs` metadata walking; ~250 lines of logic to reimplement over VFS, and dropping it removes the `gix-sec` wasm blocker. |
| 5 | Effort tier | **Path 2-native ≈ M. Path 2 + VFS ≈ L. Path 2 + VFS + wasm ≈ L (same work).** | See §5 for the per-capability table. |

**The design doc's conditional resolves in favour of Path 2.** Its own test was:
*"If the readers are generic over object storage and refs/index are tractable,
Path 2 is the more-kaish bet."* The readers are generic (§2, unambiguously),
gix-index is tractable to the point of being free (§4a), and gix-ref's **read**
path is tractable via exposed byte parsers (§4b). Only ref **writes** are
genuinely hard-wired — and the read profile does not write refs.

### The two decisive findings

1. **gix-pack 0.73.0 is already mmap-optional.** `pub struct File<T = MMap>`
   with `pub fn from_data(data: T, path: PathBuf, …)` where
   `T: Deref<Target = [u8]>` — for pack data, pack index, *and* multi-pack-index
   (§3.1). `gix_pack::Bundle::find()`, the only thing that stitches them
   together and is `MMap`-typed, is **69 lines** and trivially reimplemented
   (§3.3). The upstream ask in the prior research and in appendix issue #12
   ("gix-pack: read-into-memory fallback") **is already satisfied by the
   plumbing API** — it is only the *facade* and `gix-odb::Store` that go
   straight to mmap.

2. **The plumbing set compiles clean for `wasm32-unknown-unknown` today**
   (§6.2) — including `gix-ref`, `gix-index`, `gix-lock`, `gix-tempfile`,
   `memmap2` and `gix-imara-diff`. The *only* compile blocker is `gix-sec`,
   and it is reachable **only** through `gix-discover` and `gix-config`. Drop
   `gix-discover` (which VFS-backed discovery replaces anyway) and the browser
   build is one small upstream `gix-sec` cfg fix away — not the multi-crate
   rewrite the prior research implied.

### Corrections to prior documents

- **`gix-research-2026-08.md` §1 "Virtual filesystem: not possible"** (citing
  Byron's Discussion #1150 "No, that's not possible yet") is **true of the
  `gix` facade and false of the plumbing**. `gix-object`'s find traits,
  `gix-pack`'s `from_data`, `gix-index`'s `State::from_bytes`/`write_to`,
  `gix-ref`'s byte parsers and `gix-config`'s `from_bytes_owned` are a
  coherent, deliberate byte-level seam. Byron's answer is about
  `gix::Repository`, which is the thing Path 2 replaces.
- **`gix-research-2026-08.md` §3(a) mitigation (ii)** — *"drop to `gix-diff`'s
  `tree_with_rewrites` without the `blob` feature for name/status-only diff"* —
  **is wrong**. `tree_with_rewrites`, `rewrites` (the whole rename tracker) and
  `Rewrites` are all `#[cfg(feature = "blob")]`
  (`gix-diff-0.66.0/src/lib.rs:20,48,56-59`). Without `blob` you get only the
  low-level `gix_diff::tree()` + `tree::Visit` delegate. **Rename detection is
  not available in a no-`blob` build at any level** (§5.4).
- **`gix-research-2026-08.md` §1 "The runtime blocker: mmap"** is correct about
  `gix_pack::mmap::read_only` (`gix-pack-0.73.0/src/lib.rs:60-71`) but
  incomplete: that function is only reachable from the `File<MMap>`-specific
  `at()` constructors, which the byte path never calls.

---

## 1. Object access abstraction — `gix_object::Find` et al.

### 1.1 The traits

`gix-object-0.63.0/src/traits/find.rs`, re-exported at
`gix-object-0.63.0/src/lib.rs:71`:

```rust
pub use traits::{Exists, Find, FindExt, FindObjectOrHeader,
                 Header as FindHeader, HeaderExt, Write, WriteTo};
```

Note the rename: the trait is declared `pub trait Header` but is
`gix_object::FindHeader` to consumers.

| Trait | Definition | Required method |
|---|---|---|
| `Exists` | `traits/find.rs:4-7` | `fn exists(&self, id: &gix_hash::oid) -> bool` |
| `Find` | `traits/find.rs:17-24` | `fn try_find<'a>(&self, id: &gix_hash::oid, buffer: &'a mut Vec<u8>) -> Result<Option<gix_object::Data<'a>>, find::Error>` |
| `FindHeader` | `traits/find.rs:27-32` | `fn try_header(&self, id: &gix_hash::oid) -> Result<Option<gix_object::Header>, find::Error>` |
| `FindObjectOrHeader` | `traits/find.rs:35`, blanket at `traits/_impls.rs`… see `traits/find.rs:53` | — (marker: `Find + FindHeader`) |

Each trait has **exactly one required method**. That is the whole contract.

**Buffer discipline.** `try_find` writes the object's *decoded, header-stripped*
bytes into the caller's `&mut Vec<u8>` and returns a `Data<'a>` borrowing it —
`Data { kind, data: &'a [u8], object_hash }`. The lifetime ties the returned
object to the buffer, so a caller cannot hold two objects from one buffer. This
is why traversal code threads `state.buf1`/`state.buf2` around (e.g.
`gix-diff-0.66.0/src/tree/mod.rs:41-44`). A VFS implementation just does
`buffer.clear(); buffer.extend_from_slice(inflated); Ok(Some(Data{..}))`.

**Error type is opaque and forgiving** —
`gix-object-0.63.0/src/find.rs:1-2`:

```rust
pub type Error = Box<dyn std::error::Error + Send + Sync + 'static>;
```

So a kaish-vfs implementation can return *its own* error type with no
conversion ceremony and no upstream change. This is the single most
important ergonomic fact for Path 2.

**`FindExt` is free.** `traits/find.rs:155-230` provides
`find_commit`/`find_tree`/`find_tag`/`find_blob` and the `*_iter` variants
as blanket extension methods over any `Find` (macro-generated at
`traits/find.rs:158-188` and `:190-207`). Implement one method, get the whole
typed API.

**Blanket impls for wrappers** — `traits/find.rs:44-152` covers `&T`, `Box<T>`,
`Rc<T>`, `Arc<T>` for all three traits. So `Arc<KaishVfsOdb>` is a `Find` for
free.

**A worked reference implementation ships in-crate**: `find::Never`
(`gix-object-0.63.0/src/find.rs:65-85`) implements all three traits plus
`Write` in ~20 lines. That is the shape of the exercise.

### 1.2 Is `gix-odb::Store` usable, or do we implement `Find` ourselves?

**Both are available; they are not coupled.**

- `gix_odb::at(objects_dir)` / `at_opts(...)`
  (`gix-odb-0.83.0/src/lib.rs:182,197`) builds the dynamic `Store`
  (`src/lib.rs:137`, options at `src/store_impls/dynamic/init.rs:10-31`). Its
  `Handle` implements `gix_object::Find` at
  `gix-odb-0.83.0/src/store_impls/dynamic/find.rs:510` and `FindHeader` at
  `:524`; the `Cache<S>` wrapper does likewise at `src/cache.rs:177,204`.
- `Store` is **irreducibly path-and-mmap bound**: loose objects are mmapped at
  `src/store_impls/loose/find.rs:230,257-262`; packs come in as
  `gix_pack::Bundle` (the `MMap`-typed struct).
- `gix_odb::memory::Proxy<T>` (`src/memory.rs:17-27`, `Find` at `:139`,
  `FindHeader` at `:191`) wraps *any* inner odb with an in-memory
  write-through layer — useful for a VFS-backed store that needs to stage
  written objects, and proof that odb layering is designed for composition.

**Recommendation:** use `gix-odb::Store` on the native/localfs path (it gives
alternates, multi-pack-index, `.keep` handling, slot-map refresh and prefix
disambiguation for free) and implement `Find`/`Exists`/`FindHeader` directly
over kaish-vfs for the VFS/wasm path. The consumers cannot tell the difference
(§2). Loose-object decode over VFS needs only
`gix_object::decode::loose_header(&[u8])`
(`gix-object-0.63.0/src/lib.rs:354`) plus `gix_zlib::Inflate`.

---

## 2. Reader genericity — traverse, revision, revwalk

**Verdict: unambiguously generic.** The strongest evidence is negative: none of
`gix-traverse`, `gix-revision`, or `gix-revwalk` **has a dependency on
`gix-odb`**. Their full gix-dependency lists (from their `Cargo.toml`s) are:

| Crate | gix deps |
|---|---|
| `gix-traverse 0.60.0` | `gix-commitgraph`, `gix-date`, `gix-hash`, `gix-hashtable`, `gix-object`, `gix-revwalk` (`Cargo.toml:49-64`) |
| `gix-revision 0.48.0` | `gix-commitgraph`, `gix-date`, `gix-error`, `gix-hash`, `gix-hashtable`, `gix-object`, `gix-revwalk`, `gix-trace` (`Cargo.toml:81-103`) |
| `gix-revwalk 0.34.0` | `gix-commitgraph`, `gix-date`, `gix-error`, `gix-hash`, `gix-hashtable`, `gix-object` (`Cargo.toml:46-61`) |

### 2.1 gix-traverse — commit walks

| Entry point | Location | Bound |
|---|---|---|
| `struct Simple<Find, Predicate>` (field `objects: Find`) | `src/commit/mod.rs:10-11` | — |
| `Simple::new(tips, find)` | `src/commit/simple.rs:411-423` | `Find: gix_object::Find` |
| `Simple::filtered(tips, find, predicate)` | `src/commit/simple.rs:428-445` | `Find: gix_object::Find` |
| `impl Iterator for Simple` | `src/commit/simple.rs:483-485` | `Find: gix_object::Find` |
| `struct Topo<Find, Predicate>` (field `find: Find`) | `src/commit/mod.rs:26-28` | — |
| `topo::Builder::new(find)` / `from_iters` | `src/commit/topo/init.rs:20-37` | `Find: gix_object::Find` |
| `Builder::build() -> Topo<Find, Predicate>` | `src/commit/topo/init.rs:69-112` | `Find: gix_object::Find` |
| `impl Iterator for Topo` | `src/commit/topo/iter.rs:220-222` | `Find: gix_object::Find` |
| `commit::find(cache, objects, id, buf)` | `src/commit/mod.rs:101-108` | `Find: gix_object::Find` |
| `find_ancestors`-style helper taking `objects` | `src/commit/simple.rs:176-181, 396` | `&impl gix_object::Find` |

Commit-graph acceleration is **optional and injected**:
`Simple::commit_graph(Option<gix_commitgraph::Graph>)`
(`src/commit/simple.rs:344`) and
`Builder::with_commit_graph(Option<..>)` (`src/commit/topo/init.rs:104`).
Passing `None` is fully supported — which matters, because `gix-commitgraph` is
*not* byte-generic (§3.5).

### 2.2 gix-traverse — tree walks

| Entry point | Location | Bound |
|---|---|---|
| `tree::breadthfirst(root, state, objects, delegate)` | `src/tree/breadthfirst.rs:54-61` | `Find: gix_object::Find` |
| `tree::depthfirst(root, state, objects, delegate)` | `src/tree/depthfirst.rs:40-47` | `Find: gix_object::Find` |
| `tree::Visit` delegate trait | `src/tree/mod.rs:23-42` | — |
| `tree::Recorder` (ready-made delegate) | `src/tree/recorder.rs` | — |

### 2.3 gix-revwalk

`Graph<'find, 'cache, T>` is the shared commit-graph cache used by
`gix-revision`. Its constructor erases the object source into a trait object:

```rust
// gix-revwalk-0.34.0/src/graph/mod.rs:217
pub fn new(objects: impl gix_object::Find + 'find,
           cache: Option<&'cache gix_commitgraph::Graph>) -> Self
```

and the internal lookup takes `objects: &dyn gix_object::Find`
(`src/graph/mod.rs:365`). Nothing else is required.

### 2.4 gix-revision — an important nuance

`gix-revision` **does not take an object source at all**; it takes an
already-constructed `&mut gix_revwalk::Graph`:

- `describe(commit, graph: &mut Graph<'_,'_,Flags>, options)` —
  `src/describe.rs:151-160`
- `merge_base(first, others, graph: &mut Graph<'_,'_,graph::Commit<Flags>>)` —
  `src/merge_base/function.rs:25-29`

So genericity is inherited from `Graph::new`. ✅

**But rev-spec parsing is a different animal.** `gix_revision::spec::parse` is a
**pure parser driving a caller-supplied delegate** — it resolves *nothing*:

- `pub trait Delegate: delegate::Revision + delegate::Navigate + delegate::Kind`
  — `src/spec/parse/mod.rs:13`
- `delegate::Revision` requires `find_ref`, `disambiguate_prefix`, `reflog`,
  `nth_checked_out_branch`, `sibling_branch` — `src/spec/parse/delegate.rs:11-38`
- `delegate::Navigate` requires `traverse`, `peel_until`, `find`, `index_lookup`,
  … — `src/spec/parse/delegate.rs:57+`

This is **not a gap** — it is exactly the seam Path 2 wants (the delegate is
where kaish-vfs refs and objects plug in) — but it means **rev-parse is work we
own, not work we inherit**. For scale: the facade's implementation of this
delegate is `gix-0.85.0/src/revision/spec/parse/delegate/{mod,revision,navigate}.rs`
= **264 + 292 + 396 = 952 lines**. See §5.3.

### 2.5 gix-diff (without `blob`) — also generic

- `gix_diff::tree(lhs, rhs, state, objects, delegate)` —
  `gix-diff-0.66.0/src/tree/function.rs:31-38`, bound
  `objects: impl gix_object::Find`. Exported as `gix_diff::tree` at
  `src/lib.rs:52-53`.
- `tree::Visit` delegate — `src/tree/mod.rs:23-35`.
- `gix_diff::tree_with_rewrites(…, objects: &impl gix_object::FindObjectOrHeader, …)`
  — `src/tree_with_rewrites/function.rs:24-33` — **but this is
  `#[cfg(feature = "blob")]`** (`src/lib.rs:56-59`), and `blob` pulls
  `gix-command` (`Cargo.toml:48-58`). See §5.4.

---

## 3. Pack decoding without mmap — the wasm linchpin

### 3.1 The pack types are already generic over their backing bytes

`gix-pack-0.73.0/src/lib.rs:24-29`:

```rust
/// The default memory-backed storage for pack data and index files.
pub use memmap2::Mmap as MMap;

/// A byte-oriented backing store for pack data and indices.
pub trait FileData: Deref<Target = [u8]> {}
impl<T> FileData for T where T: Deref<Target = [u8]> {}
```

`Vec<u8>`, `Box<[u8]>`, `Arc<[u8]>`, `Cow<'_, [u8]>` and any kaish-vfs buffer
type that derefs to `[u8]` all qualify with **zero** newtype work.

| Type | Declaration | mmap-only ctor | **byte ctor** |
|---|---|---|---|
| pack data | `pub struct File<T = MMap>` — `src/data/mod.rs:86` | `File<MMap>::at(path, hash)` — `src/data/file/init.rs:6-25` | **`File<T>::from_data(data: T, path: PathBuf, object_hash)` — `src/data/file/init.rs:28-57`** |
| pack index (`.idx`) | `pub struct File<T = MMap>` — `src/index/mod.rs:101` | `File<MMap>::at(path, hash)` — `src/index/init.rs:26-42` | **`File<T>::from_data(data, path, object_hash)` — `src/index/init.rs:44-97`** |
| multi-pack-index | `pub struct File<T = MMap>` — `src/multi_index/mod.rs:22` | `File<MMap>::at(path, limit)` — `src/multi_index/init.rs:38-55` | **`File<T>::from_data(data, path, alloc_limit_bytes)` — `src/multi_index/init.rs:57-68`** |

`mmap::read_only` (`src/lib.rs:60-71`) — the function the prior research
identified as the blocker — is called from **exactly three places**, all inside
the `impl File<crate::MMap>` blocks above (`data/file/init.rs:20`,
`index/init.rs:36`, `multi_index/init.rs:49`). The byte path never touches it.

The design intent is explicit in the crate: `source_name()` returns
`"<memory>"` for an empty path (`src/lib.rs:76-84`), and every `from_data` doc
comment says *"as assumed to be read **or** memory-mapped from `path`"*.

### 3.2 Everything downstream of construction is generic too

All object-access and decode operations live in `impl<T> … where T: crate::FileData`
blocks:

- `data::File<T>`: `entry(offset)`, `decompress_entry`, **`decode_entry(entry,
  out, inflate, resolve, delta_cache)`** with full delta-chain resolution —
  `src/data/file/decode/entry.rs:74-76` (impl header) and `:188-215` (signature);
  accessors at `src/data/mod.rs:109-111`.
- `index::File<T>`: `lookup(oid) -> Option<EntryIndex>` (`src/index/access.rs:129`),
  `oid_at_index` (`:80`), `pack_offset_at_index` (`:94`), `lookup_prefix` (`:143`)
  — impl header `src/index/access.rs:30-32`.
- `multi_index::File<T>`: `src/multi_index/access.rs:28,69`.
- Verification and traversal: `src/index/verify.rs:121`,
  `src/index/traverse/mod.rs:61`, `src/data/file/verify.rs:16`.

`decode_entry`'s `resolve` argument is a plain closure
`&dyn Fn(&oid, &mut Vec<u8>) -> Option<ResolvedBase>` — so ref-deltas that point
outside the pack can be satisfied from the VFS-backed odb.

An allocation guard already exists for untrusted packs:
`File::with_alloc_limit_bytes(Option<usize>)` — `src/data/file/init.rs:66-69`
(and it is deliberately *off* for `at()`, *on* by caller choice for
`from_data()` — read the doc comment at `:12-14`).

### 3.3 The only thing that is `MMap`-hardwired is `Bundle`, and it is 69 lines

```rust
// gix-pack-0.73.0/src/lib.rs:34-39
pub struct Bundle {
    pub pack: data::File,     // = data::File<MMap>
    pub index: index::File,   // = index::File<MMap>
}
```

`Bundle::find()` and `get_object_by_index()` — the whole
oid → index-lookup → pack-offset → entry → `decode_entry` pipeline — is
`gix-pack-0.73.0/src/bundle/find.rs:9-68`. **Reimplementing it over
`data::File<Vec<u8>>` + `index::File<Vec<u8>>` is a copy-paste with the type
parameters changed.** That, plus enumerating `objects/pack/*.idx` through
kaish-vfs, is the entire VFS/wasm pack story.

### 3.4 `gix-odb::Store` cannot be reused for this, and does not need to be

`Store` builds `gix_pack::Bundle`s and mmaps loose objects
(`src/store_impls/loose/find.rs:230,257-262`). It cannot be pointed at bytes.
Bypassing it is not a workaround — it is the same move as §1.2: implement
`gix_object::Find` yourself. Nothing above `Find` notices.

### 3.5 The one genuine non-generic holdout: `gix-commitgraph`

```rust
// gix-commitgraph-0.38.0/src/lib.rs:29
    data: memmap2::Mmap,
// gix-commitgraph-0.38.0/src/file/init.rs:20,29
    pub fn at(path: impl AsRef<Path>) -> Result<File, Exn<Message>>
    pub fn new(data: memmap2::Mmap, path: PathBuf) -> Result<File, Exn<Message>>
```

`File` stores a concrete `memmap2::Mmap` and there is no byte constructor.
On wasm, `memmap2` is the stub that returns `Unsupported`, so
`commit-graph` is unavailable there.

**Impact: performance only.** Every consumer takes
`Option<&gix_commitgraph::Graph>` (§2.1), so `None` is a first-class mode. Log
on a large repo will be slower than git's; correctness is unaffected. This is
the right shape for an upstream ask (`File::from_data` mirroring gix-pack's) —
smaller and better-precedented than the gix-pack ask that turned out to be
already done.

---

## 4. File coupling of refs and index

### 4a. `gix-index` — effectively decoupled already ✅

| Direction | API | Location | Coupling |
|---|---|---|---|
| **read from bytes** | `State::from_bytes(data: &[u8], timestamp: FileTime, object_hash, Options) -> Result<(State, Option<ObjectId>), Error>` | `src/decode/mod.rs:67-80` | **none** — pure bytes |
| **write to bytes** | `State::write_to(&self, out: impl std::io::Write, Options) -> Result<Version, _>` | `src/write.rs:61-73` | **none** — any writer |
| **write + checksum** | `File::write_to(&self, out: impl io::Write, Options) -> Result<(Version, ObjectId), _>` | `src/file/write.rs:22-40` | **none** |
| read from path | `File::at(path, hash, skip_hash, opts)` | `src/file/init.rs:53-104` | `std::fs::File::open` + `memmap2` at `:62,65` |
| write to path | `File::write(&mut self, Options)` | `src/file/write.rs:67-81` | `gix_lock::File::acquire_to_update_resource` at `:72` |
| in-memory ctor | `File::from_state(state, path)` | `src/file/init.rs:109-115` | none |

**This is as good as it gets.** A VFS index is `State::from_bytes(vfs.read(path)?, …)`
for reads and `state.write_to(vfs.writer(path)?, …)` for writes. Both
directions, no shims, no upstream change.

Two caveats:

- `FileTime` (from `filetime`) must be synthesised for VFS-backed reads; it is
  pass-through metadata used for stat-based freshness comparisons. Supplying a
  zero/epoch value makes every entry look racily-clean — a **correctness
  concern for `status`**, not for reading the index. Decide deliberately.
- The **split/shared index** (`link` extension) is dissolved at `File` level via
  `link.dissolve_into(...)` (`src/file/init.rs:99-101`), which re-enters
  `File::at`. A VFS implementation must handle the second file itself. Rare;
  detectable; must be an explicit error rather than a silent partial index.

`src/fs.rs` wraps `std::fs::Metadata` for stat comparison
(`src/fs.rs:15,30`) and `entry/mode.rs:20` compares against
`std::fs::symlink_metadata` — relevant only to worktree-facing `status`, not to
index decode/encode.

### 4b. `gix-ref` — read path feedable, store and write path not ⚠️

**The byte-level parsers are all public:**

| What | API | Location |
|---|---|---|
| loose ref file contents | `loose::Reference::try_from_path(name: FullName, path_contents: &[u8], object_hash)` | `src/store/file/loose/reference/decode.rs:48` |
| `packed-refs` whole file | `packed::Buffer::from_bytes(bytes: &[u8], object_hash)` (uses path `"<memory>"`) | `src/store/packed/buffer.rs:99-103` |
| `packed-refs` streaming | `packed::Iter::new(packed: &[u8], object_hash)` | `src/store/packed/iter.rs:72` |
| `packed-refs` header | `packed::decode::header` | `src/store/packed/decode/mod.rs` |
| reflog forward scan | `log::iter::forward(lines: &[u8]) -> Forward<'_>` | `src/store/file/log/iter.rs:58` |
| reflog single line | `log::LineRef::from_bytes(input: &[u8])` | `src/store/file/log/line.rs:118` |
| reflog reverse scan | `log::iter::reverse<F>(log: F, buf)` — generic over `F: io::Read + io::Seek` | `src/store/file/log/iter.rs:125` |
| ref-name validation/types | `FullName`, `PartialName`, `Category`, `Target` | `src/lib.rs:130-219` |
| peeling | `ReferenceExt::peel_to_id_in_place(&mut self, store, objects: &dyn gix_object::Find)` | `src/store/file/raw_ext.rs:29-32` |

Note `peel_to_id_in_place` takes `&dyn gix_object::Find` — the object side is
already abstract; only the `store: &file::Store` argument is not.

**What is *not* feedable:**

| Thing | Location | Why |
|---|---|---|
| `file::Store` itself | `src/store/file/mod.rs:10-41` | fields are `git_dir: PathBuf`, `common_dir: Option<PathBuf>`; no trait, no injection point |
| `Store::ref_contents()` | `src/store/file/find.rs:301-305` (`std::fs::File::open` at `:305`) | **`pub(crate)`** — cannot be overridden or bypassed |
| loose ref iteration | `src/store/file/loose/iter.rs:3,34-36` | `gix_features::fs::walkdir_sorted_new` |
| overlay (loose+packed) iteration | `src/store/file/overlay_iter.rs:91` | `std::fs::File::open` |
| existence probe | `src/store/file/find.rs:342` | `std::fs::metadata` |
| `packed::Buffer::open` (path form) | `src/store/packed/buffer.rs:83-90` | `std::fs::metadata` / `read` / `memmap2` |
| reflog by ref name | `src/store/file/loose/reflog.rs:30-62,115` | `std::fs::File::open` / `OpenOptions` |
| **all transactions** | `src/store/file/transaction/prepare.rs:54-56,86,132,235-249`, `commit.rs:96,128,165` | `gix_lock::{File,Marker}` + `std::fs::remove_file` |
| packed-refs transaction | `src/store/packed/transaction.rs:219` | `std::fs::remove_file` |

**Read-path assessment: tractable, ~M.** A `VfsRefStore` reimplements
(a) name → relative path, (b) loose lookup via `try_from_path`, (c) packed
fallback via `from_bytes`, (d) symref chase with a depth cap, (e) iteration by
VFS directory walk, (f) reflog via `log::iter::forward`. All parsing, all
validation and all name types come from gix-ref. This is composition, not
reimplementation — call it **300-500 lines**, plus the namespace/common-dir
(linked worktree) rules if we support those.

**Write-path assessment: hard-wired, ~L, and out of scope.** Transactions,
`packed-refs` rewriting and reflog appends are built on `gix-lock`'s
`.lock`-file protocol against `std::fs`. Reimplementing that over VFS means
reimplementing the atomicity guarantee, which is the part you least want to
hand-roll. **The read profile (architecture.md phases 1-9) does not write
refs**, so this does not gate the fork — but it does mean the eventual
`commit`/`worktree` profiles either stay localfs-only or take on a real
VFS-locking design.

### 4c. `gix-discover` — total `std::fs` coupling, but droppable ❌→✅

`gix-discover 0.54.0` is pure filesystem probing: `upwards/mod.rs:56-172`
walks parents comparing `metadata()`; `is.rs:31-89` classifies a `git_dir` from
`std::fs::Metadata`; `path.rs:31-33` reads `.git` gitdir-files with a 64 KiB
cap; `upwards/util.rs:67,74` reads device ids for the cross-device ceiling.
There is no byte-level entry point (`from_gitdir_file` takes a path).

Its gix deps are `gix-fs`, `gix-path`, `gix-ref`, `gix-sec`
(`Cargo.toml:54-63`) — **and `gix-sec` is the entire `wasm32-unknown-unknown`
blocker** (§6.2).

Reimplementing discovery over kaish-vfs is roughly 250 lines and — crucially —
it is *the same code kaish needs anyway* to enforce architecture.md §E.2's
mount-ceiling guard. Path 2 gets a stronger ceiling for free: a VFS walk
physically cannot escape a mount, so the ceiling stops being a check and starts
being a structural property.

---

## 5. Effort tiering

Two distinct scopes hide under "Path 2". Size them separately.

- **Path 2-native**: plumbing crates against the real filesystem. Uses
  `gix-odb::Store`, `gix-ref::file::Store`, `gix-index::File::at`,
  `gix-discover` as-is. Delivers the "spawn code not linked" structural
  guarantee **only**.
- **Path 2-VFS**: adds kaish-vfs-backed odb/refs/index/config/discovery.
  Delivers the guarantee **plus** git-over-kaish-VFS **plus** (modulo the
  `gix-sec` fix) the wasm path. The wasm path is not additional work beyond
  VFS — it is the same work.

### 5.1 Per-capability

| Capability | Facade gives | Path 2-native | Path 2-VFS additional | Needs VFS-backed storage? |
|---|---|---|---|---|
| **open / discover repo** | `gix::discover`, `gix::open` (1084 LOC of permissions/env/config cascade) | **S** — `gix-discover` + hand-wire odb/ref-store/config; hermetic-only, so most of the facade's permission surface is deleted, not ported | **S-M** — reimplement upward walk over VFS (~250 LOC); *removes* `gix-discover` and with it `gix-sec` | Read-through only |
| **read refs** (list, resolve, peel, HEAD) | `Repository::references()`, `find_reference`, `head()` (766 + 384 LOC) | **S** — `gix-ref::file::Store` works as-is; `peel_to_id_in_place` already takes `&dyn Find` | **M** — `VfsRefStore` (~300-500 LOC) over gix-ref's public parsers | Read-through only |
| **revwalk / log** | `Repository::rev_walk()` (371 LOC) | **S** — `gix_traverse::commit::{Simple,Topo}` are the same engine the facade wraps | **free** — generic (§2.1) | Read-through only |
| **cat objects** (`show`, `ls`) | `Repository::find_object`, `gix::Object` (2464 LOC incl. tree/blob/commit ergonomics) | **S** — `FindExt::find_{commit,tree,blob}_iter` + `gix_object` decode types | **S** — implement `Find`/`FindHeader` over VFS + `loose_header` + `Inflate` + §3.3's 69-line bundle clone | **Yes** — this is the VFS core |
| **rev-parse** (`HEAD~3`, `main^{tree}`, `rev:path`) | `Repository::rev_parse()` — delegate impl is **952 LOC** (`gix-0.85.0/src/revision/spec/parse/delegate/*`) | **M** if we implement the full `gix_revision::spec::Delegate`; **S** if we hand-roll a restricted grammar (oid / ref / `HEAD` / `^` / `~n` / `^{kind}` / `rev:path`) and never link `gix-revision::spec` | **free** — delegate is ours either way | Read-through only |
| **diff tree↔tree, name/status** | `Repository::diff_tree_to_tree()` with renames | **M** — `gix_diff::tree()` + our own `Visit` → change model; **no rename tracking** (§5.4) | **free** — generic | Read-through only |
| **diff blob↔blob, line hunks** | `blob-diff` feature → `gix_diff::blob` + `unified_diff` printer | **S** — `gix-imara-diff 0.2.4` directly on raw bytes; skips textconv by construction | **free** | No |
| **unified patch rendering** | `gix_diff::blob::unified_diff` — **`#[cfg(feature="blob")]`**, so unavailable to us | **M** — hand-render; already flagged as a gap in the prior research and as PR 6 in architecture.md §H | **free** | No |
| **status** (index + worktree) | `status` feature: `Repository::status()`, rename tracking, submodule recursion, `gix-dir` untracked walk | **L** — hand-composed. `gix-index` gives entries; we own the worktree walk, `.gitignore` (no `gix-ignore`), pathspecs (no `gix-pathspec`), racy-clean stat rules | **L** — same code, VFS walk instead of `std::fs` walk; but see the `FileTime` caveat in §4a | **Yes** (worktree side) |
| **blame** | `blame` feature: `Repository::blame_file()` | **L** — revwalk + per-commit path-limited tree diff + line mapping. Same tier under the facade's own "blame-ish" caveats | **free** beyond the odb | Read-through only |
| **config read** | `gix::config` — 6954 LOC of cascade, includes, env overrides | **S** — `gix-config` (see §6.1); we need `core.repositoryformatversion`, `extensions.objectformat`, `core.bare`, `core.worktree` and little else | **S** — `File::from_bytes_owned` | Read-through only |
| **ref writes / commit / worktree lifecycle** | partial (`commit()` yes, worktree lifecycle no) | **M** on native — `gix-ref` transactions work as-is | **L** — `gix-lock` is `std::fs`-only; needs a VFS atomicity design | Out of read-profile scope |

### 5.2 Roll-up

| Scope | Tier | Notes |
|---|---|---|
| **Path 2-native, read profile** (open, refs, log, cat, diff name/status + line hunks) | **M — 1-2 weeks** | Genuinely close to the facade in effort. The convenience layer we replace is thin *because* the plumbing is well-factored. |
| Path 2-native, + status + blame | **L — multi-week** | `status` and `blame` are L under *either* path: architecture.md already commits to hand-composing them because `gix-status`/`gix-blame` drag in `gix-command`. **Path 1 does not save this work.** |
| **Path 2-VFS, read profile** | **L — multi-week** | Adds VFS odb (S-M), `VfsRefStore` (M), VFS discovery (S-M). Index and config are ~free. |
| Path 2-VFS + wasm | **L — same work** | wasm is a *consequence* of VFS, not an increment, modulo the `gix-sec` cfg fix (§6.2). |
| Path 2-VFS + writes | **L+ and unscoped** | `gix-lock` over VFS is a real design problem. Defer with the ledger. |

### 5.3 The rev-parse decision is the biggest single lever

952 lines of facade delegate is the largest chunk of "convenience layer" Path 2
does not inherit. Two honest options:

1. Implement `gix_revision::spec::Delegate` fully — full git revspec fidelity,
   ~M, and we own the reflog/`@{upstream}`/`@{n}` semantics.
2. Hand-roll a documented subset and **do not link `gix-revision::spec` at
   all** — ~S, and an agent-facing tool arguably *should* have a small, stated
   revspec grammar rather than git's baroque one. Unsupported syntax exits with
   a specific error, never a wrong answer.

Recommend (2) for the read profile, with (1) as a later PR if demand appears.
Either way `gix-revision`'s `describe` and `merge_base` remain available
(§2.4) — they are independent of `spec`.

### 5.4 The rename-tracking hole is real and must be a stated limitation

`Rewrites`, `rewrites::Tracker` and `tree_with_rewrites` are all
`#[cfg(feature = "blob")]` (`gix-diff-0.66.0/src/lib.rs:20,48,56-59`), and
`rewrites/tracker.rs` is written against `crate::blob::Platform` throughout
(`:18,98,223,376,427,707,782,825`) — the similarity computation *is* the blob
platform. `blob` pulls `gix-command` (`Cargo.toml:48-58`).

So under Path 2 the options are: (a) no rename detection; (b) exact-match-only
rename detection (same blob oid, different path) — cheap, correct, ~50 LOC, and
covers pure renames; (c) reimplement similarity scoring on `gix-imara-diff` —
L, and divergence from git is guaranteed.

**Recommend (b), reported honestly in the payload** — architecture.md already
has the vocabulary for this (`follows_renames: false`). Note that architecture.md
§H PR 2 lists a "rename fixture" for `status`; that fixture's expectations need
revising under Path 2.

---

## 6. Two findings outside the five questions

### 6.1 The probe crate set is missing `gix-config` — and it is spawn-free

Path 2 cannot open a repository correctly without reading `.git/config`:
`core.repositoryformatversion`, `extensions.objectformat` (sha256 repos!),
`core.bare`, `core.worktree`, `core.precomposeUnicode`. `gix-config` was not in
the probe set.

Verified on this machine: **`gix-config 0.59.0`** resolves cleanly alongside
`gix-ref 0.66` / `gix-object 0.63` with **no duplicate crates**
(`cargo tree -d` → "nothing to print") and **pulls no `gix-command`**
(`cargo tree -i gix-command` → "did not match any packages").

(Do **not** use `gix-config 0.51` — it resolves an entire second-generation
graph: `gix-ref 0.58`, `gix-object 0.55`, `gix-hash 0.22`.)

It is also VFS-friendly: `File::from_bytes_no_includes`
(`gix-config-0.59.0/src/file/init/mod.rs:35`), `File::from_bytes_owned` (`:81`),
`FromStr` / `TryFrom<&BStr>` (`src/file/impls.rs:7,16,27`). Only three
`std::fs` sites exist, all in the path-based comfort constructors
(`src/file/init/from_paths.rs:34,89`, `src/file/includes/mod.rs:111`) — and
that last one is the `include.path` machinery that architecture.md appendix
issue #8 already wants to audit. **Using `from_bytes_no_includes` and resolving
includes ourselves resolves issue #8 by construction.**

Its one cost: **`gix-config` depends on `gix-sec`**
(`gix-config-0.59.0/Cargo.toml:95-96`) — see below.

### 6.2 The plumbing set compiles for `wasm32-unknown-unknown` today

Built for real on this machine (`rustc` stable, target installed):

```
gix-object 0.63 (sha1) + gix-pack 0.73 (no default features, sha1)
+ gix-ref 0.66 + gix-index 0.54 + gix-traverse 0.60 + gix-revision 0.48
+ gix-revwalk 0.34 + gix-diff 0.66 (no default features) + gix-imara-diff 0.2.4
+ gix-odb 0.83
→ cargo build --target wasm32-unknown-unknown : SUCCEEDS
```

`memmap2 0.9.11`, `gix-lock 24.0.0`, `gix-tempfile 24.0.0` and
`gix-commitgraph 0.38.0` are all in that tree and all compile — they merely fail
or degrade at *runtime* on wasm, which the byte-path code never reaches.

Adding **`gix-config 0.59`** breaks it, and adding `gix-discover 0.54` would
too. Both for the same single reason — `gix-sec`:

```
$ cargo tree -i gix-sec
gix-sec v0.14.2
└── gix-config v0.59.0
error[E0425]: cannot find function `geteuid` in crate `libc`
  --> gix-sec-0.14.2/src/identity.rs:44:38
error[E0599]: no method named `uid` found for struct `Metadata`
  --> gix-sec-0.14.2/src/identity.rs:38:21
```

This is exactly the failure the prior research documented
(`gix-research-2026-08.md` §0, citing `gix-sec-0.14.2/src/identity.rs:30` where
`target_os = "wasi"` is special-cased to `Ok(true)` but bare `wasm32` is not).

**What changes is the size of the ask.** The prior research treated wasm as
blocked by (a) `gix-sec` *and* (b) gix-pack's mmap, with (b) requiring an
upstream feature. (b) turns out to already exist (§3.1). (a) is a
two-line cfg extension to a special case that is already there for WASI, and
Path 2 reduces its blast radius to a single crate (`gix-config`) that we could
also bypass with `from_bytes_no_includes` behind a target cfg while the upstream
PR lands.

**Architecture.md appendix issues #1 and #12 should be rewritten**, and #13
(`gix-sec` wasm cfg) promoted from side-note to the single upstream blocker.

---

## 7. Honest counterweights

Reasons a reviewer could still choose Path 1, stated as strongly as I can:

1. **`status` and `blame` dominate the budget and Path 2 does not make them
   worse — but it does not make them better either.** Both are L under either
   path. If the read profile's real cost centre is `status`, the facade-vs-
   plumbing choice moves less of the total than this document's framing implies.
2. **`gix-ref` write is a wall.** Everything past the read profile
   (`commit`, `worktree`, ledger-gated writes) needs ref transactions, and over
   VFS that means reimplementing `gix-lock`'s atomicity. If the roadmap's centre
   of gravity is write verbs, Path 2-VFS buys a debt.
3. **Rename detection is lost** (§5.4) and it is user-visible in `status` and
   `diff`. Exact-match-only renames are a real fidelity regression from git.
4. **Version churn multiplies.** Path 2 pins ~12 crates that bump on
   independent schedules instead of one facade version. Every bump is a
   compatibility matrix. (Mitigated: they release in lockstep from one repo, and
   we already pin exactly.)
5. **We inherit maintenance of ~1000-2000 lines that Byron maintains for us
   today.** The facade's convenience layer is not incidental complexity; parts
   of it (revspec delegate, ref peeling edge cases, worktree/common-dir rules)
   encode git behaviours we would rediscover through bugs.

Counterweight to the counterweights: Path 2 collapses **three** separately-hard
goals — the structural no-spawn guarantee, git-over-kaish-VFS, and a browser
build — into one body of work, and §6.2 shows the browser build is now one
upstream cfg fix away rather than an open research question.

---

## 8. Recommendation

**Path 2, staged.** Concretely:

1. **Build Path 2-native first** (M). It ships the read profile and delivers
   the structural "spawn code not linked" guarantee that motivated the fork.
   The §A.4 tripwire becomes literally true and testable: `cargo tree -i
   gix-command` must fail.
2. **Design the object/ref/index access behind kaish-owned traits from day
   one** — `Find`/`Exists`/`FindHeader` on the object side (they already are
   the seam), and a small internal `RefRead`/`RefWrite` split on the ref side.
   Back them with `gix-odb::Store` and `gix-ref::file::Store` initially. Then
   VFS is a second implementation, not a refactor.
3. **Take the `gix-sec` fix upstream now** — it is the only wasm compile
   blocker left, it is tiny, and kaish-extras is the "known downstream
   consumer" Byron asked for.
4. **Retire the gix-pack upstream ask** (architecture.md appendix #12) — it is
   already implemented. Replace it with the `gix-commitgraph::File::from_data`
   ask (§3.5), which is the same shape and much lower stakes.
5. **State the rename-detection limitation in the design**, not at
   implementation time.

If Amy prefers to ship sooner and revisit, **Path 1 remains defensible** — but
note that its runtime-unreachability guarantee is a claim about configuration
(`Permissions::isolated()`) that must be re-proved on every gix bump, whereas
Path 2's is a property of the dependency graph that CI can assert in one line.
