# kaish agent-safety facilities — point-in-time inventory (2026-08-01)

Design input for kaish-git (see [../git.md](../git.md)). Surveyed against kaish
v0.12.0 by a code-reading agent with file:line citations; **line numbers will
drift** — treat citations as "where to look", not gospel. The "where latch falls
short" section at the bottom is the load-bearing part for the approval-ledger
design.

---

**Headline finding up front:** there is no in-tree git support of any kind. Git today is an ordinary external command run through the `subprocess` feature (`docs/EMBEDDING.md:168-171`); the only "git" awareness in the kernel is gitignore parsing (`crates/kaish-kernel/src/ignore_config.rs:36,125,169,290`) and lexer/parser test fixtures. `KernelBackend::resolve_real_path` explicitly names git as the motivating consumer for the VFS→real-path escape (`crates/kaish-tool-api/src/backend.rs:111-116`).

---

## 1. The confirmation latch (deepest coverage)

### 1.1 What it is

A two-phase gate on destructive operations: first call returns **exit code 2** with a typed `LatchRequest` and touches nothing; a second call carrying `--confirm=<nonce>` performs the operation. Off by default; enabled per-kernel or per-session.

### 1.2 Data flow, end to end

```
set -o latch  ──►  Scope.latch_enabled = true
                   crates/kaish-kernel/src/tools/builtin/set.rs:126,147,171
                   crates/kaish-kernel/src/interpreter/scope.rs:396,602,607

Kernel::execute / execute_argv
  └─ dispatch seam: capture (dispatch_name, ToolArgs::to_argv())
       crates/kaish-kernel/src/kernel.rs:3321-3322  →  ctx.current_invocation
       (field: crates/kaish-kernel/src/tools/context.rs:162)
  └─ tool.execute(args, ctx)          kernel.rs:3324
       └─ rm:    decide_rm_action → RmAction::Latch
                 crates/kaish-kernel/src/tools/builtin/rm.rs:55-105, 208-224
          tee/write/patch/sed -i/cp/mv/dd of=:
                 ExecContext::gate_overwrites → MutationAction::Latch
                 crates/kaish-kernel/src/tools/context.rs:828-921
                 decision table: context.rs:236-258
          kaish-trash empty: unconditional gate (latch flag irrelevant)
                 crates/kaish-kernel/src/tools/builtin/kaish_trash.rs:168
       └─ ExecContext::latch_result(command, paths, reason, hint_fn)
                 crates/kaish-kernel/src/tools/context.rs:759-800
            ├─ NonceStore::issue(command, paths)  →  8-hex-char nonce
            │     crates/kaish-kernel/src/nonce.rs:99-117
            └─ ExecResult{ code: 2, err: prompt, latch: Some(Box<LatchRequest>) }
                  LatchRequest def: crates/kaish-types/src/result.rs:74-116
                  ExecResult.latch field: crates/kaish-types/src/result.rs:208

Embedder reads:  result.latch_request()        crates/kaish-types/src/result.rs:547
Embedder approves: Kernel::confirm(&req)       crates/kaish-kernel/src/kernel.rs:1594-1619
  └─ prepends Value::String("--confirm=<nonce>") then execute_argv(req.tool, argv)
     (prepend, not append — to_argv() trails a `--` terminator: kernel.rs:1602-1607)
  └─ on success + req.job_id: retires the originating background job
     kernel.rs:1609-1616 (guarded by JobManager::is_latched)
```

### 1.3 Who stores the nonce

`NonceStore` (`crates/kaish-kernel/src/nonce.rs:46-165`) — an `Arc<Mutex<HashMap<String, (Instant, NonceScope)>>>` with a TTL (default **60s**, `nonce.rs:59`). It lives on `ExecContext.nonce_store` (`crates/kaish-kernel/src/tools/context.rs:115`) and is **cloned into every pipeline stage / fork** (`context.rs:712`), so `Arc` sharing means a nonce issued in one stage validates in another.

Key properties:
- **Scope = `(command, paths)`** (`nonce.rs:20-25`). Validation requires exact command match and confirmed paths ⊆ authorized paths (`nonce.rs:125-159`). Subset accepted, superset rejected (`nonce.rs:283-301`).
- **Not consumed on validation** — reusable within TTL, so retries are idempotent (`nonce.rs:124`, tests at `nonce.rs:209-217`).
- **Opportunistic GC** on each `issue()` (`nonce.rs:112-113`).
- **Lifetime:** fresh per kernel unless the embedder passes a shared store via `KernelConfig::with_nonce_store` (`crates/kaish-kernel/src/kernel.rs:616-618`, field at `kernel.rs:248`, wired at `kernel.rs:1185-1187`).
- **Nonce quality is weak**: `generate_nonce` (`nonce.rs:174-191`) is `RandomState` + `SystemTime` nanos folded to a **u32** (`{:08x}` of `hasher.finish() as u32`). Not a CSPRNG, 32 bits of space. The crate already depends on `getrandom` for `mktemp`, so this is a deliberate-or-accidental gap. For an authorization token this is a sharp edge: guessable/collidable in a hostile setting, and there is no rate limit on `--confirm` attempts.

### 1.4 The exit-code-2 contract and output surfaces

| Surface | Where |
|---|---|
| `ExecResult.code == 2`, `err` = human prompt, **stdout empty** | `crates/kaish-kernel/src/tools/context.rs:775-777` |
| `ExecResult.latch: Option<Box<LatchRequest>>` — control-plane, **never** `.data` | `crates/kaish-types/src/result.rs:195-208` |
| Survives `clear_stdout` (a `rm big > log` redirect can't drop the gate) | `crates/kaish-types/src/result.rs:496`; devlog `docs/devlog.md:884-901` |
| `--json` envelope: `{"error","code":2,"latch":{...}}` under its own key | `crates/kaish-types/src/output.rs:533-596` (`latch_envelope`) |
| Survives the `ExecResult`↔`ToolResult` backend roundtrip | `crates/kaish-types/src/backend.rs:258-262, 337-339, 390, 415` |
| Mid-pipeline gate overrides a later stage's success (GH #125) | `crates/kaish-kernel/src/scheduler/pipeline.rs:626-657` |
| `scatter`/`gather` rows carry the latch object (GH #124 pt 3) | `crates/kaish-kernel/src/scheduler/scatter.rs:537-545` |
| `wait` surfaces the first latch, stamps `job_id` (GH #124 pt 4) | `crates/kaish-kernel/src/tools/builtin/wait.rs:118-157` |

The `LatchRequest` fields (`crates/kaish-types/src/result.rs:74-116`): `nonce`, `command` (display label), `paths` (resolved), `hint` (display-only, **does not quote paths** — `result.rs:82-86`), `tool` (argv0 for replay), `argv` (exact captured, minus `--confirm`), `ttl`, `job_id` (`Option<u64>`, background only).

### 1.5 Background-job latch surfacing (GH #92 → #96 → #124/#125)

- `JobStatus::Latched` — a held state distinct from `Failed` (`crates/kaish-types/src/job.rs:27-30`; computed at `crates/kaish-kernel/src/scheduler/job.rs:188-194`, string form at `job.rs:202-212`).
- `JobInfo.latch: Option<LatchRequest>` (`crates/kaish-types/src/job.rs:62-66`, builder `job.rs:99-101`).
- **`Job::latch()` is the single chokepoint** that stamps `job_id` onto every surfaced request (`crates/kaish-kernel/src/scheduler/job.rs:223-232`). Every reader goes through it: `list` (`job.rs:631-643`), `get` (`job.rs:713-723`), `get_latch` (`job.rs:741-744`), `is_latched` (`job.rs:707-710`), `reap_finished` (`job.rs:670-689`).
- **`/v/jobs/{id}/latch`** VFS node — pretty JSON, or empty body when not latched (`crates/kaish-kernel/src/vfs/jobfs.rs:130-140`, listed at `jobfs.rs:231`, whitelist at `jobfs.rs:278`).
- **`jobs --json`** rows carry `latch` only on `Latched` rows (`crates/kaish-kernel/src/tools/builtin/jobs.rs:10-30`).

### 1.6 Abandon / cleanup / kill semantics

This was a real, verified bug class ("The latch survives its consumers", `docs/devlog.md:145-178`) — both housekeeping consumers silently destroyed the only handle to a pending gate. Current state:

| Path | Behavior | Cite |
|---|---|---|
| `jobs --cleanup` / `reap_finished` | **Keeps** latched jobs, reports "Kept N latched job(s) awaiting confirmation" | `scheduler/job.rs:670-689`; `tools/builtin/jobs.rs:94-100` |
| `kill %N` on a latched job | **Refuses**, points at `/v/jobs/N/latch` | `tools/builtin/kill.rs:131-152` |
| `kill --discard %N` | Explicit abandon; clap-conflicts with `--signal` | `tools/builtin/kill.rs:31-35, 153-155` |
| `JobManager::remove` | **Bypasses the guard** — documented as such; `cleanup()` named as the safe bulk path | `scheduler/job.rs:887-897` |
| `Kernel::confirm` on a successful replay | Auto-retires the originating job, guarded by `is_latched` | `kernel.rs:1609-1616` |
| `fg`/`bg` | Structurally cannot reach a latched job (no pid/pgid, not stopped) | devlog `docs/devlog.md:165-168` |
| `Kernel::shutdown` | Waits for jobs, never removes | `kernel.rs:5869-5873` |
| Expiry | Nonce dies at TTL (60s); the job stays `Latched` forever with a **dead nonce** and no re-issue path | `nonce.rs:86-90` |

### 1.7 What the latch does **NOT** cover — for a git plugin, read this section

1. **It gates a fixed set of builtins, not arbitrary operations.** The complete set of gate call sites in the tree:
   - `rm` (`tools/builtin/rm.rs:221`)
   - `kaish-trash empty` (`tools/builtin/kaish_trash.rs:168`)
   - `gate_overwrites` callers: `tee:86`, `write:96`, `patch:169`, `sed:235`, `cp:134`, `mv:117`, `dd:180`
   - the shared implementation at `tools/context.rs:900`

   There is **no generic "gate this operation" entry point on the portable tool API**. `latch_result`, `verify_nonce`, and `gate_overwrites` are `pub` methods on `ExecContext` (`tools/context.rs:745, 759, 828`), *not* on the `ToolCtx` trait (`crates/kaish-tool-api/src/ctx.rs:54-122`). A third-party crate can reach them only by depending on `kaish-kernel` and doing `ctx.as_any_mut().downcast_mut::<ExecContext>()` — the `#[doc(hidden)]` escape hatch (`ctx.rs:106-121`), used by 92 in-tree builtins. `ExecContext` *is* publicly re-exported (`crates/kaish-kernel/src/lib.rs:103`), so this works — but it costs the whole kernel as a dependency and rides an explicitly-unsupported hatch.

2. **Gating is per-invocation, not mid-execution.** The gate must fire *before* the tool does its work and return exit 2 from `execute()`. There is no suspend-and-resume: a tool cannot get halfway through a multi-step operation, request approval, and continue. `Kernel::confirm` **re-runs the entire argv from scratch** (`kernel.rs:1602-1607`), so any gated operation must be idempotent-on-replay by construction. For a git plugin this means `git push` cannot gate after computing the refspec — it must gate up front and recompute on replay.

3. **No audit trail whatsoever.** Grepping the tree for `audit|ledger|approval|authoriz` turns up only the nonce's internal "authorized paths" vocabulary and unrelated test fixtures. Nothing records: who approved, when, from what policy, what the outcome was. A nonce is issued, validated (possibly many times), and silently GC'd (`nonce.rs:112-113`). `NonceStore::lookup` (`nonce.rs:77-94`) exists as an embedder read hook but returns only the live scope, and there is no enumeration API, no callback, no event.

4. **No automated/scoped policy layer inside the kernel.** The doc is explicit: "the kernel owns the *mechanism* … the embedder owns the *judgment*" (`docs/EMBEDDING.md:266-268`). There is no allowlist, no rule engine, no "pre-approve `rm` under `/tmp/scratch` for this session". The only automation the kernel offers is `KAISH_LATCH=1` (on/off) and TTL-window reuse of one nonce.

5. **`set +o latch` is available to script code.** The latch flag is ordinary shell scope state (`tools/builtin/set.rs:147`, `interpreter/scope.rs:607`) with no readonly/lock. Any script the agent runs can disable its own gate. There is no kernel-level "latch is pinned on" mode — `KernelConfig::latch_enabled` only seeds the initial scope (`kernel.rs:1205`).

6. **`KAISH_LATCH` / `KAISH_TRASH` are read from the OS environment** in `transient()`, `repl()`, `agent()`, `agent_with_root()` (`kernel.rs:339-340, 455-456, 490-491, 518-519`) — a direct exception to the "hermetic, never reads `std::env`" story, and an env-var vector that can *only* turn the gate on, never off, so the security direction is safe but the hermeticity claim is inexact.

7. **A latch raised by a custom `KernelBackend::call_tool` has empty `tool`/`argv`** and `Kernel::confirm` fails loud with exit 2 (`kernel.rs:1595-1601`; documented at `docs/EMBEDDING.md:279-284`). Only tools registered in the kernel registry get the dispatch-seam capture.

8. **`--confirm` is a per-builtin convention, not a framework flag.** Each tool declares its own (`rm.rs:30-32` uses `--confirm`; `dd.rs:98` uses the `confirm=<nonce>` key=value idiom). There is no schema-level marker for "this tool is gateable", so a policy engine cannot discover gateable operations from `tools --json`.

9. **The `hint` is unsafe for machine replay** — `format!`-built, does not quote paths with spaces or globs (`result.rs:82-86`). This is why `confirm` exists; but if a plugin surfaces the hint to a model as the approval affordance, the model can be led to run a *different* command than the one that gated.

10. **`wait` on several gated jobs keeps only the first latch** (`tools/builtin/wait.rs:138-140`; noted as an accepted design touch in `docs/devlog.md:717-718`). `ExecResult.latch` holds exactly one request — there is no multi-gate batch surface.

**Test coverage:** `crates/kaish-kernel/tests/latch_trash_tests.rs` — 60+ tests including the capstone background loop `backgrounded_latch_is_reachable_and_confirmable:1173`, `confirm_retires_the_originating_backgrounded_job:1202`, `jobs_cleanup_keeps_latched_job:1334`, `kill_refuses_latched_job:1369`, `confirm_replays_a_path_with_spaces_the_hint_cannot:215`, `latch_in_a_pipeline_stage_overrides_later_success:370`.

---

## 2. Capability feature axes

Declared at `crates/kaish-kernel/Cargo.toml:98-138`; documented at `docs/EMBEDDING.md:146-172`.

| Feature | Gates | Default | Cite |
|---|---|---|---|
| `localfs` | `LocalFs` backend, Passthrough/Sandboxed VFS modes, spill-to-disk | ✓ | `Cargo.toml:108` |
| `overlay` | Copy-on-write overlay wiring (implies `localfs`) | ✓ | `Cargo.toml:113` |
| `subprocess` | External commands: exec/spawn/which/bg/fg/kill, PATH, signals, controlling-terminal job control, pidfd. "The single largest attack-surface axis." Implies `localfs`. | — | `Cargo.toml:115-119` |
| `host` | `ps` (procfs, via `kaish-tools-host`), `uname --host`, `hostname` | — | `Cargo.toml:121-124` |
| `os-integration` | freedesktop trash + XDG dirs (`dep:trash`, `dep:directories`) | — | `Cargo.toml:126-127` |
| `tokens` | BPE tokenization | — | `Cargo.toml:130` |
| `full` / `native` | all of the above | — | `Cargo.toml:133-135` |
| `schema` | JSON-schema derivation | — | `Cargo.toml:137` |

`kaish-vfs` has its own finer axes: `localfs`, `memory`, `overlay`, all default-off (`crates/kaish-vfs/Cargo.toml:26-37`).

**A no-default-features build** (what `kaish-wasi` does: `crates/kaish-wasi/Cargo.toml:10`) has: no real filesystem at all, no external processes, no host introspection, no trash. It gets the memory VFS, `/dev` synthetics, and the full text-processing builtin set. Behaviour is pinned by `crates/kaish-kernel/tests/sandbox_mode_tests.rs` (~100 tests) covering: memory-only VFS with no host leaks (`:60`), file tests cannot probe the host (`:174`), external commands blocked (`:190`), gated builtins absent from help (`:227-240`), `/dev/*` synthetics (`:298-352`), and extensive binary-data round-trips.

**Consequence for `kaish-git`:** git-as-external needs `subprocess` (compile) **and** `allow_external_commands` (runtime). A libgit2/gitoxide-based plugin would instead need `resolve_real_path` to return `Some` — i.e. a `localfs`-backed mount — which means it cannot work under `NoLocal`, under a pure-`MemoryFs` mount, or against a `with_backend` embedder backend that is not disk-backed.

---

## 3. VFS

- **`Filesystem` trait** — `crates/kaish-vfs/src/traits.rs:17-…`. Path-typed (`&Path`, not `&str`). Notables: `read_range` with an in-memory-slice default that infinite devices must override (`traits.rs:21-34`); `set_mtime` defaults to `Unsupported` with an explicit **no-silent-no-op** rule (`traits.rs:57-69`); `read_only()` (`traits.rs:71-72`); `resident_bytes()` for RAM accounting (`traits.rs:74-86`).
- **`ReadRange`** — `crates/kaish-types/src/backend.rs:149`, re-exported through `kaish-vfs` (`crates/kaish-vfs/src/lib.rs:17`) and used on `KernelBackend::read` (`crates/kaish-tool-api/src/backend.rs:31`).
- **Backends**: `LocalFs` (`crates/kaish-vfs/src/local.rs` — `new` at `:26`, **`read_only` ctor at `:34`**, `set_read_only` at `:42`, enforcement at `:181`), `MemoryFs` (`crates/kaish-vfs/src/memory.rs`), `OverlayFs` (`crates/kaish-vfs/src/overlay.rs`), `DevFs` (`crates/kaish-vfs/src/dev.rs`).
- **`OverlayFs`** — `new:104`, `with_budget:111`, `over:136`, `over_with_budget:144`, `changes():345`, `is_dirty():358`, `reset():378`, `reset_all():414`, `commit_into():529`, `fork_into():649`. Design rationale in `docs/kaish-overlayfs.md`. Surfaced to scripts via the `kaish-vfs` builtin (`status|diff|commit|reset`, `crates/kaish-kernel/src/tools/builtin/kaish_vfs.rs:3-119`) and to embedders via `OverlayHandle { fs, mount_path, commit_root }` (`crates/kaish-kernel/src/kernel.rs:698-706`).
- **Mounts** — `VfsRouter::mount` / `mount_arc` (`crates/kaish-kernel/src/vfs/router.rs:48, 54`), longest-prefix routing, per-op tracing spans (`router.rs:229-277`). `MountInfo` is exposed to tools via `KernelBackend::mounts()` (`crates/kaish-tool-api/src/backend.rs:109`).
- **Byte budgets** — `ByteBudget` (`crates/kaish-vfs/src/budget.rs:22`, `labeled:37`, `used:51`, `remaining:61`). Config: `KernelConfig::with_vfs_budget` / `without_vfs_budget` (`kernel.rs:664, 673`); `agent()`/`agent_with_root()` default to **64 MiB** (`kernel.rs:496, 524`), others unbounded. Over-budget writes fail loud with `StorageFull` (`kernel.rs:660-663`). Budget is Arc-shared into forks so background jobs draw the same pool (`kernel.rs:740-745`). **Note:** a `with_backend` kernel's budget does *not* cover embedder mounts — cap them yourself with `MemoryFs::with_budget` (`docs/EMBEDDING.md:326-337`).
- **`SpillMode`** — `crates/kaish-kernel/src/output_limit.rs:42-56`. `Disk` (default) writes overflow under `paths::spill_dir()`; `Memory` truncates head+tail with no host I/O. **Auto-forced to `Memory`** for `NoLocal` and for every `with_backend` kernel, overriding an explicit `Disk` (`kernel.rs:1144-1150, 1108-1121`), because a host write would bypass the embedder's VFS. Tested at `output_limit.rs:986-1005`.

---

## 4. Sandbox modes / `NoLocal` / `with_backend`

`VfsMountMode` — `crates/kaish-kernel/src/kernel.rs:122-185`:
- **`Passthrough`** (`:136`) — native `/`; the REPL's mode.
- **`Sandboxed { root }`** (`:154`) — restricted subtree; the agent mode.
- **`NoLocal`** (`:175`) — complete isolation, memory only.
- Default is `Sandboxed { root: None }` with `localfs`, `NoLocal` without (`:182-184`).

Presets (`kernel.rs`): `transient():376/400` and `named():406/430` → Sandboxed; `repl():442` → Passthrough; `agent():479` / `agent_with_root():508` → Sandboxed + 64 MiB budget; **`isolated():533` → `NoLocal` + `allow_external_commands: false` + `latch_enabled: false`**.

Overlay interactions fail loud rather than silently degrading: `overlay=true` + `NoLocal` errors at construction (`kernel.rs:1000-1007`), `overlay=true` + `with_backend` errors (`kernel.rs:1075-1082`).

**`Kernel::with_backend`** (`kernel.rs:1067-1092`) — the embedder-owned-storage door. Guarantees:
- Kernel-owned mounts only: `/v/jobs` (JobFs), `/v/blobs` (MemoryFs), `/dev` (DevFs, kernel-owned specifically so `> /dev/null` works even against a read-only embedder backend — `kernel.rs:1074-1078`).
- `configure_vfs` and `configure_tools` closures for extra mounts and **third-party tool registration** (`kernel.rs:1041-1042, 1080-1081`).
- **Hermetic by construction**: `no_host_filesystem = true` unconditionally forces in-memory spill and disables background-job output files (`kernel.rs:1087, 1108-1121`; warned at `docs/EMBEDDING.md:310-316`).

`crates/kaish-kernel/tests/sandbox_mode_tests.rs` pins the `isolated()` config end to end (see §2). Companion suites: `with_backend_dev_tests.rs`, `with_backend_v_overlay_tests.rs`, `overlay_tests.rs`, `vfs_budget_tests.rs`.

**Also relevant: `Kernel::classify_command`** — an embedder preflight classifier (`CommandKind` = `Builtin`/`UserTool`/`Special`/`Dynamic`/`External`, plus `escapes_kernel()`), designed exactly for consent gating and living in `kaish-types` as `#[non_exhaustive]` so an unknown kind defaults to "gate it". Documented at `docs/EMBEDDING.md:593-630`; design rationale and the alias-expansion hole it closes at `docs/devlog.md:1528-1585`. Kept honest against the interpreter by a `SpecialForm` enum that makes drift a **compile error**, plus a `classify_command_matches_executor` drift test. Residual over-reporting (`/v/bin/cat`, `.kai`/backend tools report `External`) is deliberately in the safe direction.

---

## 5. ExecuteOptions, cancellation, watchdog, kill

**`ExecuteOptions`** — `crates/kaish-types/src/kernel.rs:30-88`, builders `:90-165`:
- `vars` (`:32`) — bash function-local overlay, exported, popped on return.
- `timeout` (`:40`) — exit **124**; `Some(ZERO)` is a validate-only dry run.
- `cancel_token` (`:48`) — embedder-owned, **raced** against the kernel's internal token, never stored. Kernel timeouts do *not* fire your token; distinguish by code: 124 = timeout, 130 = cancel (`:20-28`).
- `cwd` (`:55`), `stdin` (`:78`).
- `traceparent` / `tracestate` / `baggage` (`:56-69`).
- **`interrupt`** (`:79-87`) — `Option<Arc<dyn Fn() -> bool + Send + Sync>>`, polled at every cancellation checkpoint; motivated by single-threaded wasm. Polled inside `Kernel::is_cancelled` (`crates/kaish-kernel/src/kernel.rs:1443-1449`), so every call site is an interrupt checkpoint for free.

**Watchdog** — `crates/kaish-kernel/src/watchdog.rs:52-140`. Movable-deadline timer; `run` re-arms on deadline moves (`:71`); `hold(budget)` returns a `WatchdogHold` whose `Drop` restores (`:91, 132-139`). Driven by `Kernel::run_under_watchdog` (`kernel.rs:1633-1650`), which mirrors the handle into `exec_ctx` and clears it on the way out.

**`ToolCtx::patient(budget)`** — `crates/kaish-tool-api/src/ctx.rs:101-104`, guard type at `:22-41`. Freezes the *script* clock while held; the hold's own budget still has teeth (exit 124); **cancellation stays live** — a patient tool must still `select!` on the cancel token (`ctx.rs:92-94`); script code has no path to the guard; the explicit `timeout` builtin is never suspended (`ctx.rs:96-97`). Default impl is inert. Full semantics at `docs/EMBEDDING.md:541-580`; tests in `crates/kaish-kernel/tests/patient_watchdog_tests.rs`.

**Kill semantics** — `Kernel::cancel()` (`kernel.rs:1437-1441`) fires the internal token; cancellation cascades to external children via `wait_or_kill` (SIGTERM → `kill_grace` → SIGKILL on the process group). `kill_grace` defaults to 2s, configurable via `with_kill_grace` (`kernel.rs:651-653`). Per-job pgids tracked at `scheduler/job.rs:860-885`. `shutdown()` waits for all jobs and removes none (`kernel.rs:5869-5873`). Tests: `cancellation_tests.rs`, `interrupt.rs`, `spawn_timeout_kill_tests.rs`.

---

## 6. Trash and other destructive-op protections

**`TrashBackend` trait** — `crates/kaish-kernel/src/trash.rs:110-139`: `trash` (move), **`trash_bytes`** (copy a snapshot for overwrite gating — `:117-125`), `list`, `find_by_name`, `restore`, `purge_all`. System impl in `trash_system.rs` (needs `os-integration`). `TrashId` wraps the backend id so callers don't depend on the `trash` crate (`trash.rs:22-37`).

**Semantics** (`set -o trash`, `crates/kaish-kernel/src/interpreter/scope.rs:398, 612-628`):
- `rm`: files under `trash_max_size` (**default 10 MB**, `scope.rs:429`) and *all* directories go to trash; larger files fall through to the latch check, then to permanent delete (`tools/builtin/rm.rs:55-105`).
- Overwrites: `decide_mutation_action` (`tools/context.rs:236-258`) — **trash wins over latch** (trash *is* the safety net); prior bytes are **copied**, not moved, so the file keeps its identity for read-modify-write callers (`context.rs:923-952`).
- Exclusions: real paths under `/tmp` only (`is_trash_excluded`, `context.rs:219-221`). Deliberately **no `/v` lexical exclusion** — a `with_backend` embedder's real content under `/v` must keep the safety net (`context.rs:213-218`).
- **Trash failure is loud and never falls through to a destructive op** (`context.rs:916, 947-950`; test `latch_trash_tests.rs:564`). Missing backend is also loud (`context.rs:938-941`; test `:586`).
- `kaish-trash empty` gates on a nonce **unconditionally**, independent of `set -o latch` (`tools/builtin/kaish_trash.rs:168`).

**Adjacent protections:**
- **Compare-and-swap overwrite** — `cas_overwrite` (`tools/context.rs:269-292`) re-reads and compares against the gate's snapshot; a concurrent change is a loud `InvalidOperation`, never a silent clobber. A re-read failure propagates rather than defaulting to empty bytes (`context.rs:276-280`). Documented residual: not OS-atomic (no write-temp-then-rename primitive yet, `context.rs:266-268`).
- **Symlink safety** in `rm`/`mv` — `crates/kaish-kernel/tests/rm_mv_symlink_safety_tests.rs`; glob-flag-injection guard in `rm_glob_flag_injection_tests.rs`.
- **Output limiting** (`output_limit.rs`) — caps runaway output, remaps exit code to 3 on spill (`crates/kaish-types/src/result.rs:173-180`).
- **Recursion guard** — `MAX_RECURSION_DEPTH` 48 / `RECOMMENDED_STACK_SIZE` (GH #46/#47/#48, `docs/devlog.md:622-770`); `recursion_guard_tests.rs`.

---

## 7. OpenTelemetry / tracing

**`crates/kaish-kernel/src/telemetry.rs`** is the whole surface:
- `extract_parent(&ExecuteOptions) -> Option<Context>` (`:38-65`) — W3C `TraceContextPropagator` over a synthetic carrier; `tracestate` is dropped without a `traceparent` (`:33-34, 50-52`); baggage is set directly from the map, not round-tripped through a header, so identifier values survive verbatim (`:34-36, 56-62`).
- `merge_egress_baggage(&mut ExecResult, embedder_map)` (`:78-82`) — echoes embedder baggage back onto `ExecResult.baggage` (`crates/kaish-types/src/result.rs:189-194`) without clobbering tool-emitted entries (`.entry().or_insert()`).
- Fork-context helper (`:104-120`) via `tracing-opentelemetry`'s `OpenTelemetrySpanExt`, so forked spans nest and baggage propagates. Degrades to a no-op when no OTel layer is installed (`:110-113`).
- **Ordering constraint worth knowing:** the caller must attach the parent context as current *before* the `#[instrument]` span is created — you cannot re-parent an already-started async span (`telemetry.rs:3-13`).

**Existing spans (the complete set):**

| Span | Level | Cite |
|---|---|---|
| `execute_argv` (`cmd`, `argc`) | info | `kernel.rs:1540` |
| `execute_with_options` (`input_len`) | info | `kernel.rs:1819` |
| `execute_pipeline` (`command_count`) | debug | `kernel.rs:2931` |
| `execute_command` (`command`) | debug | `kernel.rs:5148` |
| scatter (`item_count`, `parallelism`) | info | `scheduler/scatter.rs:147` |
| scatter workers (`worker_count`) | debug | `scheduler/scatter.rs:236` |
| VFS router read/write/list/stat | trace | `vfs/router.rs:229-277` |

Tests: `crates/kaish-kernel/tests/trace_context_tests.rs`.

**Where an approval-ledger call site would attach a span:** the natural chokepoints are `ExecContext::latch_result` (`tools/context.rs:759`, issue-time) and `Kernel::confirm` (`kernel.rs:1594`, approve-time). Neither is instrumented today. `confirm` sits *outside* the `execute_argv` span it then creates, so an `approval` span there would correctly parent the replay. The dispatch seam at `kernel.rs:3091` deliberately uses a breadcrumb event rather than a span (per its own comment) — worth respecting if you add ledger events on the hot path.

---

## 8. The tool API surface (`kaish-tool-api`)

The stated purpose: "out-of-tree tool bundles … can be written against a small, audited surface" without pulling in the parser/interpreter/runtime (`crates/kaish-tool-api/src/lib.rs:1-26`).

**What a third-party `kaish-git` CAN reach with only `kaish-tool-api`:**
- `Tool` (`src/tool.rs:19-36`) — `name()`, `schema()`, `execute(args, &mut dyn ToolCtx) -> ExecResult`, `validate()`.
- `ToolCtx` (`src/ctx.rs:54-122`) — `backend()`, `cwd()`, `resolve_path()`, `var()`, `set_var()`, `set_output_format()`, `patient()`, plus the `#[doc(hidden)]` `as_any`/`as_any_mut`.
- `KernelBackend` (`src/backend.rs:25-117`) — full VFS I/O (`read` with `ReadRange`, `write`/`append`/`patch`/`list`/`stat`/`mkdir`/`set_mtime`/`remove`/`rename`/`exists`/`lstat`/`read_link`/`symlink`), tool re-dispatch (`call_tool`/`list_tools`/`get_tool`), and introspection (`read_only()`, `backend_type()`, `mounts()`, **`resolve_real_path()`** — the escape hatch that explicitly names git as its consumer, `backend.rs:111-116`).
- `GlobalFlags` (`src/global_flags.rs:21-47`) — flatten into your clap struct, call `parsed.global.apply(ctx)`. The kernel *also* calls `apply_from_args` before `execute()` so `--json` survives a clap parse failure (`global_flags.rs:36-47`; invoked at `kernel.rs:3293`).
- Clap reflection: `schema_from_clap` / `schema_tree_from_clap` / `params_from_clap` (`src/clap_schema.rs`), `validate_against_schema` (`src/tool.rs:50-150`).
- Re-exported data types: `ExecResult`, `OutputData`, `OutputFormat`, `ParamSchema`, `ToolArgs`, `ToolSchema`, `Value` (`src/lib.rs:44-46`).

**`ToolSchema` knobs relevant to a plugin** (`crates/kaish-types/src/tool.rs:197-…`): `subcommands` for subcommand-aware tools (`:210-221`, walked by the kernel's `select_leaf`), `aliases` (`:222-226`), `map_positionals` (`:206-209`), **`owns_output`** (`:227-235`, builder `with_owned_output` at `:347`), and raw-argv-order mode (`:236-242`).

`owns_output` semantics: the kernel skips `apply_output_format` when the tool owns output *and* succeeded (`kernel.rs:6003-6020`) — a failure still gets the standard envelope (`kernel.rs:9022-9026`). It **also** makes `--help`/`-h` your responsibility with no safety net (`kernel.rs:3244`; `docs/EMBEDDING.md:530-539`). A git plugin with `git log --json`-style bespoke envelopes wants this; a git plugin returning typed `OutputData` does not.

**What it CANNOT reach:**
- The **latch API** — `latch_result`, `verify_nonce`, `gate_overwrites`, `NonceStore`, `current_invocation` all live on `ExecContext`/`kaish-kernel` (§1.7 item 1).
- Job control, streaming pipes, the dispatcher, `Scope` (latch/trash flags), the trash backend, the overlay handle, the cancellation token — all `ExecContext` fields.
- Registration requires the kernel: `Kernel::with_backend(backend, config, configure_vfs, configure_tools)` (`kernel.rs:1067-1071`), `tools.register(MyTool)` (`docs/EMBEDDING.md:487-527`).

So: a `kaish-git` that only reads is a clean `kaish-tool-api` consumer. A `kaish-git` that wants approval-gated writes must today depend on `kaish-kernel` and downcast.

---

## 9. Hermetic env handling

- The kernel **never reads `std::env::vars()`** (`kernel.rs:253-256`; `docs/EMBEDDING.md:337-345`). External commands see only what kaish has marked exported.
- `KernelConfig::initial_vars` is the opt-in door: `with_var` / `with_vars` / `with_initial_vars`. All entries are marked exported at boot (`kernel.rs:1190-1205`). Retained past construction so `reset()` re-seeds them (`kernel.rs:729-733`).
- Per-call overlay via `ExecuteOptions::vars` — pushed as a scope frame, exported, popped on return (`crates/kaish-types/src/kernel.rs:31-32`, `docs/EMBEDDING.md:400-405`).
- `~` expansion uses the *session* `HOME`, not the process env (`kernel.rs:3365-3367`; `interpreter/eval.rs:868`). Tests: `hermetic_home_tests.rs`, `env_filter_tests.rs`.
- **Documented exceptions where the kernel does touch the process env:** XDG/`HOME` path primitives (`paths.rs:58,74,97,120,143`; `kernel.rs:320,959`), PATH fallback for `exec`/`spawn`/`which`/dispatch (`dispatch.rs:592`, `spawn.rs:123`, `which.rs:82`, `exec.rs:102`) — these fall back to `std::env::var("PATH")` only when the session has no `PATH`, and `KAISH_LATCH`/`KAISH_TRASH` (§1.7 item 6). For a git plugin this matters: `git` shells out reading `GIT_*`, `HOME`, `SSH_AUTH_SOCK`, `GIT_CONFIG_*` — none of which reach the child unless the embedder seeds them, so a "hermetic" kaish + external git is a *different* git than the user's interactive one, and credential helpers will not resolve.

---

## Where latch falls short as an approval system for writeable git

The latch is a well-built **speed bump for a small, fixed set of filesystem verbs**. It is not an authorization system, and the gap is structural, not incremental.

**1. It is per-builtin, not per-operation.** There are exactly ten gate call sites, all hardcoded, all in-tree (§1.7 item 1). The mechanism to raise a gate (`latch_result`) is a `pub` method on `ExecContext` — a kernel type — and is absent from the portable `ToolCtx`. A `kaish-git` cannot gate `push`/`commit`/`reset --hard`/`clean -fdx` without depending on the entire kernel crate and going through the explicitly-unsupported `as_any_mut` downcast. Before writeable git, the first thing to build is a `ToolCtx`-level gate API (something like `fn request_approval(&self, op: ApprovalRequest) -> Result<Approval, ExecResult>`), so plugins are first-class producers of gates rather than downcasting squatters.

**2. There is no approval record — the nonce *is* the record, and it evaporates.** `NonceStore` holds `(created_at, command, paths)` in a HashMap that is GC'd on the next `issue()` (`nonce.rs:112-113`). Nothing persists who approved, under what policy, at what time, with what outcome, or whether the approval was even exercised. A real system needs an append-only ledger entry per *request* (issued, scope, requester, trace context) and per *decision* (approved/denied/expired, by whom, outcome exit code). The natural attach points exist — `latch_result` (`context.rs:759`) and `Kernel::confirm` (`kernel.rs:1594`) — and neither is instrumented, so today there is not even a span or event to hang a ledger sink on (§7).

**3. Policy is entirely outside the kernel, and there is no vocabulary to express it.** `LatchRequest.command` is a display label; `paths` is a flat string list; there is no operation taxonomy, no risk class, no resource identity beyond a path. An embedder writing "auto-approve `git fetch`, gate `git push` to protected branches, never allow `git push --force`" has to string-match `command` and re-parse `argv`. For git specifically, the interesting resource is not a path — it is a *ref*, a *remote*, and a *reachability claim* ("this push is fast-forward"). The latch's `(command, paths)` scope model cannot express any of that, and `NonceStore::validate` (`nonce.rs:125`) only knows how to do set-subset checks on strings.

**4. No double-confirmation, no escalation, no principal.** One nonce, one class of approval, no notion of *who* approved. `--confirm=<nonce>` is a bearer token: anything holding it — including the very agent that was supposed to be gated — can redeem it. Since `set +o latch` is script-reachable (§1.7 item 5) and the nonce is 32 bits from a non-CSPRNG (§1.3), the latch's security model is honestly "prevent accident", not "prevent an adversarial or confused agent". A writeable git plugin whose worst case is `push --force` to `main` needs a stronger claim than that: a nonce bound to a principal, a single-use mode for irreversible ops, and an out-of-band channel (the human's terminal, not the agent's stdout) for the high-risk tier.

**5. Approval is replay, not resume — so every gated op must be idempotent-on-replay.** `Kernel::confirm` re-runs the whole argv from scratch (`kernel.rs:1602-1607`). Anything the tool computed before the gate is recomputed after it, and there is no guarantee the world is unchanged in between (the TTL window is 60s of TOCTOU). The overwrite gate handles this with an explicit compare-and-swap against the snapshot (`cas_overwrite`, `context.rs:269-292`) — that pattern is exactly right and is the model a git approval flow should follow (approve a *specific* old-oid→new-oid transition, verify it at redemption, fail loud if the ref moved), but nothing in the latch generalizes it.

**6. No expiry/renewal story for held approvals.** A `Latched` background job outlives its nonce: at T+60s the job is still tracked and still `Latched`, but the stored nonce is dead and there is no re-issue path (§1.6). The user's only options are `kill --discard %N` or re-running the command. For long-running approval loops (human in another timezone, model review queued behind a batch) this is a dead end.

**7. Missing observability.** `wait` keeps only the first of several latches (§1.7 item 10); there is no "list all pending approvals across all jobs" API (you must walk `JobManager::list` and filter). A ledger system wants a queryable pending set as a primitive.

**What *is* directly reusable when designing the ledger.** The good bones are real and worth keeping: the **typed control-plane field** discipline (`.latch` never folded into `.data`, survives redirects and `--json` — `result.rs:195-208`), the **exact-argv capture** at the dispatch seam (`kernel.rs:3321`) which makes machine replay exact where the human `hint` cannot be, the **single-chokepoint rule** (`Job::latch()` stamping `job_id`, `job.rs:223-232`) that made every consumer consistent, the **never-fall-through-to-destructive-on-error** rule enforced throughout the trash gate, the **loud-failure-over-silent-degradation** posture (construction-time errors for incompatible configs), and `classify_command`'s **compile-enforced anti-drift** design (`docs/devlog.md:1568-1585`) — which is the right template for keeping a git policy engine in sync with what git actually does.
